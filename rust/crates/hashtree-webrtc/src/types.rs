//! WebRTC transport types for P2P data exchange
//!
//! Defines message types for WebRTC signaling via Nostr relays
//! and the data channel protocol for hash-based data requests.

use hashtree_core::Hash;
use serde::{Deserialize, Serialize};

/// Unique identifier for a peer in the network
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId {
    /// Nostr public key (hex encoded)
    pub pubkey: String,
    /// Unique session identifier
    pub uuid: String,
}

impl PeerId {
    pub fn new(pubkey: String, uuid: String) -> Self {
        Self { pubkey, uuid }
    }

    pub fn to_peer_string(&self) -> String {
        format!("{}:{}", self.pubkey, self.uuid)
    }

    pub fn from_peer_string(s: &str) -> Option<Self> {
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
}

/// Signaling message types sent via Nostr relays
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    /// Initial hello message to discover peers
    #[serde(rename = "hello")]
    Hello {
        #[serde(rename = "peerId")]
        peer_id: String,
        roots: Vec<String>,
    },

    /// WebRTC offer (SDP)
    #[serde(rename = "offer")]
    Offer {
        #[serde(rename = "peerId")]
        peer_id: String,
        #[serde(rename = "targetPeerId")]
        target_peer_id: String,
        sdp: String,
    },

    /// WebRTC answer (SDP)
    #[serde(rename = "answer")]
    Answer {
        #[serde(rename = "peerId")]
        peer_id: String,
        #[serde(rename = "targetPeerId")]
        target_peer_id: String,
        sdp: String,
    },

    /// Single ICE candidate
    #[serde(rename = "candidate")]
    Candidate {
        #[serde(rename = "peerId")]
        peer_id: String,
        #[serde(rename = "targetPeerId")]
        target_peer_id: String,
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_m_line_index: Option<u16>,
        #[serde(rename = "sdpMid")]
        sdp_mid: Option<String>,
    },

    /// Batched ICE candidates
    #[serde(rename = "candidates")]
    Candidates {
        #[serde(rename = "peerId")]
        peer_id: String,
        #[serde(rename = "targetPeerId")]
        target_peer_id: String,
        candidates: Vec<IceCandidate>,
    },
}

impl SignalingMessage {
    /// Get the sender's peer ID
    pub fn peer_id(&self) -> &str {
        match self {
            Self::Hello { peer_id, .. } => peer_id,
            Self::Offer { peer_id, .. } => peer_id,
            Self::Answer { peer_id, .. } => peer_id,
            Self::Candidate { peer_id, .. } => peer_id,
            Self::Candidates { peer_id, .. } => peer_id,
        }
    }

    /// Get the target peer ID (if applicable)
    pub fn target_peer_id(&self) -> Option<&str> {
        match self {
            Self::Hello { .. } => None, // Broadcast
            Self::Offer { target_peer_id, .. } => Some(target_peer_id),
            Self::Answer { target_peer_id, .. } => Some(target_peer_id),
            Self::Candidate { target_peer_id, .. } => Some(target_peer_id),
            Self::Candidates { target_peer_id, .. } => Some(target_peer_id),
        }
    }

    /// Check if this message is addressed to a specific peer
    pub fn is_for(&self, my_peer_id: &str) -> bool {
        match self.target_peer_id() {
            Some(target) => target == my_peer_id,
            None => true, // Broadcasts are for everyone
        }
    }
}

/// Perfect negotiation: determine if we are the "polite" peer
///
/// In WebRTC perfect negotiation, both peers can send offers simultaneously.
/// When a collision occurs (we receive an offer while we have a pending offer),
/// the "polite" peer backs off and accepts the incoming offer instead.
///
/// The "impolite" peer (higher ID) keeps their offer and ignores the incoming one.
///
/// This pattern ensures connections form even when one peer is "satisfied" -
/// the unsatisfied peer can still initiate and the satisfied peer will accept.
#[inline]
pub fn is_polite_peer(local_peer_id: &str, remote_peer_id: &str) -> bool {
    // Lower ID is "polite" - they back off on collision
    local_peer_id < remote_peer_id
}

/// ICE candidate for WebRTC connection establishment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMLineIndex")]
    pub sdp_m_line_index: Option<u16>,
    #[serde(rename = "sdpMid")]
    pub sdp_mid: Option<String>,
}

/// Data channel message types for hash-based data exchange
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DataMessage {
    /// Request data by hash
    #[serde(rename = "req")]
    Request {
        id: u32,
        hash: String,
        /// Hops To Live - decremented on each forward hop
        /// When htl reaches 0, request is not forwarded further
        #[serde(skip_serializing_if = "Option::is_none")]
        htl: Option<u8>,
    },

    /// Response with data (binary payload follows)
    #[serde(rename = "res")]
    Response {
        id: u32,
        hash: String,
        found: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
    },

    /// Push data for a hash the peer previously requested but we didn't have
    /// This happens when we get it later from another peer
    #[serde(rename = "push")]
    Push { hash: String },

    /// Announce available hashes
    #[serde(rename = "have")]
    Have { hashes: Vec<String> },

    /// Request list of wanted hashes
    #[serde(rename = "want")]
    Want { hashes: Vec<String> },

    /// Notify about root hash update
    #[serde(rename = "root")]
    RootUpdate {
        hash: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
    },
}

/// HTL (Hops To Live) constants - Freenet-style probabilistic decrement
pub const MAX_HTL: u8 = 10;
/// Probability to decrement at max HTL (50%)
pub const DECREMENT_AT_MAX_PROB: f64 = 0.5;
/// Probability to decrement at min HTL=1 (25%)
pub const DECREMENT_AT_MIN_PROB: f64 = 0.25;

/// Per-peer HTL configuration (Freenet-style probabilistic decrement)
/// Generated once per peer connection, stays fixed for connection lifetime
#[derive(Debug, Clone, Copy)]
pub struct PeerHTLConfig {
    /// Whether to decrement at MAX_HTL
    pub decrement_at_max: bool,
    /// Whether to decrement at HTL=1
    pub decrement_at_min: bool,
}

impl PeerHTLConfig {
    /// Generate random HTL config for a new peer connection
    pub fn random() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        Self {
            decrement_at_max: rng.gen::<f64>() < DECREMENT_AT_MAX_PROB,
            decrement_at_min: rng.gen::<f64>() < DECREMENT_AT_MIN_PROB,
        }
    }

    /// Decrement HTL using this peer's config (Freenet-style probabilistic)
    /// Called when SENDING to a peer, not on receive
    pub fn decrement(&self, htl: u8) -> u8 {
        if htl == 0 {
            return 0;
        }

        if htl == MAX_HTL {
            // At max: only decrement if this peer's config says so
            if self.decrement_at_max {
                htl - 1
            } else {
                htl
            }
        } else if htl == 1 {
            // At min: only decrement if this peer's config says so
            if self.decrement_at_min {
                0
            } else {
                htl
            }
        } else {
            // Middle values: always decrement
            htl - 1
        }
    }
}

/// Check if a request should be forwarded based on HTL
pub fn should_forward(htl: u8) -> bool {
    htl > 0
}

use tokio::sync::{mpsc, oneshot};

/// Request to forward a data request to other peers
pub struct ForwardRequest {
    /// Hash being requested
    pub hash: Hash,
    /// Peer ID to exclude (the one who sent the request)
    pub exclude_peer_id: String,
    /// HTL for forwarded request
    pub htl: u8,
    /// Channel to send result back
    pub response: oneshot::Sender<Option<Vec<u8>>>,
}

/// Sender for forward requests
pub type ForwardTx = mpsc::Sender<ForwardRequest>;
/// Receiver for forward requests
pub type ForwardRx = mpsc::Receiver<ForwardRequest>;

/// Peer pool classification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerPool {
    /// Users in social graph (followed or followers)
    Follows,
    /// Everyone else
    Other,
}

/// Settings for a peer pool
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    /// Maximum connections in this pool
    pub max_connections: usize,
    /// Number of connections to consider "satisfied"
    pub satisfied_connections: usize,
}

impl PoolConfig {
    /// Check if we can accept more peers (below max)
    #[inline]
    pub fn can_accept(&self, current_count: usize) -> bool {
        current_count < self.max_connections
    }

    /// Check if we need more peers (below satisfied)
    #[inline]
    pub fn needs_peers(&self, current_count: usize) -> bool {
        current_count < self.satisfied_connections
    }

    /// Check if we're satisfied (at or above satisfied threshold)
    #[inline]
    pub fn is_satisfied(&self, current_count: usize) -> bool {
        current_count >= self.satisfied_connections
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 20,
            satisfied_connections: 10,
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
                max_connections: 20,
                satisfied_connections: 10,
            },
        }
    }
}

/// Request to classify a peer by pubkey
pub struct ClassifyRequest {
    /// Pubkey to classify (hex)
    pub pubkey: String,
    /// Channel to send result back
    pub response: oneshot::Sender<PeerPool>,
}

/// Sender for peer classification requests
pub type ClassifierTx = mpsc::Sender<ClassifyRequest>;

/// Receiver for peer classification requests (implement this to provide classification)
pub type ClassifierRx = mpsc::Receiver<ClassifyRequest>;

/// Create a classifier channel pair
pub fn classifier_channel(buffer: usize) -> (ClassifierTx, ClassifierRx) {
    mpsc::channel(buffer)
}

/// Configuration for WebRTC store
#[derive(Clone)]
pub struct WebRTCStoreConfig {
    /// Nostr relays for signaling
    pub relays: Vec<String>,
    /// Root hashes to advertise
    pub roots: Vec<Hash>,
    /// Timeout for data requests (ms)
    pub request_timeout_ms: u64,
    /// Interval for sending hello messages (ms)
    pub hello_interval_ms: u64,
    /// Enable verbose logging
    pub debug: bool,
    /// Pool settings for follows and other peers
    pub pools: PoolSettings,
    /// Channel for peer classification (optional)
    /// If None, all peers go to "Other" pool
    pub classifier_tx: Option<ClassifierTx>,
}

impl Default for WebRTCStoreConfig {
    fn default() -> Self {
        Self {
            relays: vec![
                "wss://temp.iris.to".to_string(),
                "wss://relay.damus.io".to_string(),
            ],
            roots: Vec::new(),
            request_timeout_ms: 10000,
            hello_interval_ms: 30000,
            debug: false,
            pools: PoolSettings::default(),
            classifier_tx: None,
        }
    }
}

/// Connection state for a peer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// Initial state
    New,
    /// Connecting via signaling
    Connecting,
    /// WebRTC connection established
    Connected,
    /// Data channel open and ready
    Ready,
    /// Connection failed or closed
    Disconnected,
}

/// Statistics for WebRTC store
#[derive(Debug, Clone, Default)]
pub struct WebRTCStats {
    pub connected_peers: usize,
    pub pending_requests: usize,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub requests_made: u64,
    pub requests_fulfilled: u64,
}

/// Nostr event kind for WebRTC signaling (ephemeral, NIP-17 style)
pub const NOSTR_KIND_HASHTREE: u16 = 25050;

/// Data channel label
pub const DATA_CHANNEL_LABEL: &str = "hashtree";
