//! Hashtree blob exchange over FIPS endpoint bytes.
//!
//! FIPS owns peer discovery, signaling, and underlay transports. This crate
//! keeps the Hashtree side to verified blob request/response frames carried as
//! app-owned endpoint bytes.

use async_trait::async_trait;
use fips_core::config::{NostrDiscoveryPolicy, PeerAddress, RoutingMode, TransportInstances};
use fips_core::PeerIdentity;
use hashtree_core::{Hash, MemoryStore, Store, StoreError};
pub use hashtree_network::PubsubPublishStats;
use hashtree_network::{
    transport::{PeerLink, PeerLinkFactory, SignalingTransport, TransportError},
    MeshRouter, MeshRoutingConfig, MeshStoreCore, PoolSettings, PubsubDeliveryMode,
    PubsubSchedulerConfig, SignalingMessage, MESH_EVENT_POLICY,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

pub const DEFAULT_FIPS_DISCOVERY_SCOPE: &str = "hashtree-v1";
pub const DEFAULT_FIPS_REQUEST_TIMEOUT: Duration = Duration::from_millis(5_500);
pub const DEFAULT_FIPS_REQUEST_RETRY_INTERVAL: Duration = Duration::from_millis(750);
pub const DEFAULT_FIPS_REQUEST_MAX_ATTEMPTS: usize = 4;
pub const FIPS_RESPONSE_FRAGMENT_SIZE: usize = 1024;
pub const FIPS_APP_FRAGMENT_SIZE: usize = 768;
pub const MAX_HTL: u8 = 10;
pub const DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS: usize = 512;
const APP_MESSAGE_BROADCAST_CAPACITY: usize = 4096;

const MSG_TYPE_REQUEST: u8 = 0x00;
const MSG_TYPE_RESPONSE: u8 = 0x01;
const MSG_TYPE_APP: u8 = 0x7f;
const MAX_RESPONSE_FRAGMENTS: u32 = 16_384;
const FIPS_MESH_SIGNALING_TOPIC: &str = "hashtree/fips/mesh/signaling/v1";
const FIPS_MESH_DATA_TOPIC: &str = "hashtree/fips/mesh/data/v1";
const FIPS_MESH_PUMP_INTERVAL: Duration = Duration::from_millis(20);
const FIPS_MESH_HELLO_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FipsEndpointPacket {
    pub peer_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FipsRelayStatus {
    pub url: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FipsPeerStatus {
    pub npub: String,
    pub transport_addr: Option<String>,
    pub transport_type: Option<String>,
    pub srtt_ms: Option<u64>,
    pub packets_sent: u64,
    pub packets_recv: u64,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
}

#[derive(Debug, Error)]
pub enum FipsTransportError {
    #[error("endpoint failed: {0}")]
    Endpoint(String),
    #[error("endpoint send failed: {0}")]
    Send(String),
    #[error("wire decode failed: {0}")]
    Wire(String),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FipsAppMessage {
    pub peer_id: String,
    pub topic: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FipsPeerConfig {
    pub npub: String,
    pub udp_addresses: Vec<String>,
}

impl FipsPeerConfig {
    pub fn new(npub: impl Into<String>) -> Self {
        Self {
            npub: npub.into(),
            udp_addresses: Vec::new(),
        }
    }
}

#[async_trait]
pub trait FipsEndpointIo: Send + Sync {
    async fn send(&self, peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError>;
    async fn recv(&self) -> Option<FipsEndpointPacket>;
    async fn set_peer_configs(
        &self,
        peer_configs: Vec<FipsPeerConfig>,
    ) -> Result<(), FipsTransportError> {
        self.set_peer_ids(peer_configs.into_iter().map(|peer| peer.npub).collect())
            .await
    }
    async fn set_peer_ids(&self, _peer_ids: Vec<String>) -> Result<(), FipsTransportError> {
        Ok(())
    }
    async fn peer_ids(&self) -> Vec<String> {
        Vec::new()
    }
    async fn peer_statuses(&self) -> Vec<FipsPeerStatus> {
        Vec::new()
    }
    async fn relay_statuses(&self) -> Vec<FipsRelayStatus> {
        Vec::new()
    }
    async fn shutdown(&self) -> Result<(), FipsTransportError> {
        Ok(())
    }
    fn local_peer_id(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct FipsEndpointOptions {
    pub identity_nsec: String,
    pub discovery_scope: String,
    pub relays: Vec<String>,
    pub enable_udp: bool,
    pub enable_webrtc: bool,
    pub udp_bind_addr: Option<String>,
    pub udp_public: bool,
    pub udp_external_addr: Option<String>,
    pub webrtc_auto_connect: bool,
    pub webrtc_max_connections: usize,
    pub open_discovery_max_pending: usize,
    pub packet_channel_capacity: usize,
}

impl FipsEndpointOptions {
    pub fn new(identity_nsec: impl Into<String>) -> Self {
        Self {
            identity_nsec: identity_nsec.into(),
            discovery_scope: DEFAULT_FIPS_DISCOVERY_SCOPE.to_string(),
            relays: Vec::new(),
            enable_udp: true,
            enable_webrtc: true,
            udp_bind_addr: None,
            udp_public: false,
            udp_external_addr: None,
            webrtc_auto_connect: false,
            webrtc_max_connections: DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS,
            open_discovery_max_pending: 0,
            packet_channel_capacity: 1024,
        }
    }
}

pub struct BoundFipsEndpoint {
    pub endpoint: Arc<dyn FipsEndpointIo>,
    pub local_peer_id: String,
    pub discovery_scope: String,
}

pub async fn bind_fips_endpoint(
    options: FipsEndpointOptions,
) -> Result<BoundFipsEndpoint, FipsTransportError> {
    if !options.enable_udp && !options.enable_webrtc {
        return Err(FipsTransportError::Endpoint(
            "at least one FIPS transport must be enabled".to_string(),
        ));
    }

    let discovery_scope = if options.discovery_scope.trim().is_empty() {
        DEFAULT_FIPS_DISCOVERY_SCOPE.to_string()
    } else {
        options.discovery_scope.trim().to_string()
    };
    let packet_channel_capacity = options.packet_channel_capacity;
    let config = fips_endpoint_config(options, &discovery_scope);

    let endpoint = fips_core::FipsEndpoint::builder()
        .config(config)
        .discovery_scope(discovery_scope.clone())
        .without_system_tun()
        .packet_channel_capacity(packet_channel_capacity)
        .bind()
        .await
        .map_err(|err| FipsTransportError::Endpoint(err.to_string()))?;
    let local_peer_id = endpoint.npub().to_string();

    Ok(BoundFipsEndpoint {
        endpoint: Arc::new(endpoint),
        local_peer_id,
        discovery_scope,
    })
}

fn fips_endpoint_config(options: FipsEndpointOptions, discovery_scope: &str) -> fips_core::Config {
    let mut config = fips_core::Config::new();
    config.node.identity = fips_core::IdentityConfig {
        nsec: Some(options.identity_nsec),
        persistent: false,
    };
    config.node.routing.mode = RoutingMode::ReplyLearned;
    config.node.limits.max_peers = options.webrtc_max_connections.max(1);
    config.node.limits.max_links = options.webrtc_max_connections.saturating_mul(2).max(1);
    config.node.limits.max_connections = options.webrtc_max_connections.saturating_mul(2).max(1);
    config.node.limits.max_pending_inbound =
        options.webrtc_max_connections.saturating_mul(4).max(1);
    config.node.control.enabled = false;
    config.tun.enabled = false;
    config.dns.enabled = false;
    config.node.system_files_enabled = false;
    config.node.discovery.lan.scope = Some(discovery_scope.to_string());
    config.node.discovery.nostr.enabled = true;
    config.node.discovery.nostr.advertise = true;
    config.node.discovery.nostr.policy = if options.open_discovery_max_pending == 0 {
        NostrDiscoveryPolicy::ConfiguredOnly
    } else {
        NostrDiscoveryPolicy::Open
    };
    config.node.discovery.nostr.open_discovery_max_pending = options.open_discovery_max_pending;
    config.node.discovery.nostr.share_local_candidates = true;
    config.node.discovery.nostr.app = discovery_scope.to_string();
    if !options.relays.is_empty() {
        config.node.discovery.nostr.advert_relays = options.relays.clone();
        config.node.discovery.nostr.dm_relays = options.relays;
    }

    if options.enable_udp {
        config.transports.udp = TransportInstances::Single(fips_core::UdpConfig {
            bind_addr: Some(
                options
                    .udp_bind_addr
                    .filter(|addr| !addr.trim().is_empty())
                    .unwrap_or_else(|| "0.0.0.0:0".to_string()),
            ),
            advertise_on_nostr: Some(true),
            public: Some(options.udp_public),
            external_addr: options
                .udp_external_addr
                .filter(|addr| !addr.trim().is_empty()),
            outbound_only: Some(false),
            accept_connections: Some(true),
            ..Default::default()
        });
    }

    #[cfg(feature = "webrtc")]
    if options.enable_webrtc {
        config.transports.webrtc = TransportInstances::Single(fips_core::WebRtcConfig {
            advertise_on_nostr: Some(true),
            auto_connect: Some(options.webrtc_auto_connect),
            accept_connections: Some(true),
            max_connections: Some(options.webrtc_max_connections.max(1)),
            ..Default::default()
        });
    }
    #[cfg(not(feature = "webrtc"))]
    if options.enable_webrtc {
        tracing::warn!(
            "FIPS WebRTC transport requested but this binary was built without the webrtc feature"
        );
    }

    // Some shared bootstrap peers expose tcp:443 for UDP-hostile networks.
    // Binding stays disabled by default, so this is outbound-only.
    config.transports.tcp = TransportInstances::Single(Default::default());

    config
}

fn peer_address_from_configured_addr(raw: &str) -> Option<PeerAddress> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (transport, addr) = split_configured_transport_addr(trimmed);
    Some(PeerAddress::new(transport, addr))
}

fn split_configured_transport_addr(value: &str) -> (&str, &str) {
    let Some((transport, addr)) = value.split_once(':') else {
        return ("udp", value);
    };
    match transport.to_ascii_lowercase().as_str() {
        "udp" | "tcp" | "webrtc" | "tor" | "ethernet" | "ble" => (transport, addr),
        _ => ("udp", value),
    }
}

fn sanitize_peer_configs(
    local: Option<&str>,
    peers: Vec<FipsPeerConfig>,
    seen: &mut std::collections::HashSet<String>,
) -> Vec<FipsPeerConfig> {
    let mut out = Vec::new();
    for peer in peers {
        let npub = peer.npub.trim().to_string();
        if npub.is_empty() || Some(npub.as_str()) == local || !seen.insert(npub.clone()) {
            continue;
        }
        let udp_addresses = peer
            .udp_addresses
            .into_iter()
            .map(|addr| addr.trim().to_string())
            .filter(|addr| !addr.is_empty())
            .collect();
        out.push(FipsPeerConfig {
            npub,
            udp_addresses,
        });
    }
    out
}

#[async_trait]
impl FipsEndpointIo for fips_core::FipsEndpoint {
    async fn send(&self, peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError> {
        let peer = PeerIdentity::from_npub(peer_id)
            .map_err(|err| FipsTransportError::Send(err.to_string()))?;
        self.send_batch_to_peer(peer, vec![data])
            .await
            .map_err(|err| FipsTransportError::Send(err.to_string()))
    }

    async fn recv(&self) -> Option<FipsEndpointPacket> {
        loop {
            let mut messages = Vec::with_capacity(1);
            self.recv_batch_into(&mut messages, 1).await?;
            let message = messages.into_iter().next()?;
            let peer_id = message.source_peer.npub();
            if peer_id.is_empty() {
                continue;
            }
            return Some(FipsEndpointPacket {
                peer_id,
                data: message.data.into_vec(),
            });
        }
    }

    async fn peer_ids(&self) -> Vec<String> {
        match self.peers().await {
            Ok(peers) => peers.into_iter().map(|peer| peer.npub).collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn peer_statuses(&self) -> Vec<FipsPeerStatus> {
        match self.peers().await {
            Ok(peers) => peers
                .into_iter()
                .map(|peer| FipsPeerStatus {
                    npub: peer.npub,
                    transport_addr: peer.transport_addr,
                    transport_type: peer.transport_type,
                    srtt_ms: peer.srtt_ms,
                    packets_sent: peer.packets_sent,
                    packets_recv: peer.packets_recv,
                    bytes_sent: peer.bytes_sent,
                    bytes_recv: peer.bytes_recv,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn relay_statuses(&self) -> Vec<FipsRelayStatus> {
        match fips_core::FipsEndpoint::relay_statuses(self).await {
            Ok(statuses) => statuses
                .into_iter()
                .map(|status| FipsRelayStatus {
                    url: status.url,
                    status: status.status,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn shutdown(&self) -> Result<(), FipsTransportError> {
        fips_core::FipsEndpoint::shutdown(self)
            .await
            .map_err(|err| FipsTransportError::Endpoint(err.to_string()))
    }

    async fn set_peer_ids(&self, peer_ids: Vec<String>) -> Result<(), FipsTransportError> {
        self.set_peer_configs(
            peer_ids
                .into_iter()
                .map(FipsPeerConfig::new)
                .collect::<Vec<_>>(),
        )
        .await
    }

    async fn set_peer_configs(
        &self,
        peer_configs: Vec<FipsPeerConfig>,
    ) -> Result<(), FipsTransportError> {
        let peers: Vec<fips_core::config::PeerConfig> = peer_configs
            .into_iter()
            .map(|peer| fips_core::config::PeerConfig {
                npub: peer.npub,
                addresses: peer
                    .udp_addresses
                    .into_iter()
                    .filter_map(|addr| peer_address_from_configured_addr(&addr))
                    .collect(),
                ..Default::default()
            })
            .collect();
        let peer_count = peers.len();
        match self.update_peers(peers).await {
            Ok(outcome) => {
                tracing::info!(
                    peer_count,
                    added = outcome.added,
                    removed = outcome.removed,
                    updated = outcome.updated,
                    unchanged = outcome.unchanged,
                    "updated FIPS endpoint peer configs"
                );
                Ok(())
            }
            Err(err) => {
                tracing::warn!(
                    peer_count,
                    error = %err,
                    "failed to update FIPS endpoint peer configs"
                );
                Err(FipsTransportError::Endpoint(err.to_string()))
            }
        }
    }

    fn local_peer_id(&self) -> Option<String> {
        Some(self.npub().to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataRequest {
    #[serde(with = "serde_bytes")]
    h: Vec<u8>,
    #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
    htl: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataResponse {
    #[serde(with = "serde_bytes")]
    h: Vec<u8>,
    #[serde(with = "serde_bytes")]
    d: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    i: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppPacket {
    t: String,
    #[serde(with = "serde_bytes")]
    d: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    i: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u32>,
}

enum Message {
    Request(DataRequest),
    Response(DataResponse),
    App(AppPacket),
}

fn default_htl() -> u8 {
    MAX_HTL
}

fn is_max_htl(htl: &u8) -> bool {
    *htl == MAX_HTL
}

fn hash_key(hash: &Hash) -> String {
    hex::encode(hash)
}

fn bytes_to_hash(bytes: &[u8]) -> Option<Hash> {
    if bytes.len() != 32 {
        return None;
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(bytes);
    Some(hash)
}

fn verify_hash(data: &[u8], hash: &Hash) -> bool {
    let digest = Sha256::digest(data);
    digest.as_slice() == hash
}

fn remaining_until(deadline: tokio::time::Instant) -> Duration {
    deadline
        .checked_duration_since(tokio::time::Instant::now())
        .unwrap_or_default()
}

fn encode_request(hash: &Hash, htl: u8) -> Result<Vec<u8>, FipsTransportError> {
    let body = rmp_serde::to_vec_named(&DataRequest {
        h: hash.to_vec(),
        htl,
    })
    .map_err(|err| FipsTransportError::Wire(err.to_string()))?;
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(MSG_TYPE_REQUEST);
    out.extend(body);
    Ok(out)
}

fn encode_response(hash: &Hash, data: &[u8]) -> Result<Vec<u8>, FipsTransportError> {
    let body = rmp_serde::to_vec_named(&DataResponse {
        h: hash.to_vec(),
        d: data.to_vec(),
        i: None,
        n: None,
    })
    .map_err(|err| FipsTransportError::Wire(err.to_string()))?;
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(MSG_TYPE_RESPONSE);
    out.extend(body);
    Ok(out)
}

fn encode_fragment_response(
    hash: &Hash,
    data: &[u8],
    index: u32,
    total: u32,
) -> Result<Vec<u8>, FipsTransportError> {
    let body = rmp_serde::to_vec_named(&DataResponse {
        h: hash.to_vec(),
        d: data.to_vec(),
        i: Some(index),
        n: Some(total),
    })
    .map_err(|err| FipsTransportError::Wire(err.to_string()))?;
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(MSG_TYPE_RESPONSE);
    out.extend(body);
    Ok(out)
}

fn encode_app_message(topic: &str, data: &[u8]) -> Result<Vec<u8>, FipsTransportError> {
    encode_app_packet(topic, data, None, None, None)
}

fn encode_app_packet(
    topic: &str,
    data: &[u8],
    id: Option<String>,
    index: Option<u32>,
    total: Option<u32>,
) -> Result<Vec<u8>, FipsTransportError> {
    let topic = topic.trim();
    if topic.is_empty() {
        return Err(FipsTransportError::Wire(
            "application message topic is empty".to_string(),
        ));
    }
    let body = rmp_serde::to_vec_named(&AppPacket {
        t: topic.to_string(),
        d: data.to_vec(),
        id,
        i: index,
        n: total,
    })
    .map_err(|err| FipsTransportError::Wire(err.to_string()))?;
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(MSG_TYPE_APP);
    out.extend(body);
    Ok(out)
}

fn encode_app_messages(topic: &str, data: &[u8]) -> Result<Vec<Vec<u8>>, FipsTransportError> {
    if data.len() <= FIPS_APP_FRAGMENT_SIZE {
        return encode_app_message(topic, data).map(|packet| vec![packet]);
    }

    let total = data.len().div_ceil(FIPS_APP_FRAGMENT_SIZE);
    if total > MAX_RESPONSE_FRAGMENTS as usize {
        return Err(FipsTransportError::Wire(format!(
            "application message has too many fragments: {total}"
        )));
    }

    let mut hasher = Sha256::new();
    hasher.update(topic.trim().as_bytes());
    hasher.update([0]);
    hasher.update(data);
    let id = hex::encode(hasher.finalize());
    let mut out = Vec::with_capacity(total);
    for (index, chunk) in data.chunks(FIPS_APP_FRAGMENT_SIZE).enumerate() {
        out.push(encode_app_packet(
            topic,
            chunk,
            Some(id.clone()),
            Some(index as u32),
            Some(total as u32),
        )?);
    }
    Ok(out)
}

fn parse_message(data: &[u8]) -> Result<Option<Message>, FipsTransportError> {
    let Some((&kind, body)) = data.split_first() else {
        return Ok(None);
    };
    match kind {
        MSG_TYPE_REQUEST => rmp_serde::from_slice::<DataRequest>(body)
            .map(|req| Some(Message::Request(req)))
            .map_err(|err| FipsTransportError::Wire(err.to_string())),
        MSG_TYPE_RESPONSE => rmp_serde::from_slice::<DataResponse>(body)
            .map(|resp| Some(Message::Response(resp)))
            .map_err(|err| FipsTransportError::Wire(err.to_string())),
        MSG_TYPE_APP => rmp_serde::from_slice::<AppPacket>(body)
            .map(|packet| Some(Message::App(packet)))
            .map_err(|err| FipsTransportError::Wire(err.to_string())),
        _ => Ok(None),
    }
}

struct PendingRequest {
    resolve: oneshot::Sender<Option<Vec<u8>>>,
}

struct ResponseReassembly {
    total: u32,
    fragments: HashMap<u32, Vec<u8>>,
    received_bytes: usize,
}

struct AppReassembly {
    total: u32,
    fragments: HashMap<u32, Vec<u8>>,
    received_bytes: usize,
    topic: String,
}

pub struct HashtreeFipsTransport<S: Store + Send + Sync + 'static = MemoryStore> {
    endpoint: Arc<dyn FipsEndpointIo>,
    local_store: Arc<S>,
    peers: Arc<RwLock<Vec<String>>>,
    peer_filter_configured: Arc<RwLock<bool>>,
    unconfigured_app_message_topics: Vec<String>,
    pending: Arc<Mutex<HashMap<String, Vec<PendingRequest>>>>,
    response_fragments: Arc<Mutex<HashMap<String, ResponseReassembly>>>,
    app_fragments: Arc<Mutex<HashMap<String, AppReassembly>>>,
    app_messages: broadcast::Sender<FipsAppMessage>,
    request_timeout: Duration,
    request_retry_interval: Duration,
    request_max_attempts: usize,
    request_htl: u8,
    cache_responses: bool,
}

impl HashtreeFipsTransport<MemoryStore> {
    pub fn in_memory(endpoint: Arc<dyn FipsEndpointIo>) -> Self {
        Self::new(endpoint, Arc::new(MemoryStore::new()))
    }
}

impl<S: Store + Send + Sync + 'static> HashtreeFipsTransport<S> {
    pub fn new(endpoint: Arc<dyn FipsEndpointIo>, local_store: Arc<S>) -> Self {
        let (app_messages, _) = broadcast::channel(APP_MESSAGE_BROADCAST_CAPACITY);
        Self {
            endpoint,
            local_store,
            peers: Arc::new(RwLock::new(Vec::new())),
            peer_filter_configured: Arc::new(RwLock::new(false)),
            unconfigured_app_message_topics: Vec::new(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            response_fragments: Arc::new(Mutex::new(HashMap::new())),
            app_fragments: Arc::new(Mutex::new(HashMap::new())),
            app_messages,
            request_timeout: DEFAULT_FIPS_REQUEST_TIMEOUT,
            request_retry_interval: DEFAULT_FIPS_REQUEST_RETRY_INTERVAL,
            request_max_attempts: DEFAULT_FIPS_REQUEST_MAX_ATTEMPTS,
            request_htl: MAX_HTL,
            cache_responses: true,
        }
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn with_request_retry_interval(mut self, interval: Duration) -> Self {
        self.request_retry_interval = if interval.is_zero() {
            Duration::from_millis(1)
        } else {
            interval
        };
        self
    }

    pub fn with_request_max_attempts(mut self, attempts: usize) -> Self {
        self.request_max_attempts = attempts.max(1);
        self
    }

    pub fn with_request_htl(mut self, htl: u8) -> Self {
        self.request_htl = htl;
        self
    }

    pub fn with_cache_responses(mut self, cache_responses: bool) -> Self {
        self.cache_responses = cache_responses;
        self
    }

    pub fn with_unconfigured_app_message_topics<T, I>(mut self, topics: I) -> Self
    where
        T: Into<String>,
        I: IntoIterator<Item = T>,
    {
        self.unconfigured_app_message_topics = topics.into_iter().map(Into::into).collect();
        self
    }

    pub async fn set_peers(&self, peers: Vec<String>) {
        self.set_peer_configs(peers.into_iter().map(FipsPeerConfig::new).collect())
            .await;
    }

    pub async fn set_peer_configs(&self, peers: Vec<FipsPeerConfig>) {
        self.set_peer_configs_with_routing_peers(peers, Vec::new())
            .await;
    }

    pub async fn set_peer_configs_with_routing_peers(
        &self,
        application_peers: Vec<FipsPeerConfig>,
        routing_peers: Vec<FipsPeerConfig>,
    ) {
        let local = self.endpoint.local_peer_id();
        let mut seen = std::collections::HashSet::new();
        let app_out = sanitize_peer_configs(local.as_deref(), application_peers, &mut seen);
        let routing_out = sanitize_peer_configs(local.as_deref(), routing_peers, &mut seen);
        let mut endpoint_out = app_out.clone();
        endpoint_out.extend(routing_out.clone());
        let configured_count = endpoint_out.len();
        let application_count = app_out.len();
        let routing_only_count = routing_out.len();
        let udp_hint_count: usize = endpoint_out
            .iter()
            .map(|peer| peer.udp_addresses.len())
            .sum();
        match self.endpoint.set_peer_configs(endpoint_out.clone()).await {
            Ok(()) => {
                tracing::info!(
                    configured_count,
                    application_count,
                    routing_only_count,
                    udp_hint_count,
                    "configured Hashtree FIPS peers"
                );
                *self.peers.write().await = app_out.into_iter().map(|peer| peer.npub).collect();
                *self.peer_filter_configured.write().await = true;
            }
            Err(error) => {
                tracing::warn!(
                    configured_count,
                    application_count,
                    routing_only_count,
                    udp_hint_count,
                    error = %error,
                    "failed to configure Hashtree FIPS peers"
                );
            }
        }
    }

    pub async fn peer_ids(&self) -> Vec<String> {
        let configured = self.peers.read().await.clone();
        if configured.is_empty() && !*self.peer_filter_configured.read().await {
            self.endpoint.peer_ids().await
        } else {
            configured
        }
    }

    pub async fn configured_peer_ids(&self) -> Vec<String> {
        self.peers.read().await.clone()
    }

    pub async fn connected_peer_ids(&self) -> Vec<String> {
        self.endpoint.peer_ids().await
    }

    pub async fn peer_statuses(&self) -> Vec<FipsPeerStatus> {
        self.endpoint.peer_statuses().await
    }

    pub async fn relay_statuses(&self) -> Vec<FipsRelayStatus> {
        self.endpoint.relay_statuses().await
    }

    pub fn subscribe_app_messages(&self) -> broadcast::Receiver<FipsAppMessage> {
        self.app_messages.subscribe()
    }

    pub async fn send_app_message(
        &self,
        peer_id: &str,
        topic: &str,
        data: Vec<u8>,
    ) -> Result<(), FipsTransportError> {
        for packet in encode_app_messages(topic, &data)? {
            self.endpoint.send(peer_id, packet).await?;
        }
        Ok(())
    }

    pub async fn broadcast_app_message(
        &self,
        topic: &str,
        data: Vec<u8>,
    ) -> Result<usize, FipsTransportError> {
        let packets = encode_app_messages(topic, &data)?;
        let mut sent = 0usize;
        for peer in self.peer_ids().await {
            let mut peer_sent = true;
            for packet in &packets {
                if self.endpoint.send(&peer, packet.clone()).await.is_err() {
                    peer_sent = false;
                    break;
                }
            }
            if peer_sent {
                sent += 1;
            }
        }
        Ok(sent)
    }

    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(packet) = this.endpoint.recv().await {
                let _ = this.handle_packet(packet).await;
            }
        })
    }

    pub async fn shutdown(&self) -> Result<(), FipsTransportError> {
        self.endpoint.shutdown().await
    }

    pub async fn get_from_peers(
        &self,
        hash: &Hash,
        peers: &[String],
    ) -> Result<Option<Vec<u8>>, FipsTransportError> {
        if let Some(data) = self.local_store.get(hash).await? {
            if verify_hash(&data, hash) {
                return Ok(Some(data));
            }
        }
        if peers.is_empty() {
            return Ok(None);
        }

        let key = hash_key(hash);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .push(PendingRequest { resolve: tx });

        let payload = encode_request(hash, self.request_htl)?;
        let deadline = tokio::time::Instant::now() + self.request_timeout;
        let mut rx = rx;
        for attempt in 0..self.request_max_attempts {
            if attempt > 0 && remaining_until(deadline).is_zero() {
                break;
            }
            let sent = self.send_request_to_peers(peers, &payload).await;
            if sent == 0 && attempt == 0 {
                self.resolve_pending(&key, None).await;
                return Ok(None);
            }
            if attempt + 1 >= self.request_max_attempts {
                break;
            }
            let remaining = remaining_until(deadline);
            if remaining.is_zero() {
                break;
            }
            match timeout(self.request_retry_interval.min(remaining), &mut rx).await {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(_)) => return Ok(None),
                Err(_) => {
                    if remaining_until(deadline).is_zero() {
                        break;
                    }
                }
            }
        }

        match timeout(remaining_until(deadline), rx).await {
            Ok(Ok(result)) => Ok(result),
            _ => {
                self.remove_pending_sender(&key).await;
                Ok(None)
            }
        }
    }

    async fn send_request_to_peers(&self, peers: &[String], payload: &[u8]) -> usize {
        let mut sent = 0usize;
        for peer in peers {
            if self.endpoint.send(peer, payload.to_vec()).await.is_ok() {
                sent += 1;
            }
        }
        sent
    }

    async fn handle_packet(&self, packet: FipsEndpointPacket) -> Result<(), FipsTransportError> {
        let Some(message) = parse_message(&packet.data)? else {
            return Ok(());
        };
        let unconfigured_app_message_allowed = match &message {
            Message::App(app) => self
                .unconfigured_app_message_topics
                .iter()
                .any(|topic| topic == &app.t),
            _ => false,
        };
        let is_application_peer = self.is_application_peer(&packet.peer_id).await;
        if !is_application_peer && !unconfigured_app_message_allowed {
            return Ok(());
        }
        match message {
            Message::Request(req) => {
                let Some(hash) = bytes_to_hash(&req.h) else {
                    return Ok(());
                };
                let Some(data) = self.local_store.get(&hash).await? else {
                    return Ok(());
                };
                if !verify_hash(&data, &hash) {
                    return Ok(());
                }
                self.send_response(&packet.peer_id, &hash, &data).await?;
            }
            Message::Response(resp) => {
                let Some(hash) = bytes_to_hash(&resp.h) else {
                    return Ok(());
                };
                let key = hash_key(&hash);
                if !self.pending.lock().await.contains_key(&key) {
                    return Ok(());
                }
                let Some(data) = self.response_data_from_message(&key, resp).await else {
                    return Ok(());
                };
                if !verify_hash(&data, &hash) {
                    return Ok(());
                }
                if self.cache_responses {
                    let _ = self.local_store.put(hash, data.clone()).await;
                }
                self.resolve_pending(&key, Some(data)).await;
            }
            Message::App(app) => {
                if app.t.trim().is_empty() {
                    return Ok(());
                }
                if let Some((topic, data)) = self.app_data_from_message(&packet.peer_id, app).await
                {
                    let _ = self.app_messages.send(FipsAppMessage {
                        peer_id: packet.peer_id,
                        topic,
                        data,
                    });
                }
            }
        }
        Ok(())
    }

    async fn is_application_peer(&self, peer_id: &str) -> bool {
        let peers = self.peers.read().await;
        if peers.is_empty() && !*self.peer_filter_configured.read().await {
            return true;
        }
        peers.iter().any(|peer| peer == peer_id)
    }

    async fn send_response(
        &self,
        peer_id: &str,
        hash: &Hash,
        data: &[u8],
    ) -> Result<(), FipsTransportError> {
        if data.len() <= FIPS_RESPONSE_FRAGMENT_SIZE {
            self.endpoint
                .send(peer_id, encode_response(hash, data)?)
                .await?;
            return Ok(());
        }

        let total = data.len().div_ceil(FIPS_RESPONSE_FRAGMENT_SIZE) as u32;
        for index in 0..total {
            let start = index as usize * FIPS_RESPONSE_FRAGMENT_SIZE;
            let end = (start + FIPS_RESPONSE_FRAGMENT_SIZE).min(data.len());
            self.endpoint
                .send(
                    peer_id,
                    encode_fragment_response(hash, &data[start..end], index, total)?,
                )
                .await?;
        }
        Ok(())
    }

    async fn response_data_from_message(&self, key: &str, resp: DataResponse) -> Option<Vec<u8>> {
        match (resp.i, resp.n) {
            (Some(index), Some(total)) => {
                self.reassemble_response_fragment(key, resp.d, index, total)
                    .await
            }
            (None, None) => Some(resp.d),
            _ => None,
        }
    }

    async fn reassemble_response_fragment(
        &self,
        key: &str,
        data: Vec<u8>,
        index: u32,
        total: u32,
    ) -> Option<Vec<u8>> {
        if total == 0 || total > MAX_RESPONSE_FRAGMENTS || index >= total {
            return None;
        }

        let mut fragments = self.response_fragments.lock().await;
        let entry = fragments
            .entry(key.to_string())
            .or_insert_with(|| ResponseReassembly {
                total,
                fragments: HashMap::new(),
                received_bytes: 0,
            });
        if entry.total != total {
            *entry = ResponseReassembly {
                total,
                fragments: HashMap::new(),
                received_bytes: 0,
            };
        }

        if let std::collections::hash_map::Entry::Vacant(slot) = entry.fragments.entry(index) {
            entry.received_bytes += data.len();
            slot.insert(data);
        }

        if entry.fragments.len() != entry.total as usize {
            return None;
        }

        let mut assembled = Vec::with_capacity(entry.received_bytes);
        for fragment_index in 0..entry.total {
            let fragment = entry.fragments.get(&fragment_index)?;
            assembled.extend_from_slice(fragment);
        }
        fragments.remove(key);
        Some(assembled)
    }

    async fn app_data_from_message(
        &self,
        peer_id: &str,
        app: AppPacket,
    ) -> Option<(String, Vec<u8>)> {
        match (app.id, app.i, app.n) {
            (None, None, None) => Some((app.t, app.d)),
            (Some(id), Some(index), Some(total)) => {
                self.reassemble_app_fragment(peer_id, app.t, id, app.d, index, total)
                    .await
            }
            _ => None,
        }
    }

    async fn reassemble_app_fragment(
        &self,
        peer_id: &str,
        topic: String,
        id: String,
        data: Vec<u8>,
        index: u32,
        total: u32,
    ) -> Option<(String, Vec<u8>)> {
        if total == 0 || total > MAX_RESPONSE_FRAGMENTS || index >= total {
            return None;
        }

        let key = format!("{peer_id}\0{topic}\0{id}");
        let mut fragments = self.app_fragments.lock().await;
        let entry = fragments
            .entry(key.clone())
            .or_insert_with(|| AppReassembly {
                total,
                fragments: HashMap::new(),
                received_bytes: 0,
                topic: topic.clone(),
            });
        if entry.total != total {
            *entry = AppReassembly {
                total,
                fragments: HashMap::new(),
                received_bytes: 0,
                topic: topic.clone(),
            };
        }

        if let std::collections::hash_map::Entry::Vacant(slot) = entry.fragments.entry(index) {
            entry.received_bytes += data.len();
            slot.insert(data);
        }

        if entry.fragments.len() != entry.total as usize {
            return None;
        }

        let mut assembled = Vec::with_capacity(entry.received_bytes);
        for fragment_index in 0..entry.total {
            let fragment = entry.fragments.get(&fragment_index)?;
            assembled.extend_from_slice(fragment);
        }
        let topic = entry.topic.clone();
        fragments.remove(&key);
        Some((topic, assembled))
    }

    async fn resolve_pending(&self, key: &str, data: Option<Vec<u8>>) {
        self.response_fragments.lock().await.remove(key);
        let pending = self.pending.lock().await.remove(key);
        if let Some(pending) = pending {
            for request in pending {
                let _ = request.resolve.send(data.clone());
            }
        }
    }

    async fn remove_pending_sender(&self, key: &str) {
        let remove_fragments = {
            let mut pending = self.pending.lock().await;
            if let Some(requests) = pending.get_mut(key) {
                requests.retain(|request| !request.resolve.is_closed());
                if requests.is_empty() {
                    pending.remove(key);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if remove_fragments {
            self.response_fragments.lock().await.remove(key);
        }
    }
}

type FipsMeshStore<S> = MeshStoreCore<S, FipsMeshSignaling<S>, FipsMeshLinkFactory<S>>;

/// Local pubsub payload delivered by the hashtree mesh core over FIPS links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FipsMeshPubsubEvent {
    pub stream_id: String,
    pub seq: u64,
    pub origin_peer_id: String,
    pub from_peer_id: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FipsMeshPubsubOptions {
    pub forwarding: bool,
    pub fanout: usize,
    pub max_hops: u8,
}

impl Default for FipsMeshPubsubOptions {
    fn default() -> Self {
        Self {
            forwarding: true,
            fanout: PubsubSchedulerConfig::default().fanout,
            max_hops: MESH_EVENT_POLICY.max_htl,
        }
    }
}

impl FipsMeshPubsubOptions {
    fn routing_config(&self) -> MeshRoutingConfig {
        let mut pubsub_scheduler = PubsubSchedulerConfig::default();
        pubsub_scheduler.fanout = self.fanout;
        MeshRoutingConfig {
            pubsub_delivery_mode: PubsubDeliveryMode::HtlInvWant,
            pubsub_scheduler,
            pubsub_forwarding: self.forwarding,
            pubsub_max_htl: self.max_hops,
            ..Default::default()
        }
    }
}

/// Hashtree mesh pubsub runtime backed by FIPS endpoint bytes.
pub struct FipsMeshPubsub<S: Store + Send + Sync + 'static> {
    store: Arc<FipsMeshStore<S>>,
    demux_task: JoinHandle<()>,
    pump_task: JoinHandle<()>,
}

impl<S: Store + Send + Sync + 'static> FipsMeshPubsub<S> {
    /// Stop background tasks owned by this FIPS-backed pubsub runtime.
    pub fn shutdown(&self) {
        self.demux_task.abort();
        self.pump_task.abort();
    }

    /// Subscribe this node to a mesh pubsub stream.
    pub async fn subscribe_pubsub(&self, stream_id: impl Into<String>) -> PubsubPublishStats {
        self.store.subscribe_pubsub(stream_id.into()).await
    }

    /// Stop local delivery for a mesh pubsub stream and withdraw advertised interest.
    pub async fn unsubscribe_pubsub(&self, stream_id: impl Into<String>) -> PubsubPublishStats {
        self.store.unsubscribe_pubsub(stream_id.into()).await
    }

    /// Publish bytes on a mesh pubsub stream with the given origin-local sequence.
    pub async fn publish_pubsub(
        &self,
        stream_id: impl Into<String>,
        seq: u64,
        payload: Vec<u8>,
    ) -> PubsubPublishStats {
        self.store
            .publish_pubsub(stream_id.into(), seq, payload)
            .await
    }

    /// Drain pubsub events delivered to this node.
    pub async fn drain_pubsub_events(&self) -> Vec<FipsMeshPubsubEvent> {
        self.store
            .drain_pubsub_events()
            .await
            .into_iter()
            .map(|event| FipsMeshPubsubEvent {
                stream_id: event.stream_id,
                seq: event.seq,
                origin_peer_id: event.origin_peer_id,
                from_peer_id: event.from_peer_id,
                payload: event.payload,
            })
            .collect()
    }

    /// Wait for the next pubsub event delivered to this node.
    pub async fn recv_pubsub_event(&self) -> FipsMeshPubsubEvent {
        let event = self.store.recv_pubsub_event().await;
        FipsMeshPubsubEvent {
            stream_id: event.stream_id,
            seq: event.seq,
            origin_peer_id: event.origin_peer_id,
            from_peer_id: event.from_peer_id,
            payload: event.payload,
        }
    }

    /// Current mesh peer count known to the shared mesh core.
    pub async fn peer_count(&self) -> usize {
        self.store.peer_count().await
    }

    /// Current mesh peer IDs known to the shared mesh core.
    pub async fn peer_ids(&self) -> Vec<String> {
        self.store.peer_ids().await
    }
}

#[async_trait]
impl<S: Store + Send + Sync + 'static> Store for FipsMeshPubsub<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.store.get(hash).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.store.delete(hash).await
    }
}

impl<S: Store + Send + Sync + 'static> Drop for FipsMeshPubsub<S> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl<S: Store + Send + Sync + 'static> HashtreeFipsTransport<S> {
    /// Start a hashtree mesh pubsub runtime on top of this FIPS transport.
    ///
    /// FIPS remains responsible for discovery, authorization, and endpoint
    /// delivery. The shared mesh core handles pubsub interest floods,
    /// inventory/want routing, frame dedupe, and fanout scheduling.
    pub async fn start_mesh_pubsub(
        self: &Arc<Self>,
        local_store: Arc<S>,
        peer_id: String,
        request_timeout: Duration,
    ) -> Result<FipsMeshPubsub<S>, FipsTransportError> {
        self.start_mesh_pubsub_with_options(
            local_store,
            peer_id,
            request_timeout,
            FipsMeshPubsubOptions::default(),
        )
        .await
    }

    /// Start a hashtree mesh pubsub runtime with explicit forwarding/fanout/hop options.
    pub async fn start_mesh_pubsub_with_options(
        self: &Arc<Self>,
        local_store: Arc<S>,
        peer_id: String,
        request_timeout: Duration,
        options: FipsMeshPubsubOptions,
    ) -> Result<FipsMeshPubsub<S>, FipsTransportError> {
        let hub = FipsMeshLinkHub::default();
        let (signaling_tx, signaling_rx) = mpsc::unbounded_channel();
        let signaling_transport = Arc::new(FipsMeshSignaling {
            peer_id: peer_id.clone(),
            transport: self.clone(),
            rx: Mutex::new(signaling_rx),
            connected: AtomicBool::new(true),
        });
        let link_factory = Arc::new(FipsMeshLinkFactory {
            transport: self.clone(),
            hub: hub.clone(),
        });
        let router = Arc::new(MeshRouter::new(
            peer_id.clone(),
            signaling_transport.clone(),
            link_factory,
            PoolSettings::default(),
            false,
        ));
        let store = Arc::new(MeshStoreCore::new_with_routing(
            local_store,
            router,
            request_timeout,
            false,
            options.routing_config(),
        ));
        store
            .start()
            .await
            .map_err(|error| FipsTransportError::Endpoint(error.to_string()))?;

        let demux_task = spawn_fips_mesh_demux(self.clone(), peer_id, hub, signaling_tx);
        let pump_task = spawn_fips_mesh_pump(store.clone(), signaling_transport);
        Ok(FipsMeshPubsub {
            store,
            demux_task,
            pump_task,
        })
    }
}

#[derive(Default, Clone)]
struct FipsMeshLinkHub {
    inboxes: Arc<Mutex<HashMap<String, Arc<FipsMeshInbox>>>>,
}

impl FipsMeshLinkHub {
    async fn inbox(&self, peer_id: &str) -> Arc<FipsMeshInbox> {
        let mut inboxes = self.inboxes.lock().await;
        inboxes
            .entry(peer_id.to_string())
            .or_insert_with(|| Arc::new(FipsMeshInbox::default()))
            .clone()
    }

    async fn push(&self, peer_id: &str, data: Vec<u8>) {
        self.inbox(peer_id).await.push(data).await;
    }
}

struct FipsMeshInbox {
    queue: Mutex<VecDeque<Vec<u8>>>,
    notify: Notify,
    open: AtomicBool,
}

impl Default for FipsMeshInbox {
    fn default() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            open: AtomicBool::new(true),
        }
    }
}

impl FipsMeshInbox {
    async fn push(&self, data: Vec<u8>) {
        self.queue.lock().await.push_back(data);
        self.notify.notify_waiters();
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        loop {
            if let Some(data) = self.queue.lock().await.pop_front() {
                return Some(data);
            }
            if !self.open.load(Ordering::Relaxed) {
                return None;
            }
            self.notify.notified().await;
        }
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        let Ok(mut queue) = self.queue.try_lock() else {
            return None;
        };
        queue.pop_front()
    }

    fn close(&self) {
        self.open.store(false, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

struct FipsMeshSignaling<S: Store + Send + Sync + 'static> {
    peer_id: String,
    transport: Arc<HashtreeFipsTransport<S>>,
    rx: Mutex<mpsc::UnboundedReceiver<SignalingMessage>>,
    connected: AtomicBool,
}

#[async_trait]
impl<S: Store + Send + Sync + 'static> SignalingTransport for FipsMeshSignaling<S> {
    async fn connect(&self, _relays: &[String]) -> Result<(), TransportError> {
        self.connected.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    async fn publish(&self, msg: SignalingMessage) -> Result<(), TransportError> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err(TransportError::NotConnected);
        }
        let payload = serde_json::to_vec(&msg)
            .map_err(|error| TransportError::SendFailed(error.to_string()))?;
        if let Some(peer_id) = msg.target_peer_id() {
            self.transport
                .send_app_message(peer_id, FIPS_MESH_SIGNALING_TOPIC, payload)
                .await
                .map_err(|error| TransportError::SendFailed(error.to_string()))
        } else {
            self.transport
                .broadcast_app_message(FIPS_MESH_SIGNALING_TOPIC, payload)
                .await
                .map(|_| ())
                .map_err(|error| TransportError::SendFailed(error.to_string()))
        }
    }

    async fn recv(&self) -> Option<SignalingMessage> {
        self.rx.lock().await.recv().await
    }

    fn try_recv(&self) -> Option<SignalingMessage> {
        let Ok(mut rx) = self.rx.try_lock() else {
            return None;
        };
        rx.try_recv().ok()
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

struct FipsMeshLinkFactory<S: Store + Send + Sync + 'static> {
    transport: Arc<HashtreeFipsTransport<S>>,
    hub: FipsMeshLinkHub,
}

impl<S: Store + Send + Sync + 'static> FipsMeshLinkFactory<S> {
    async fn link_for(&self, peer_id: &str) -> Arc<dyn PeerLink> {
        Arc::new(FipsMeshPeerLink {
            peer_id: peer_id.to_string(),
            transport: self.transport.clone(),
            inbox: self.hub.inbox(peer_id).await,
        })
    }
}

#[async_trait]
impl<S: Store + Send + Sync + 'static> PeerLinkFactory for FipsMeshLinkFactory<S> {
    async fn create_offer(
        &self,
        target_peer_id: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
        Ok((self.link_for(target_peer_id).await, "fips-mesh-v1".into()))
    }

    async fn accept_offer(
        &self,
        from_peer_id: &str,
        _offer_sdp: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
        Ok((self.link_for(from_peer_id).await, "fips-mesh-v1".into()))
    }

    async fn handle_answer(
        &self,
        target_peer_id: &str,
        _answer_sdp: &str,
    ) -> Result<Arc<dyn PeerLink>, TransportError> {
        Ok(self.link_for(target_peer_id).await)
    }
}

struct FipsMeshPeerLink<S: Store + Send + Sync + 'static> {
    peer_id: String,
    transport: Arc<HashtreeFipsTransport<S>>,
    inbox: Arc<FipsMeshInbox>,
}

#[async_trait]
impl<S: Store + Send + Sync + 'static> PeerLink for FipsMeshPeerLink<S> {
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError> {
        self.transport
            .send_app_message(&self.peer_id, FIPS_MESH_DATA_TOPIC, data)
            .await
            .map_err(|error| TransportError::SendFailed(error.to_string()))
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        self.inbox.recv().await
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        self.inbox.try_recv()
    }

    fn is_open(&self) -> bool {
        self.inbox.open.load(Ordering::Relaxed)
    }

    async fn close(&self) {
        self.inbox.close();
    }
}

fn spawn_fips_mesh_demux<S: Store + Send + Sync + 'static>(
    transport: Arc<HashtreeFipsTransport<S>>,
    peer_id: String,
    hub: FipsMeshLinkHub,
    signaling_tx: mpsc::UnboundedSender<SignalingMessage>,
) -> JoinHandle<()> {
    let mut app_messages = transport.subscribe_app_messages();
    tokio::spawn(async move {
        loop {
            let Ok(message) = app_messages.recv().await else {
                break;
            };
            match message.topic.as_str() {
                FIPS_MESH_SIGNALING_TOPIC => {
                    let Ok(signal) = serde_json::from_slice::<SignalingMessage>(&message.data)
                    else {
                        continue;
                    };
                    if signal.peer_id() != peer_id
                        && (signal.target_peer_id().is_none()
                            || signal.target_peer_id() == Some(peer_id.as_str()))
                    {
                        let _ = signaling_tx.send(signal);
                    }
                }
                FIPS_MESH_DATA_TOPIC => {
                    hub.push(&message.peer_id, message.data).await;
                }
                _ => {}
            }
        }
    })
}

fn spawn_fips_mesh_pump<S: Store + Send + Sync + 'static>(
    store: Arc<FipsMeshStore<S>>,
    signaling: Arc<FipsMeshSignaling<S>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut pump = tokio::time::interval(FIPS_MESH_PUMP_INTERVAL);
        pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut hello = tokio::time::interval(FIPS_MESH_HELLO_INTERVAL);
        hello.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                signal = signaling.recv() => {
                    let Some(signal) = signal else {
                        break;
                    };
                    let _ = store.process_signaling(signal).await;
                }
                _ = pump.tick() => {
                    store.drain_available_data_messages().await;
                }
                _ = hello.tick() => {
                    let _ = store.send_hello().await;
                }
            }
        }
    })
}

#[async_trait]
impl<S: Store + Send + Sync + 'static> Store for HashtreeFipsTransport<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local_store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.local_store.get(hash).await? {
            if verify_hash(&data, hash) {
                return Ok(Some(data));
            }
        }
        let peers = self.peer_ids().await;
        self.get_from_peers(hash, &peers)
            .await
            .map_err(|err| StoreError::Other(err.to_string()))
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.delete(hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct FakeEndpoint {
        id: String,
        network: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<FipsEndpointPacket>>>>,
        rx: Mutex<mpsc::UnboundedReceiver<FipsEndpointPacket>>,
        configured_peers: Mutex<Vec<String>>,
        configured_peer_configs: Mutex<Vec<FipsPeerConfig>>,
        peer_statuses: Mutex<Vec<FipsPeerStatus>>,
        sent: AtomicUsize,
        drop_next: AtomicUsize,
        shutdown_count: AtomicUsize,
    }

    impl FakeEndpoint {
        async fn new(
            id: &str,
            network: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<FipsEndpointPacket>>>>,
        ) -> Arc<Self> {
            let (tx, rx) = mpsc::unbounded_channel();
            network.lock().await.insert(id.to_string(), tx);
            Arc::new(Self {
                id: id.to_string(),
                network,
                rx: Mutex::new(rx),
                configured_peers: Mutex::new(Vec::new()),
                configured_peer_configs: Mutex::new(Vec::new()),
                peer_statuses: Mutex::new(Vec::new()),
                sent: AtomicUsize::new(0),
                drop_next: AtomicUsize::new(0),
                shutdown_count: AtomicUsize::new(0),
            })
        }

        fn sent_count(&self) -> usize {
            self.sent.load(Ordering::Relaxed)
        }

        fn drop_next_sends(&self, count: usize) {
            self.drop_next.store(count, Ordering::Relaxed);
        }

        fn shutdown_count(&self) -> usize {
            self.shutdown_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl FipsEndpointIo for FakeEndpoint {
        async fn send(&self, peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError> {
            self.sent.fetch_add(1, Ordering::Relaxed);
            if self
                .drop_next
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    if value > 0 {
                        Some(value - 1)
                    } else {
                        None
                    }
                })
                .is_ok()
            {
                return Ok(());
            }
            let tx = self
                .network
                .lock()
                .await
                .get(peer_id)
                .cloned()
                .ok_or_else(|| FipsTransportError::Send(format!("unknown peer {peer_id}")))?;
            tx.send(FipsEndpointPacket {
                peer_id: self.id.clone(),
                data,
            })
            .map_err(|_| FipsTransportError::Send("receiver closed".to_string()))
        }

        async fn recv(&self) -> Option<FipsEndpointPacket> {
            self.rx.lock().await.recv().await
        }

        async fn set_peer_ids(&self, peer_ids: Vec<String>) -> Result<(), FipsTransportError> {
            *self.configured_peers.lock().await = peer_ids;
            Ok(())
        }

        async fn set_peer_configs(
            &self,
            peer_configs: Vec<FipsPeerConfig>,
        ) -> Result<(), FipsTransportError> {
            *self.configured_peers.lock().await =
                peer_configs.iter().map(|peer| peer.npub.clone()).collect();
            *self.configured_peer_configs.lock().await = peer_configs;
            Ok(())
        }

        async fn peer_ids(&self) -> Vec<String> {
            let configured = self.configured_peers.lock().await.clone();
            if !configured.is_empty() {
                return configured;
            }
            self.network
                .lock()
                .await
                .keys()
                .filter(|id| *id != &self.id)
                .cloned()
                .collect()
        }

        async fn peer_statuses(&self) -> Vec<FipsPeerStatus> {
            self.peer_statuses.lock().await.clone()
        }

        async fn shutdown(&self) -> Result<(), FipsTransportError> {
            self.shutdown_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn local_peer_id(&self) -> Option<String> {
            Some(self.id.clone())
        }
    }

    fn hash(data: &[u8]) -> Hash {
        let digest = Sha256::digest(data);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        hash
    }

    #[test]
    fn endpoint_config_uses_reply_learned_routing_for_mesh_fallback() {
        let config = fips_endpoint_config(FipsEndpointOptions::new("nsec1example"), "test-scope");

        assert_eq!(config.node.routing.mode, RoutingMode::ReplyLearned);
    }

    #[test]
    fn endpoint_config_scopes_nostr_discovery_app() {
        let config = fips_endpoint_config(
            FipsEndpointOptions::new("nsec1example"),
            "iris-drive-v1:private-owner",
        );

        assert_eq!(
            config.node.discovery.nostr.app,
            "iris-drive-v1:private-owner"
        );
    }

    #[test]
    fn endpoint_config_disables_control_socket_for_embedded_clients() {
        let config = fips_endpoint_config(FipsEndpointOptions::new("nsec1example"), "test-scope");

        assert!(!config.node.control.enabled);
    }

    #[test]
    fn endpoint_config_caps_total_peer_fanout() {
        let mut options = FipsEndpointOptions::new("nsec1example");
        options.webrtc_max_connections = 9;
        let config = fips_endpoint_config(options, "test-scope");

        assert_eq!(config.node.limits.max_peers, 9);
        assert_eq!(config.node.limits.max_links, 18);
        assert_eq!(config.node.limits.max_connections, 18);
        assert_eq!(config.node.limits.max_pending_inbound, 36);
    }

    #[test]
    fn transport_tagged_peer_addresses_are_preserved_for_fips() {
        let udp = peer_address_from_configured_addr(" udp:10.44.1.2:22121 ").unwrap();
        let tcp = peer_address_from_configured_addr("tcp:203.0.113.9:443").unwrap();
        let bare = peer_address_from_configured_addr("10.44.1.3:22121").unwrap();

        assert_eq!(udp.transport, "udp");
        assert_eq!(udp.addr, "10.44.1.2:22121");
        assert_eq!(tcp.transport, "tcp");
        assert_eq!(tcp.addr, "203.0.113.9:443");
        assert_eq!(bare.transport, "udp");
        assert_eq!(bare.addr, "10.44.1.3:22121");
    }

    #[tokio::test]
    async fn set_peers_configures_underlying_fips_endpoint() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = FakeEndpoint::new("local", network).await;
        let transport = HashtreeFipsTransport::new(endpoint.clone(), Arc::new(MemoryStore::new()));

        transport
            .set_peers(vec![
                "remote".to_string(),
                "local".to_string(),
                "remote".to_string(),
                "  ".to_string(),
            ])
            .await;

        assert_eq!(
            endpoint.configured_peers.lock().await.as_slice(),
            &["remote".to_string()]
        );
        assert_eq!(transport.configured_peer_ids().await, vec!["remote"]);
    }

    #[tokio::test]
    async fn shutdown_delegates_to_underlying_fips_endpoint() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = FakeEndpoint::new("local", network).await;
        let transport = HashtreeFipsTransport::new(endpoint.clone(), Arc::new(MemoryStore::new()));

        transport.shutdown().await.unwrap();

        assert_eq!(endpoint.shutdown_count(), 1);
    }

    #[tokio::test]
    async fn set_peer_configs_preserves_static_udp_addresses() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = FakeEndpoint::new("local", network).await;
        let transport = HashtreeFipsTransport::new(endpoint.clone(), Arc::new(MemoryStore::new()));

        transport
            .set_peer_configs(vec![
                FipsPeerConfig {
                    npub: " remote ".to_string(),
                    udp_addresses: vec![" 10.44.1.2:22121 ".to_string(), " ".to_string()],
                },
                FipsPeerConfig {
                    npub: "local".to_string(),
                    udp_addresses: vec!["10.44.1.1:22121".to_string()],
                },
            ])
            .await;

        assert_eq!(
            endpoint.configured_peer_configs.lock().await.as_slice(),
            &[FipsPeerConfig {
                npub: "remote".to_string(),
                udp_addresses: vec!["10.44.1.2:22121".to_string()],
            }]
        );
        assert_eq!(transport.configured_peer_ids().await, vec!["remote"]);
    }

    #[tokio::test]
    async fn routing_only_peers_are_configured_but_not_application_peers() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = FakeEndpoint::new("local", network).await;
        let transport = HashtreeFipsTransport::new(endpoint.clone(), Arc::new(MemoryStore::new()));

        transport
            .set_peer_configs_with_routing_peers(
                vec![FipsPeerConfig {
                    npub: "device".to_string(),
                    udp_addresses: vec!["10.44.1.2:22121".to_string()],
                }],
                vec![FipsPeerConfig {
                    npub: "bootstrap".to_string(),
                    udp_addresses: vec!["udp:203.0.113.7:2121".to_string()],
                }],
            )
            .await;

        assert_eq!(
            endpoint.configured_peers.lock().await.as_slice(),
            &["device".to_string(), "bootstrap".to_string()]
        );
        assert_eq!(transport.configured_peer_ids().await, vec!["device"]);
        assert_eq!(transport.peer_ids().await, vec!["device"]);
    }

    #[tokio::test]
    async fn empty_application_peer_set_does_not_fall_back_to_routing_peers() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = FakeEndpoint::new("local", network).await;
        let transport = HashtreeFipsTransport::new(endpoint.clone(), Arc::new(MemoryStore::new()));

        transport
            .set_peer_configs_with_routing_peers(
                Vec::new(),
                vec![FipsPeerConfig {
                    npub: "bootstrap".to_string(),
                    udp_addresses: vec!["udp:203.0.113.7:2121".to_string()],
                }],
            )
            .await;

        assert_eq!(
            endpoint.configured_peers.lock().await.as_slice(),
            &["bootstrap".to_string()]
        );
        assert!(transport.configured_peer_ids().await.is_empty());
        assert!(transport.peer_ids().await.is_empty());
        assert_eq!(transport.connected_peer_ids().await, vec!["bootstrap"]);
    }

    #[tokio::test]
    async fn peer_statuses_expose_fips_endpoint_latency_snapshot() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = FakeEndpoint::new("local", network).await;
        *endpoint.peer_statuses.lock().await = vec![FipsPeerStatus {
            npub: "remote".to_string(),
            transport_addr: Some("udp:10.44.1.2:2121".to_string()),
            transport_type: Some("udp".to_string()),
            srtt_ms: Some(23),
            packets_sent: 5,
            packets_recv: 7,
            bytes_sent: 512,
            bytes_recv: 1024,
        }];
        let transport = HashtreeFipsTransport::new(endpoint, Arc::new(MemoryStore::new()));

        assert_eq!(
            transport.peer_statuses().await,
            vec![FipsPeerStatus {
                npub: "remote".to_string(),
                transport_addr: Some("udp:10.44.1.2:2121".to_string()),
                transport_type: Some("udp".to_string()),
                srtt_ms: Some(23),
                packets_sent: 5,
                packets_recv: 7,
                bytes_sent: 512,
                bytes_recv: 1024,
            }]
        );
    }

    #[tokio::test]
    async fn fetches_hash_verified_blob_over_fips_endpoint_bytes() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let data = b"hashtree over fips".to_vec();
        let hash = hash(&data);
        let store_a = Arc::new(MemoryStore::new());
        let store_b = Arc::new(MemoryStore::new());
        store_a.put(hash, data.clone()).await.unwrap();

        let transport_a = Arc::new(HashtreeFipsTransport::new(endpoint_a, store_a));
        let transport_b = Arc::new(
            HashtreeFipsTransport::new(endpoint_b, store_b.clone())
                .with_request_timeout(Duration::from_millis(100)),
        );
        transport_a.start();
        transport_b.start();
        transport_b.set_peers(vec!["a".to_string()]).await;

        assert_eq!(transport_b.get(&hash).await.unwrap(), Some(data.clone()));
        assert_eq!(store_b.get(&hash).await.unwrap(), Some(data));
    }

    #[tokio::test]
    async fn fetches_fragmented_blob_over_fips_endpoint_bytes() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let data = (0..(FIPS_RESPONSE_FRAGMENT_SIZE * 2 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let hash = hash(&data);
        let store_a = Arc::new(MemoryStore::new());
        let store_b = Arc::new(MemoryStore::new());
        store_a.put(hash, data.clone()).await.unwrap();

        let transport_a = Arc::new(HashtreeFipsTransport::new(endpoint_a, store_a));
        let transport_b = Arc::new(
            HashtreeFipsTransport::new(endpoint_b, store_b.clone())
                .with_request_timeout(Duration::from_millis(100)),
        );
        transport_a.start();
        transport_b.start();
        transport_b.set_peers(vec!["a".to_string()]).await;

        assert_eq!(transport_b.get(&hash).await.unwrap(), Some(data.clone()));
        assert_eq!(store_b.get(&hash).await.unwrap(), Some(data));
    }

    #[tokio::test]
    async fn delivers_fragmented_app_messages_over_fips_endpoint_bytes() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let transport_a = Arc::new(HashtreeFipsTransport::new(
            endpoint_a.clone(),
            Arc::new(MemoryStore::new()),
        ));
        let transport_b = Arc::new(HashtreeFipsTransport::new(
            endpoint_b,
            Arc::new(MemoryStore::new()),
        ));
        let mut app_messages = transport_b.subscribe_app_messages();
        transport_a.start();
        transport_b.start();

        let data = (0..(FIPS_APP_FRAGMENT_SIZE * 3 + 19))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        transport_a
            .send_app_message("b", "iris-drive/root/frame/v1", data.clone())
            .await
            .unwrap();

        let message = timeout(Duration::from_millis(100), app_messages.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.peer_id, "a");
        assert_eq!(message.topic, "iris-drive/root/frame/v1");
        assert_eq!(message.data, data);
        assert!(endpoint_a.sent_count() > 1);
    }

    #[tokio::test]
    async fn app_message_broadcast_retains_bursts_until_app_drain() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = FakeEndpoint::new("local", network).await;
        let transport = HashtreeFipsTransport::new(endpoint, Arc::new(MemoryStore::new()));
        let mut app_messages = transport.subscribe_app_messages();

        let burst = 512;
        assert!(burst < APP_MESSAGE_BROADCAST_CAPACITY);
        for index in 0..burst {
            transport
                .app_messages
                .send(FipsAppMessage {
                    peer_id: "peer".to_string(),
                    topic: "iris-drive/root/frame/v1".to_string(),
                    data: index.to_string().into_bytes(),
                })
                .unwrap();
        }

        let mut received = 0usize;
        loop {
            match app_messages.try_recv() {
                Ok(_) => received += 1,
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    panic!("app message subscriber lagged by {skipped}");
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    panic!("app message channel closed");
                }
            }
        }
        assert_eq!(received, burst);
    }

    #[tokio::test]
    async fn can_deliver_unconfigured_app_messages_without_serving_blocks() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let data = b"app-only peer".to_vec();
        let hash = hash(&data);
        let store_a = Arc::new(MemoryStore::new());
        let store_b = Arc::new(MemoryStore::new());
        store_b.put(hash, data.clone()).await.unwrap();
        let transport_a = Arc::new(
            HashtreeFipsTransport::new(endpoint_a, store_a)
                .with_request_timeout(Duration::from_millis(50)),
        );
        let transport_b = Arc::new(
            HashtreeFipsTransport::new(endpoint_b, store_b)
                .with_unconfigured_app_message_topics(["iris-drive/device-link/v1/request"]),
        );
        let mut app_messages = transport_b.subscribe_app_messages();
        transport_a.start();
        transport_b.start();
        transport_b
            .set_peers(vec!["configured-peer".to_string()])
            .await;

        transport_a
            .send_app_message("b", "iris-drive/device-link/v1/request", b"join".to_vec())
            .await
            .unwrap();

        let message = timeout(Duration::from_millis(100), app_messages.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.peer_id, "a");
        assert_eq!(message.topic, "iris-drive/device-link/v1/request");
        assert_eq!(message.data, b"join");
        assert_eq!(
            transport_a
                .get_from_peers(&hash, &["b".to_string()])
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn mesh_pubsub_delivers_over_fips_endpoint_bytes() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let store_a = Arc::new(MemoryStore::new());
        let store_b = Arc::new(MemoryStore::new());
        let transport_a = Arc::new(HashtreeFipsTransport::new(endpoint_a, store_a.clone()));
        let transport_b = Arc::new(HashtreeFipsTransport::new(endpoint_b, store_b.clone()));
        transport_a.set_peers(vec!["b".to_string()]).await;
        transport_b.set_peers(vec!["a".to_string()]).await;
        transport_a.start();
        transport_b.start();

        let mesh_a = transport_a
            .start_mesh_pubsub(store_a, "a".to_string(), Duration::from_millis(200))
            .await
            .unwrap();
        let mesh_b = transport_b
            .start_mesh_pubsub(store_b, "b".to_string(), Duration::from_millis(200))
            .await
            .unwrap();
        mesh_b.subscribe_pubsub("iris-drive/root-events/test").await;

        let payload = vec![0x42; 4096];
        let delivered = timeout(Duration::from_secs(2), async {
            let mut seq = 1u64;
            loop {
                mesh_a
                    .publish_pubsub("iris-drive/root-events/test", seq, payload.clone())
                    .await;
                seq += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
                let events = mesh_b.drain_pubsub_events().await;
                if let Some(event) = events.into_iter().next() {
                    break event;
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(delivered.stream_id, "iris-drive/root-events/test");
        assert_eq!(delivered.origin_peer_id, "a");
        assert_eq!(delivered.payload, payload);
        assert_eq!(mesh_a.peer_ids().await, vec!["b"]);
        assert_eq!(mesh_b.peer_ids().await, vec!["a"]);

        mesh_b
            .unsubscribe_pubsub("iris-drive/root-events/test")
            .await;
        timeout(Duration::from_secs(2), async {
            let mut seq = 100u64;
            loop {
                let stats = mesh_a
                    .publish_pubsub("iris-drive/root-events/test", seq, payload.clone())
                    .await;
                seq += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
                let events = mesh_b.drain_pubsub_events().await;
                if stats.sent_peers == 0 && events.is_empty() {
                    break;
                }
            }
        })
        .await
        .unwrap();

        let stats = mesh_a
            .publish_pubsub("iris-drive/root-events/test", 200, payload.clone())
            .await;
        assert_eq!(stats.sent_peers, 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(mesh_b.drain_pubsub_events().await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn silence_resolves_unknown_without_retrying_same_peer() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let missing = [7u8; 32];
        let transport_a = Arc::new(HashtreeFipsTransport::new(
            endpoint_a,
            Arc::new(MemoryStore::new()),
        ));
        let transport_b = Arc::new(
            HashtreeFipsTransport::new(endpoint_b.clone(), Arc::new(MemoryStore::new()))
                .with_request_timeout(Duration::from_millis(25)),
        );
        transport_a.start();
        transport_b.start();
        transport_b.set_peers(vec!["a".to_string()]).await;

        let pending = transport_b.get(&missing);
        tokio::time::advance(Duration::from_millis(30)).await;

        assert_eq!(pending.await.unwrap(), None);
        assert_eq!(endpoint_b.sent_count(), 1);
        assert!(
            transport_b.pending.lock().await.is_empty(),
            "timed-out requests should not leave stale pending senders"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_dropped_request_to_same_peer() {
        let network = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_a = FakeEndpoint::new("a", network.clone()).await;
        let endpoint_b = FakeEndpoint::new("b", network).await;
        let data = b"retried request".to_vec();
        let hash = hash(&data);
        let store_a = Arc::new(MemoryStore::new());
        store_a.put(hash, data.clone()).await.unwrap();
        endpoint_b.drop_next_sends(1);
        let transport_a = Arc::new(HashtreeFipsTransport::new(endpoint_a, store_a));
        let transport_b = Arc::new(
            HashtreeFipsTransport::new(endpoint_b.clone(), Arc::new(MemoryStore::new()))
                .with_request_timeout(Duration::from_millis(300))
                .with_request_retry_interval(Duration::from_millis(50)),
        );
        transport_a.start();
        transport_b.start();
        transport_b.set_peers(vec!["a".to_string()]).await;

        let pending = transport_b.get(&hash);
        tokio::time::advance(Duration::from_millis(60)).await;

        assert_eq!(pending.await.unwrap(), Some(data));
        assert_eq!(endpoint_b.sent_count(), 2);
    }
}
