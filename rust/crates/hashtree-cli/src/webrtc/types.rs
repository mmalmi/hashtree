//! WebRTC signaling types compatible with iris-client and hashtree-ts

use rand::Rng;
use serde::{Deserialize, Serialize};

// HTL (Hops To Live) constants - Freenet-style probabilistic decrement
pub const MAX_HTL: u8 = 10;
pub const DECREMENT_AT_MAX_PROB: f64 = 0.5;  // 50% chance to decrement at max
pub const DECREMENT_AT_MIN_PROB: f64 = 0.25; // 25% chance to decrement at 1

/// Per-peer HTL decrement configuration (Freenet-style)
/// Stored per peer connection to prevent probing attacks
#[derive(Debug, Clone)]
pub struct PeerHTLConfig {
    pub decrement_at_max: bool,  // Whether to decrement when HTL is at max
    pub decrement_at_min: bool,  // Whether to decrement when HTL is 1
}

impl PeerHTLConfig {
    /// Generate random HTL decrement config for a new peer connection
    /// This is decided once per peer, not per request, to prevent probing
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            decrement_at_max: rng.gen_bool(DECREMENT_AT_MAX_PROB),
            decrement_at_min: rng.gen_bool(DECREMENT_AT_MIN_PROB),
        }
    }
}

impl Default for PeerHTLConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Decrement HTL using Freenet-style probabilistic rules
/// - At max HTL: probabilistic decrement (50% by default)
/// - At HTL=1: probabilistic decrement (25% by default)
/// - Otherwise: always decrement
///
/// Returns new HTL value
pub fn decrement_htl(htl: u8, config: &PeerHTLConfig) -> u8 {
    // Clamp to max
    let htl = htl.min(MAX_HTL);

    // Already dead
    if htl == 0 {
        return 0;
    }

    // At max: probabilistic decrement
    if htl == MAX_HTL {
        return if config.decrement_at_max { htl - 1 } else { htl };
    }

    // At min (1): probabilistic decrement
    if htl == 1 {
        return if config.decrement_at_min { 0 } else { htl };
    }

    // Middle: always decrement
    htl - 1
}

/// Check if a request should be forwarded based on HTL
pub fn should_forward(htl: u8) -> bool {
    htl > 0
}

/// Event kind for WebRTC signaling (ephemeral kind 25050)
/// All signaling uses this kind - hellos use #l tag, directed use gift wrap
pub const WEBRTC_KIND: u64 = 25050;

/// Tag for hello messages (broadcast discovery)
pub const HELLO_TAG: &str = "hello";

/// Legacy tag for WebRTC signaling messages (kept for compatibility)
pub const WEBRTC_TAG: &str = "webrtc";

/// Generate a UUID for peer identification
pub fn generate_uuid() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    format!(
        "{}{}",
        (0..15).map(|_| char::from_digit(rng.gen_range(0..36), 36).unwrap()).collect::<String>(),
        (0..15).map(|_| char::from_digit(rng.gen_range(0..36), 36).unwrap()).collect::<String>()
    )
}

/// Peer identifier combining pubkey and session UUID
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerId {
    pub pubkey: String,
    pub uuid: String,
}

impl PeerId {
    pub fn new(pubkey: String, uuid: Option<String>) -> Self {
        Self {
            pubkey,
            uuid: uuid.unwrap_or_else(generate_uuid),
        }
    }

    pub fn from_string(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 2 {
            Some(Self {
                pubkey: parts[0].to_string(),
                uuid: parts[1].to_string(),
            })
        } else {
            None
        }
    }

    pub fn short(&self) -> String {
        format!("{}:{}", &self.pubkey[..8.min(self.pubkey.len())], &self.uuid[..6.min(self.uuid.len())])
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.pubkey, self.uuid)
    }
}

/// Hello message for peer discovery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(rename = "peerId")]
    pub peer_id: String,
}

/// WebRTC offer message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub offer: serde_json::Value,
    pub recipient: String,
    #[serde(rename = "peerId")]
    pub peer_id: String,
}

/// WebRTC answer message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub answer: serde_json::Value,
    pub recipient: String,
    #[serde(rename = "peerId")]
    pub peer_id: String,
}

/// ICE candidate message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub candidate: serde_json::Value,
    pub recipient: String,
    #[serde(rename = "peerId")]
    pub peer_id: String,
}

/// Batched ICE candidates message (hashtree-ts extension)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidatesMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub candidates: Vec<serde_json::Value>,
    pub recipient: String,
    #[serde(rename = "peerId")]
    pub peer_id: String,
}

/// All signaling message types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "hello")]
    Hello { #[serde(rename = "peerId")] peer_id: String },
    #[serde(rename = "offer")]
    Offer {
        offer: serde_json::Value,
        recipient: String,
        #[serde(rename = "peerId")]
        peer_id: String,
    },
    #[serde(rename = "answer")]
    Answer {
        answer: serde_json::Value,
        recipient: String,
        #[serde(rename = "peerId")]
        peer_id: String,
    },
    #[serde(rename = "candidate")]
    Candidate {
        candidate: serde_json::Value,
        recipient: String,
        #[serde(rename = "peerId")]
        peer_id: String,
    },
    #[serde(rename = "candidates")]
    Candidates {
        candidates: Vec<serde_json::Value>,
        recipient: String,
        #[serde(rename = "peerId")]
        peer_id: String,
    },
}

impl SignalingMessage {
    pub fn msg_type(&self) -> &str {
        match self {
            SignalingMessage::Hello { .. } => "hello",
            SignalingMessage::Offer { .. } => "offer",
            SignalingMessage::Answer { .. } => "answer",
            SignalingMessage::Candidate { .. } => "candidate",
            SignalingMessage::Candidates { .. } => "candidates",
        }
    }

    pub fn recipient(&self) -> Option<&str> {
        match self {
            SignalingMessage::Hello { .. } => None,
            SignalingMessage::Offer { recipient, .. } => Some(recipient),
            SignalingMessage::Answer { recipient, .. } => Some(recipient),
            SignalingMessage::Candidate { recipient, .. } => Some(recipient),
            SignalingMessage::Candidates { recipient, .. } => Some(recipient),
        }
    }

    pub fn peer_id(&self) -> &str {
        match self {
            SignalingMessage::Hello { peer_id } => peer_id,
            SignalingMessage::Offer { peer_id, .. } => peer_id,
            SignalingMessage::Answer { peer_id, .. } => peer_id,
            SignalingMessage::Candidate { peer_id, .. } => peer_id,
            SignalingMessage::Candidates { peer_id, .. } => peer_id,
        }
    }

    pub fn hello(peer_id: &str) -> Self {
        SignalingMessage::Hello {
            peer_id: peer_id.to_string(),
        }
    }

    pub fn offer(offer: serde_json::Value, recipient: &str, peer_id: &str) -> Self {
        SignalingMessage::Offer {
            offer,
            recipient: recipient.to_string(),
            peer_id: peer_id.to_string(),
        }
    }

    pub fn answer(answer: serde_json::Value, recipient: &str, peer_id: &str) -> Self {
        SignalingMessage::Answer {
            answer,
            recipient: recipient.to_string(),
            peer_id: peer_id.to_string(),
        }
    }

    pub fn candidate(candidate: serde_json::Value, recipient: &str, peer_id: &str) -> Self {
        SignalingMessage::Candidate {
            candidate,
            recipient: recipient.to_string(),
            peer_id: peer_id.to_string(),
        }
    }
}

/// Configuration for WebRTC manager
#[derive(Clone)]
pub struct WebRTCConfig {
    /// Nostr relays for signaling
    pub relays: Vec<String>,
    /// Maximum outbound connections (legacy, use pools instead)
    pub max_outbound: usize,
    /// Maximum inbound connections (legacy, use pools instead)
    pub max_inbound: usize,
    /// Hello message interval in milliseconds
    pub hello_interval_ms: u64,
    /// Message timeout in milliseconds
    pub message_timeout_ms: u64,
    /// STUN servers for NAT traversal
    pub stun_servers: Vec<String>,
    /// Enable debug logging
    pub debug: bool,
    /// Pool settings for follows and other peers
    pub pools: PoolSettings,
}

impl Default for WebRTCConfig {
    fn default() -> Self {
        Self {
            relays: vec![
                "wss://relay.damus.io".to_string(),
                "wss://relay.primal.net".to_string(),
                "wss://nos.lol".to_string(),
                "wss://temp.iris.to".to_string(),
                "wss://relay.snort.social".to_string(),
            ],
            max_outbound: 6,
            max_inbound: 6,
            hello_interval_ms: 10000,
            message_timeout_ms: 15000,
            stun_servers: vec![
                "stun:stun.iris.to:3478".to_string(),
                "stun:stun.l.google.com:19302".to_string(),
                "stun:stun.cloudflare.com:3478".to_string(),
            ],
            debug: false,
            pools: PoolSettings::default(),
        }
    }
}

/// Peer connection status
#[derive(Debug, Clone)]
pub struct PeerStatus {
    pub peer_id: String,
    pub pubkey: String,
    pub state: String,
    pub direction: PeerDirection,
    pub connected_at: Option<std::time::Instant>,
    pub pool: PeerPool,
}

/// Direction of peer connection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDirection {
    Inbound,
    Outbound,
}

/// Peer state change event for signaling layer notification
#[derive(Debug, Clone)]
pub enum PeerStateEvent {
    /// Peer connection succeeded
    Connected(PeerId),
    /// Peer connection failed
    Failed(PeerId),
    /// Peer disconnected
    Disconnected(PeerId),
}

/// Pool type for peer classification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerPool {
    /// Users in social graph (followed or followers)
    Follows,
    /// Everyone else
    Other,
}

/// Configuration for a peer pool
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    /// Maximum connections in this pool
    pub max_connections: usize,
    /// Number of connections to consider "satisfied" (stop sending hellos)
    pub satisfied_connections: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            satisfied_connections: 5,
        }
    }
}

/// Pool settings for both pools
#[derive(Debug, Clone)]
pub struct PoolSettings {
    pub follows: PoolConfig,
    pub other: PoolConfig,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            follows: PoolConfig {
                max_connections: 20,
                satisfied_connections: 10,
            },
            other: PoolConfig {
                max_connections: 10,
                satisfied_connections: 5,
            },
        }
    }
}

impl std::fmt::Display for PeerDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerDirection::Inbound => write!(f, "inbound"),
            PeerDirection::Outbound => write!(f, "outbound"),
        }
    }
}

/// Message type bytes (prefix before MessagePack body)
pub const MSG_TYPE_REQUEST: u8 = 0x00;
pub const MSG_TYPE_RESPONSE: u8 = 0x01;

/// Hashtree data channel protocol messages
/// Shared between WebRTC data channels and WebSocket transport
///
/// Wire format: [type byte][msgpack body]
/// Request:  [0x00][msgpack: {h: bytes32, htl?: u8}]
/// Response: [0x01][msgpack: {h: bytes32, d: bytes}]

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataRequest {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,  // 32-byte hash
    #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
    pub htl: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataResponse {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,  // 32-byte hash
    #[serde(with = "serde_bytes")]
    pub d: Vec<u8>,  // Data
}

#[derive(Debug, Clone)]
pub enum DataMessage {
    Request(DataRequest),
    Response(DataResponse),
}

fn default_htl() -> u8 {
    MAX_HTL
}

fn is_max_htl(htl: &u8) -> bool {
    *htl == MAX_HTL
}

/// Encode a request to wire format: [0x00][msgpack body]
/// Uses named fields for cross-language compatibility with TypeScript
pub fn encode_request(req: &DataRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let body = rmp_serde::to_vec_named(req)?;
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_REQUEST);
    result.extend(body);
    Ok(result)
}

/// Encode a response to wire format: [0x01][msgpack body]
/// Uses named fields for cross-language compatibility with TypeScript
pub fn encode_response(res: &DataResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let body = rmp_serde::to_vec_named(res)?;
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_RESPONSE);
    result.extend(body);
    Ok(result)
}

/// Parse a wire format message
pub fn parse_message(data: &[u8]) -> Result<DataMessage, rmp_serde::decode::Error> {
    if data.is_empty() {
        return Err(rmp_serde::decode::Error::LengthMismatch(0));
    }

    let msg_type = data[0];
    let body = &data[1..];

    match msg_type {
        MSG_TYPE_REQUEST => {
            let req: DataRequest = rmp_serde::from_slice(body)?;
            Ok(DataMessage::Request(req))
        }
        MSG_TYPE_RESPONSE => {
            let res: DataResponse = rmp_serde::from_slice(body)?;
            Ok(DataMessage::Response(res))
        }
        _ => Err(rmp_serde::decode::Error::LengthMismatch(msg_type as u32)),
    }
}

/// Convert hash to hex string for logging/map keys
pub fn hash_to_hex(hash: &[u8]) -> String {
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Encode a DataMessage to wire format (deprecated - use encode_request/encode_response)
pub fn encode_message(msg: &DataMessage) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    match msg {
        DataMessage::Request(req) => encode_request(req),
        DataMessage::Response(res) => encode_response(res),
    }
}
