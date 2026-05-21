//! Stub module for when P2P feature is disabled
//! Provides minimal types to allow code to compile without webrtc dependencies

use anyhow::Result;
use nostr::{nips::nip19::FromBech32, Alphabet, Event, Filter, Kind, PublicKey, SingleLetterTag};
use serde::{Deserialize, Serialize};
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
            .kind(Kind::Custom(30078))
            .author(author)
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::D),
                vec![tree_name.to_string()],
            )
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::L),
                vec!["hashtree".to_string()],
            )
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
            a.id.cmp(&b.id)
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

    for tag in &event.tags {
        let slice = tag.as_slice();
        if slice.len() < 2 {
            continue;
        }
        match slice[0].as_str() {
            "d" => tree_match = slice[1].as_str() == tree_name,
            "l" => labeled = slice[1].as_str() == "hashtree",
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
        created_at: event.created_at.as_u64(),
        peer_id: peer_id.to_string(),
    })
}

/// Content store trait stub
pub trait ContentStore: Send + Sync + 'static {
    /// Get content by hex hash
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>>;
}

pub mod types {
    use super::*;

    pub const MAX_HTL: u8 = 7;
    pub const MSG_TYPE_REQUEST: u8 = 0x00;
    pub const MSG_TYPE_RESPONSE: u8 = 0x01;
    pub const MSG_TYPE_QUOTE_REQUEST: u8 = 0x02;
    pub const MSG_TYPE_QUOTE_RESPONSE: u8 = 0x03;
    pub const MSG_TYPE_PAYMENT: u8 = 0x04;
    pub const MSG_TYPE_PAYMENT_ACK: u8 = 0x05;
    pub const MSG_TYPE_CHUNK: u8 = 0x06;
    pub const MSG_TYPE_PEER_HINTS: u8 = 0x07;
    pub const MSG_TYPE_PUBSUB_INTEREST: u8 = 0x08;
    pub const MSG_TYPE_PUBSUB_FRAME: u8 = 0x09;
    pub const MSG_TYPE_PUBSUB_INVENTORY: u8 = 0x0a;
    pub const MSG_TYPE_PUBSUB_WANT: u8 = 0x0b;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataRequest {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        #[serde(default = "default_htl")]
        pub htl: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub q: Option<u64>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataResponse {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        #[serde(with = "serde_bytes")]
        pub d: Vec<u8>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub i: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub n: Option<u32>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataQuoteRequest {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        pub p: u64,
        pub t: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub m: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataQuoteResponse {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        pub a: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub q: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub p: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub t: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub m: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataPayment {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        pub q: u64,
        pub c: u32,
        pub p: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub m: Option<String>,
        pub tok: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataPaymentAck {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        pub q: u64,
        pub c: u32,
        pub a: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub e: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DataChunk {
        #[serde(with = "serde_bytes")]
        pub h: Vec<u8>,
        pub q: u64,
        pub c: u32,
        pub n: u32,
        pub p: u64,
        #[serde(with = "serde_bytes")]
        pub d: Vec<u8>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PeerHints {
        #[serde(default, rename = "u")]
        pub signal_urls: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PubsubInterest {
        #[serde(rename = "s")]
        pub stream_id: String,
        #[serde(rename = "sub")]
        pub subscriber_peer_id: String,
        #[serde(rename = "q")]
        pub seq: u64,
        #[serde(rename = "a")]
        pub active: bool,
        #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
        pub htl: u8,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PubsubFrame {
        #[serde(rename = "s")]
        pub stream_id: String,
        #[serde(rename = "q")]
        pub seq: u64,
        #[serde(rename = "o")]
        pub origin_peer_id: String,
        #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
        pub htl: u8,
        #[serde(with = "serde_bytes", rename = "d")]
        pub payload: Vec<u8>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PubsubInventory {
        #[serde(rename = "s")]
        pub stream_id: String,
        #[serde(rename = "q")]
        pub seq: u64,
        #[serde(rename = "o")]
        pub origin_peer_id: String,
        #[serde(rename = "b")]
        pub payload_bytes: u64,
        #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
        pub htl: u8,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PubsubWant {
        #[serde(rename = "s")]
        pub stream_id: String,
        #[serde(rename = "q")]
        pub seq: u64,
        #[serde(rename = "o")]
        pub origin_peer_id: String,
    }

    #[derive(Debug, Clone)]
    pub enum DataMessage {
        Request(DataRequest),
        Response(DataResponse),
        QuoteRequest(DataQuoteRequest),
        QuoteResponse(DataQuoteResponse),
        Payment(DataPayment),
        PaymentAck(DataPaymentAck),
        Chunk(DataChunk),
        PeerHints(PeerHints),
        PubsubInterest(PubsubInterest),
        PubsubFrame(PubsubFrame),
        PubsubInventory(PubsubInventory),
        PubsubWant(PubsubWant),
    }

    fn default_htl() -> u8 {
        MAX_HTL
    }

    fn is_max_htl(htl: &u8) -> bool {
        *htl == MAX_HTL
    }

    pub fn encode_request(req: &DataRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(req)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_REQUEST);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_response(res: &DataResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(res)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_RESPONSE);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_quote_request(
        req: &DataQuoteRequest,
    ) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(req)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_QUOTE_REQUEST);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_quote_response(
        res: &DataQuoteResponse,
    ) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(res)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_QUOTE_RESPONSE);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_payment(req: &DataPayment) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(req)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_PAYMENT);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_payment_ack(res: &DataPaymentAck) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(res)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_PAYMENT_ACK);
        result.extend(body);
        Ok(result)
    }

    pub fn encode_chunk(chunk: &DataChunk) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        let body = rmp_serde::to_vec_named(chunk)?;
        let mut result = Vec::with_capacity(1 + body.len());
        result.push(MSG_TYPE_CHUNK);
        result.extend(body);
        Ok(result)
    }

    pub fn parse_message(data: &[u8]) -> Result<DataMessage, rmp_serde::decode::Error> {
        if data.is_empty() {
            return Err(rmp_serde::decode::Error::LengthMismatch(0));
        }

        match data[0] {
            MSG_TYPE_REQUEST => Ok(DataMessage::Request(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_RESPONSE => Ok(DataMessage::Response(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_QUOTE_REQUEST => Ok(DataMessage::QuoteRequest(rmp_serde::from_slice(
                &data[1..],
            )?)),
            MSG_TYPE_QUOTE_RESPONSE => Ok(DataMessage::QuoteResponse(rmp_serde::from_slice(
                &data[1..],
            )?)),
            MSG_TYPE_PAYMENT => Ok(DataMessage::Payment(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_PAYMENT_ACK => Ok(DataMessage::PaymentAck(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_CHUNK => Ok(DataMessage::Chunk(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_PEER_HINTS => Ok(DataMessage::PeerHints(rmp_serde::from_slice(&data[1..])?)),
            MSG_TYPE_PUBSUB_INTEREST => Ok(DataMessage::PubsubInterest(rmp_serde::from_slice(
                &data[1..],
            )?)),
            MSG_TYPE_PUBSUB_FRAME => {
                Ok(DataMessage::PubsubFrame(rmp_serde::from_slice(&data[1..])?))
            }
            MSG_TYPE_PUBSUB_INVENTORY => Ok(DataMessage::PubsubInventory(rmp_serde::from_slice(
                &data[1..],
            )?)),
            MSG_TYPE_PUBSUB_WANT => Ok(DataMessage::PubsubWant(rmp_serde::from_slice(&data[1..])?)),
            other => Err(rmp_serde::decode::Error::LengthMismatch(other as u32)),
        }
    }
}
