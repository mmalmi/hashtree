//! Stub module for when P2P feature is disabled
//! Provides minimal types to allow code to compile without webrtc dependencies

use anyhow::Result;
use git_remote_htree::nostr_client::hashtree_root_kinds;
use nostr::{nips::nip19::FromBech32, Alphabet, Event, Filter, PublicKey, SingleLetterTag};
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Connection state stub
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Discovered,
    Connecting,
    Connected,
    Failed,
    Disconnected,
}

/// Peer entry stub
#[derive(Debug)]
pub struct PeerEntry {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub state: ConnectionState,
    pub last_seen: std::time::Instant,
    pub peer: Option<DummyPeer>,
    pub pool: PeerPool,
    pub transport: PeerTransport,
    pub signal_paths: BTreeSet<PeerSignalPath>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Peer ID stub
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerId {
    pub pubkey: String,
}

impl PeerId {
    pub fn new(pubkey: String) -> Self {
        Self { pubkey }
    }

    pub fn short(&self) -> String {
        self.pubkey[..8.min(self.pubkey.len())].to_string()
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.pubkey)
    }
}

/// Direction of peer connection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDirection {
    Inbound,
    Outbound,
}

/// Peer transport stub
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTransport {
    WebRtc,
    Bluetooth,
}

impl std::fmt::Display for PeerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerTransport::WebRtc => f.write_str("webrtc"),
            PeerTransport::Bluetooth => f.write_str("bluetooth"),
        }
    }
}

/// Signaling/discovery path stub
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PeerSignalPath {
    Relay,
    Multicast,
    WifiAware,
    Bluetooth,
}

impl std::fmt::Display for PeerSignalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerSignalPath::Relay => f.write_str("relay"),
            PeerSignalPath::Multicast => f.write_str("multicast"),
            PeerSignalPath::WifiAware => f.write_str("wifi-aware"),
            PeerSignalPath::Bluetooth => f.write_str("bluetooth"),
        }
    }
}

/// Dummy peer stub
#[derive(Debug)]
pub struct DummyPeer;

impl DummyPeer {
    pub fn is_ready(&self) -> bool {
        false
    }

    pub fn has_data_channel(&self) -> bool {
        false
    }

    pub fn state(&self) -> &str {
        "Disabled"
    }

    pub fn as_webrtc(&self) -> Option<&Self> {
        None
    }

    pub async fn request(&self, _hash: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// Peer pool stub
#[derive(Debug, Clone, Copy)]
pub enum PeerPool {
    Follows,
    Other,
}

#[derive(Debug, Clone)]
pub struct PeerRootEvent {
    pub hash: String,
    pub key: Option<String>,
    pub encrypted_key: Option<String>,
    pub self_encrypted_key: Option<String>,
    pub event_id: String,
    pub created_at: u64,
    pub peer_id: String,
}

/// WebRTC state stub - always empty when P2P is disabled
#[derive(Debug)]
pub struct WebRTCState {
    pub peers: Arc<RwLock<HashMap<String, PeerEntry>>>,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
}

impl Default for WebRTCState {
    fn default() -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
        }
    }
}

impl WebRTCState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Query peers for data - always returns None when P2P is disabled
    pub async fn query_peers_for_data(&self, _hash: &str) -> Option<Vec<u8>> {
        None
    }

    /// Request from peers - always returns None when P2P is disabled
    pub async fn request_from_peers(&self, _hash: &str) -> Option<Vec<u8>> {
        None
    }

    /// Request from peers with source - always returns None when P2P is disabled
    pub async fn request_from_peers_with_source(&self, _hash: &str) -> Option<(Vec<u8>, String)> {
        None
    }

    pub async fn record_sent(&self, peer_id: &str, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_sent += bytes;
        }
    }

    pub async fn record_received(&self, peer_id: &str, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_received += bytes;
        }
    }

    /// Get bandwidth stats - always returns zeros when P2P is disabled
    pub fn get_bandwidth(&self) -> (u64, u64) {
        (
            self.bytes_sent.load(Ordering::Relaxed),
            self.bytes_received.load(Ordering::Relaxed),
        )
    }

    /// Get mesh stats - always returns zeros when P2P is disabled
    pub fn get_mesh_stats(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }

    /// Resolve roots from peers - always returns None when P2P is disabled
    pub async fn resolve_root_from_peers(
        &self,
        _owner_pubkey: &str,
        _tree_name: &str,
        _per_peer_timeout: Duration,
    ) -> Option<PeerRootEvent> {
        None
    }

    pub async fn resolve_root_from_local_buses_with_source(
        &self,
        _owner_pubkey: &str,
        _tree_name: &str,
        _timeout: Duration,
    ) -> Option<(&'static str, PeerRootEvent)> {
        None
    }
}

pub fn build_root_filter(owner_pubkey: &str, tree_name: &str) -> Option<Filter> {
    let author = PublicKey::from_hex(owner_pubkey)
        .or_else(|_| PublicKey::from_bech32(owner_pubkey))
        .ok()?;
    Some(
        Filter::new()
            .kinds(hashtree_root_kinds())
            .author(author)
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::D),
                tree_name.to_string(),
            )
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), "hashtree")
            .limit(50),
    )
}

pub fn pick_latest_event<'a, I>(events: I) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    events.into_iter().max_by(|a, b| {
        let ordering = a.created_at.cmp(&b.created_at);
        if ordering == std::cmp::Ordering::Equal {
            b.id.cmp(&a.id)
        } else {
            ordering
        }
    })
}

pub fn root_event_from_peer(
    event: &Event,
    peer_id: &str,
    tree_name: &str,
) -> Option<PeerRootEvent> {
    let mut tree_match = false;
    let mut labeled = false;
    let mut key = None;
    let mut encrypted_key = None;
    let mut self_encrypted_key = None;
    let mut hash_tag = None;

    for tag in event.tags.iter() {
        let slice = tag.as_slice();
        if slice.len() < 2 {
            continue;
        }
        match slice[0].as_str() {
            "d" => tree_match = slice[1].as_str() == tree_name,
            "l" => labeled |= slice[1].as_str() == "hashtree",
            "hash" => hash_tag = Some(slice[1].to_string()),
            "key" => key = Some(slice[1].to_string()),
            "encryptedKey" => encrypted_key = Some(slice[1].to_string()),
            "selfEncryptedKey" => self_encrypted_key = Some(slice[1].to_string()),
            _ => {}
        }
    }

    if !tree_match || !labeled {
        return None;
    }

    let hash = hash_tag.or_else(|| {
        if event.content.is_empty() {
            None
        } else {
            Some(event.content.clone())
        }
    })?;

    Some(PeerRootEvent {
        hash,
        key,
        encrypted_key,
        self_encrypted_key,
        event_id: event.id.to_hex(),
        created_at: event.created_at.as_secs(),
        peer_id: peer_id.to_string(),
    })
}

/// Content store trait stub
pub trait ContentStore: Send + Sync + 'static {
    /// Get content by hex hash
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>>;
}

/// Canonical Hashtree mesh wire types. The non-WebRTC build re-exports the
/// same codec instead of maintaining a second framing implementation.
pub mod types {
    pub use hashtree_network::{
        encode_chunk, encode_payment, encode_payment_ack, encode_peer_hints, encode_pubsub_frame,
        encode_pubsub_interest, encode_pubsub_inventory, encode_pubsub_want, encode_quote_request,
        encode_quote_response, encode_request, encode_response, parse_message, DataChunk,
        DataMessage, DataPayment, DataPaymentAck, DataQuoteRequest, DataQuoteResponse, DataRequest,
        DataResponse, PeerHints, PubsubFrame, PubsubInterest, PubsubInventory, PubsubWant, MAX_HTL,
    };
}
