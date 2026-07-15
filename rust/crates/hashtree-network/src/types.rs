//! Mesh transport types for peer-to-peer data exchange.
//!
//! Defines signaling frames, negotiated peer-link messages, and shared mesh
//! constants used across Nostr websocket transports, local buses, direct-link
//! transports, and simulation.

use hashtree_core::Hash;
use nostr_sdk::nostr::{Event, Kind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio::time::Instant;

use crate::mesh_store_core::{PubsubDeliveryMode, RequestDispatchConfig};
use crate::peer_selector::SelectionStrategy;

fn default_hash_get_enabled() -> bool {
    true
}

/// Unique identifier for a peer in the network
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId {
    /// Nostr public key / endpoint identity
    pub pubkey: String,
}

impl PeerId {
    pub fn new(pubkey: String) -> Self {
        Self { pubkey }
    }

    pub fn to_peer_string(&self) -> String {
        self.pubkey.clone()
    }

    pub fn from_peer_string(s: &str) -> Option<Self> {
        let pubkey = s.trim();
        if pubkey.is_empty() || pubkey.contains(':') {
            return None;
        }
        Some(Self {
            pubkey: pubkey.to_string(),
        })
    }

    pub fn from_string(s: &str) -> Option<Self> {
        Self::from_peer_string(s)
    }

    pub fn short(&self) -> String {
        self.pubkey[..8.min(self.pubkey.len())].to_string()
    }
}

/// Persisted private direct-link signaling hints for a stable peer identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPeerRecord {
    pub peer_id: String,
    #[serde(
        default,
        alias = "direct_urls",
        alias = "addresses",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub signal_urls: Vec<String>,
    #[serde(default)]
    pub last_seen_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_source: Option<String>,
}

/// Snapshot of private peer signaling hints learned over established peer links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPeerSnapshot {
    pub version: u32,
    #[serde(default)]
    pub peers: Vec<KnownPeerRecord>,
}

impl Default for KnownPeerSnapshot {
    fn default() -> Self {
        Self {
            version: 1,
            peers: Vec::new(),
        }
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.pubkey)
    }
}

/// Signaling message types exchanged over mesh signaling transports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    /// Initial hello message to discover peers
    #[serde(rename = "hello")]
    Hello {
        #[serde(rename = "peerId")]
        peer_id: String,
        roots: Vec<String>,
        #[serde(rename = "hashGet", default = "default_hash_get_enabled")]
        hash_get: bool,
    },

    /// Opaque negotiation offer payload.
    ///
    /// Serialized as `sdp` for compatibility with older signaling envelopes.
    #[serde(rename = "offer")]
    Offer {
        #[serde(rename = "peerId")]
        peer_id: String,
        #[serde(rename = "targetPeerId")]
        target_peer_id: String,
        sdp: String,
    },

    /// Opaque negotiation answer payload.
    ///
    /// Serialized as `sdp` for compatibility with older signaling envelopes.
    #[serde(rename = "answer")]
    Answer {
        #[serde(rename = "peerId")]
        peer_id: String,
        #[serde(rename = "targetPeerId")]
        target_peer_id: String,
        sdp: String,
    },

    /// Single candidate update for transports that need trickle negotiation.
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

    /// Batched candidate updates.
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
    /// Get the message type label.
    pub fn msg_type(&self) -> &str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Offer { .. } => "offer",
            Self::Answer { .. } => "answer",
            Self::Candidate { .. } => "candidate",
            Self::Candidates { .. } => "candidates",
        }
    }

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

/// Shared concurrent-offer negotiation: determine if we are the "polite" peer.
///
/// In this negotiation flow, both peers can send offers simultaneously.
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

/// Candidate metadata for transports that use ICE-style negotiation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMLineIndex")]
    pub sdp_m_line_index: Option<u16>,
    #[serde(rename = "sdpMid")]
    pub sdp_mid: Option<String>,
}

/// Direct peer-link message types for hash-based data exchange.
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

/// Default HTL (Hops To Live) budget for mesh blob retrieval.
pub const MAX_HTL: u8 = hashtree_core::BLOB_DEFAULT_HTL;
/// Probability to decrement at max HTL (50%)
pub const DECREMENT_AT_MAX_PROB: f64 = 0.5;
/// Probability to decrement at min HTL=1 (25%)
pub const DECREMENT_AT_MIN_PROB: f64 = 0.25;

/// HTL decrement mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtlMode {
    Probabilistic,
}

/// HTL policy parameters for a traffic class.
#[derive(Debug, Clone, Copy)]
pub struct HtlPolicy {
    pub mode: HtlMode,
    pub max_htl: u8,
    pub p_at_max: f64,
    pub p_at_min: f64,
}

/// Default policy for blob/content requests.
pub const BLOB_REQUEST_POLICY: HtlPolicy = HtlPolicy {
    mode: HtlMode::Probabilistic,
    max_htl: MAX_HTL,
    p_at_max: DECREMENT_AT_MAX_PROB,
    p_at_min: DECREMENT_AT_MIN_PROB,
};

/// Policy for mesh signaling/event forwarding.
pub const MESH_EVENT_POLICY: HtlPolicy = HtlPolicy {
    mode: HtlMode::Probabilistic,
    max_htl: 4,
    p_at_max: 0.75,
    p_at_min: 0.5,
};

/// Per-peer HTL configuration (Freenet-style probabilistic decrement)
/// Generated once per peer connection, stays fixed for connection lifetime
#[derive(Debug, Clone, Copy)]
pub struct PeerHTLConfig {
    /// Random sample used to decide decrement at max HTL.
    pub at_max_sample: f64,
    /// Random sample used to decide decrement at min HTL.
    pub at_min_sample: f64,
}

impl PeerHTLConfig {
    /// Generate random HTL config for a new peer connection
    pub fn random() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        Self::from_samples(rng.gen_range(0.0..1.0), rng.gen_range(0.0..1.0))
    }

    /// Construct config from explicit random samples.
    pub fn from_samples(at_max_sample: f64, at_min_sample: f64) -> Self {
        Self {
            at_max_sample: at_max_sample.clamp(0.0, 1.0),
            at_min_sample: at_min_sample.clamp(0.0, 1.0),
        }
    }

    /// Construct config from explicit decrement choices.
    pub fn from_flags(decrement_at_max: bool, decrement_at_min: bool) -> Self {
        Self {
            at_max_sample: if decrement_at_max { 0.0 } else { 1.0 },
            at_min_sample: if decrement_at_min { 0.0 } else { 1.0 },
        }
    }

    /// Decrement HTL using this peer's config (Freenet-style probabilistic)
    /// Called when SENDING to a peer, not on receive
    pub fn decrement(&self, htl: u8) -> u8 {
        decrement_htl_with_policy(htl, &BLOB_REQUEST_POLICY, self)
    }

    /// Decrement HTL using the provided traffic policy.
    pub fn decrement_with_policy(&self, htl: u8, policy: &HtlPolicy) -> u8 {
        decrement_htl_with_policy(htl, policy, self)
    }
}

impl Default for PeerHTLConfig {
    fn default() -> Self {
        Self::random()
    }
}

/// Decrement HTL according to policy and per-peer randomness profile.
pub fn decrement_htl_with_policy(htl: u8, policy: &HtlPolicy, config: &PeerHTLConfig) -> u8 {
    let htl = htl.min(policy.max_htl);
    if htl == 0 {
        return 0;
    }

    match policy.mode {
        HtlMode::Probabilistic => {
            let p_at_max = policy.p_at_max.clamp(0.0, 1.0);
            let p_at_min = policy.p_at_min.clamp(0.0, 1.0);

            if htl == policy.max_htl {
                return if config.at_max_sample < p_at_max {
                    htl.saturating_sub(1)
                } else {
                    htl
                };
            }

            if htl == 1 {
                return if config.at_min_sample < p_at_min {
                    0
                } else {
                    htl
                };
            }

            htl.saturating_sub(1)
        }
    }
}

/// Check if a request should be forwarded based on HTL.
pub fn should_forward_htl(htl: u8) -> bool {
    htl > 0
}

/// Backward-compatible helper.
pub fn should_forward(htl: u8) -> bool {
    should_forward_htl(htl)
}

use tokio::sync::{mpsc, oneshot};

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
            max_connections: 16,
            satisfied_connections: 8,
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
                max_connections: 16,
                satisfied_connections: 8,
            },
            other: PoolConfig {
                max_connections: 16,
                satisfied_connections: 8,
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

/// Configuration for the default mesh-backed store composition.
#[derive(Clone)]
pub struct MeshStoreConfig {
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
    /// Retrieval peer selection strategy (shared with MeshStoreCore/CLI/sim).
    pub request_selection_strategy: SelectionStrategy,
    /// Whether fairness constraints are enabled for retrieval selection.
    pub request_fairness_enabled: bool,
    /// Hedged request dispatch policy for retrieval.
    pub request_dispatch: RequestDispatchConfig,
    /// Pubsub delivery strategy used by the production mesh store.
    pub pubsub_delivery_mode: PubsubDeliveryMode,
    /// Upstream Blossom servers used as adaptive fallback read sources.
    pub upstream_blossom_servers: Vec<String>,
}

impl Default for MeshStoreConfig {
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
            request_selection_strategy: SelectionStrategy::Weighted,
            request_fairness_enabled: true,
            request_dispatch: RequestDispatchConfig {
                initial_fanout: 2,
                hedge_fanout: 1,
                max_fanout: 8,
                hedge_interval_ms: 120,
            },
            pubsub_delivery_mode: PubsubDeliveryMode::HtlInvWant,
            upstream_blossom_servers: Vec::new(),
        }
    }
}

/// Connection state for a peer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// Initial state
    New,
    /// Connecting via signaling.
    Connecting,
    /// Negotiated direct link established.
    Connected,
    /// Direct peer link open and ready for data.
    Ready,
    /// Connection failed or closed
    Disconnected,
}

/// Statistics for the default mesh-backed store composition.
#[derive(Debug, Clone, Default)]
pub struct MeshStats {
    pub connected_peers: usize,
    pub pending_requests: usize,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub requests_made: u64,
    pub requests_fulfilled: u64,
}

/// Nostr event kind for hashtree signaling envelopes (ephemeral, NIP-17 style).
pub const NOSTR_KIND_HASHTREE: u16 = 25050;
pub const MESH_SIGNALING_EVENT_KIND: u16 = NOSTR_KIND_HASHTREE;

/// Relayless mesh protocol constants for forwarding signaling events over data channels.
pub const MESH_PROTOCOL: &str = "htree.nostr.mesh.v1";
pub const MESH_PROTOCOL_VERSION: u8 = 1;
pub const MESH_DEFAULT_HTL: u8 = MESH_EVENT_POLICY.max_htl;
pub const MESH_MAX_HTL: u8 = 6;

/// TTL/capacity dedupe structure for forwarded mesh frames/events.
#[derive(Debug)]
pub struct TimedSeenSet {
    entries: HashMap<String, Instant>,
    order: VecDeque<(String, Instant)>,
    ttl: Duration,
    capacity: usize,
}

impl TimedSeenSet {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            ttl,
            capacity,
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some((key, inserted_at)) = self.order.front().cloned() {
            if now.duration_since(inserted_at) < self.ttl {
                break;
            }
            self.order.pop_front();
            if self
                .entries
                .get(&key)
                .map(|ts| *ts == inserted_at)
                .unwrap_or(false)
            {
                self.entries.remove(&key);
            }
        }

        while self.entries.len() > self.capacity {
            if let Some((key, inserted_at)) = self.order.pop_front() {
                if self
                    .entries
                    .get(&key)
                    .map(|ts| *ts == inserted_at)
                    .unwrap_or(false)
                {
                    self.entries.remove(&key);
                }
            } else {
                break;
            }
        }
    }

    pub fn insert_if_new(&mut self, key: String) -> bool {
        let now = Instant::now();
        self.prune(now);
        if self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key.clone(), now);
        self.order.push_back((key, now));
        self.prune(now);
        true
    }

    pub fn contains(&mut self, key: &str) -> bool {
        let now = Instant::now();
        self.prune(now);
        self.entries.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MeshNostrPayload {
    #[serde(rename = "EVENT")]
    Event { event: Event },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshNostrFrame {
    pub protocol: String,
    pub version: u8,
    pub frame_id: String,
    pub htl: u8,
    pub sender_peer_id: String,
    pub payload: MeshNostrPayload,
}

impl MeshNostrFrame {
    pub fn new_event(event: Event, sender_peer_id: &str, htl: u8) -> Self {
        Self::new_event_with_id(
            event,
            sender_peer_id,
            &uuid::Uuid::new_v4().to_string(),
            htl,
        )
    }

    pub fn new_event_with_id(event: Event, sender_peer_id: &str, frame_id: &str, htl: u8) -> Self {
        Self {
            protocol: MESH_PROTOCOL.to_string(),
            version: MESH_PROTOCOL_VERSION,
            frame_id: frame_id.to_string(),
            htl,
            sender_peer_id: sender_peer_id.to_string(),
            payload: MeshNostrPayload::Event { event },
        }
    }

    pub fn event(&self) -> &Event {
        match &self.payload {
            MeshNostrPayload::Event { event } => event,
        }
    }
}

pub fn validate_mesh_frame(frame: &MeshNostrFrame) -> Result<(), &'static str> {
    if frame.protocol != MESH_PROTOCOL {
        return Err("invalid protocol");
    }
    if frame.version != MESH_PROTOCOL_VERSION {
        return Err("invalid version");
    }
    if frame.frame_id.is_empty() {
        return Err("missing frame id");
    }
    if frame.sender_peer_id.is_empty() {
        return Err("missing sender peer id");
    }
    if frame.sender_peer_id.contains(':') {
        return Err("invalid sender peer id");
    }
    if frame.htl == 0 || frame.htl > MESH_MAX_HTL {
        return Err("invalid htl");
    }

    let event = frame.event();
    if event.kind != Kind::Custom(NOSTR_KIND_HASHTREE)
        && event.kind != Kind::from_u16(NOSTR_KIND_HASHTREE)
    {
        return Err("unsupported event kind");
    }

    Ok(())
}
