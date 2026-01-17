//! WebRTC peer connection management
//!
//! Handles WebRTC connection establishment, data channel communication,
//! and the request/response protocol for hash-based data exchange.
//!
//! Wire protocol (compatible with hashtree-ts):
//! - Request:  [0x00][msgpack: {h: bytes32, htl?: u8}]
//! - Response: [0x01][msgpack: {h: bytes32, d: bytes, i?: u32, n?: u32}]

use crate::protocol::{
    bytes_to_hash, create_request, create_response, create_fragment_response,
    encode_request, encode_response, hash_to_key, is_fragmented, parse_message,
    DataMessage as ProtoMessage, DataResponse, FRAGMENT_SIZE,
};
use crate::types::{
    should_forward, ForwardRequest, ForwardTx, PeerId, PeerHTLConfig, PeerState,
    SignalingMessage, DATA_CHANNEL_LABEL, MAX_HTL,
};
use bytes::Bytes;
use hashtree_core::{Hash, Store};
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, RwLock};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

#[derive(Debug, Error)]
pub enum PeerError {
    #[error("WebRTC error: {0}")]
    WebRTC(#[from] webrtc::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Channel closed")]
    ChannelClosed,
    #[error("Request timeout")]
    Timeout,
    #[error("Peer not ready")]
    NotReady,
    #[error("Data not found")]
    NotFound,
}

/// Default LRU cache sizes (matching hashtree-ts)
const THEIR_REQUESTS_SIZE: usize = 200;

/// Fragment reassembly timeout constants (for future use)
#[allow(dead_code)]
const FRAGMENT_STALL_TIMEOUT_MS: u64 = 5000;
#[allow(dead_code)]
const FRAGMENT_TOTAL_TIMEOUT_MS: u64 = 120000;

/// Pending request awaiting response (requests WE sent)
/// Keyed by hash hex string
struct PendingRequest {
    #[allow(dead_code)] // for debugging
    hash: Hash,
    response_tx: oneshot::Sender<Option<Vec<u8>>>,
}

/// Request this peer sent TO US that we couldn't fulfill locally
/// We track it so we can push data back when/if we get it from another peer
#[derive(Debug, Clone)]
struct TheirRequest {
    /// The hash they requested
    hash: Hash,
    /// When they requested it (for future timeout/cleanup)
    #[allow(dead_code)]
    requested_at: std::time::Instant,
}

/// Fragment reassembly tracking
struct PendingReassembly {
    #[allow(dead_code)] // for debugging
    hash: Hash,
    fragments: HashMap<u32, Vec<u8>>,
    total_expected: u32,
    received_bytes: usize,
    /// For timeout checking (future use)
    #[allow(dead_code)]
    first_fragment_at: std::time::Instant,
    last_fragment_at: std::time::Instant,
}

/// Callback type for forwarding requests to other peers (deprecated, use ForwardTx channel)
/// Parameters: (hash, exclude_peer_id, htl)
/// Returns: data if found, None otherwise
pub type ForwardRequestCallback = Arc<
    dyn Fn(Hash, String, u8) -> futures::future::BoxFuture<'static, Option<Vec<u8>>> + Send + Sync,
>;

/// Forward via channel (preferred over callback)
async fn forward_via_channel(
    forward_tx: &ForwardTx,
    hash: Hash,
    exclude_peer_id: String,
    htl: u8,
) -> Option<Vec<u8>> {
    let (response_tx, response_rx) = oneshot::channel();
    let req = ForwardRequest {
        hash,
        exclude_peer_id,
        htl,
        response: response_tx,
    };

    if forward_tx.send(req).await.is_err() {
        return None;
    }

    response_rx.await.ok().flatten()
}

/// WebRTC peer connection wrapper
///
/// Each Peer is an independent agent that tracks:
/// - `pending_requests`: requests WE sent TO this peer (awaiting response)
/// - `their_requests`: requests THEY sent TO US that we couldn't fulfill
///
/// This matches the hashtree-ts Peer architecture.
/// Wire protocol is binary MessagePack compatible with hashtree-ts.
pub struct Peer<S: Store> {
    /// Remote peer identifier
    pub remote_id: PeerId,
    /// Connection state
    state: Arc<RwLock<PeerState>>,
    /// WebRTC peer connection
    connection: Arc<RTCPeerConnection>,
    /// Data channel (when established)
    data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    /// Pending ICE candidates (before remote description set)
    pending_candidates: Arc<RwLock<Vec<RTCIceCandidateInit>>>,
    /// Requests WE sent TO this peer, keyed by hash hex string
    /// Similar to hashtree-ts: ourRequests = new Map<string, PendingRequest>()
    pending_requests: Arc<RwLock<HashMap<String, PendingRequest>>>,
    /// Requests THEY sent TO US that we couldn't fulfill locally
    /// Keyed by hash hex string, similar to hashtree-ts:
    /// theirRequests = new LRUCache<string, TheirRequest>(THEIR_REQUESTS_SIZE)
    their_requests: Arc<RwLock<LruCache<String, TheirRequest>>>,
    /// Pending fragment reassemblies, keyed by hash hex string
    pending_reassemblies: Arc<RwLock<HashMap<String, PendingReassembly>>>,
    /// Channel for outgoing signaling messages
    signaling_tx: mpsc::Sender<SignalingMessage>,
    /// Local store for responding to requests
    local_store: Arc<S>,
    /// Local peer ID
    local_peer_id: String,
    /// Debug logging enabled
    debug: bool,
    /// Per-peer HTL configuration (Freenet-style probabilistic decrement)
    htl_config: PeerHTLConfig,
    /// Channel to forward request to other peers when we don't have data locally
    forward_tx: Option<ForwardTx>,
    /// Callback to forward request to other peers (deprecated, use forward_tx)
    on_forward_request: Option<ForwardRequestCallback>,
}

impl<S: Store + 'static> Peer<S> {
    /// Create a new peer connection
    pub async fn new(
        remote_id: PeerId,
        local_peer_id: String,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        local_store: Arc<S>,
        debug: bool,
    ) -> Result<Self, PeerError> {
        Self::with_forward_channel(remote_id, local_peer_id, signaling_tx, local_store, debug, None)
            .await
    }

    /// Create a new peer connection with a forwarding channel
    pub async fn with_forward_channel(
        remote_id: PeerId,
        local_peer_id: String,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        local_store: Arc<S>,
        debug: bool,
        forward_tx: Option<ForwardTx>,
    ) -> Result<Self, PeerError> {
        // Create WebRTC API
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        // Configure ICE servers (matches hashtree-ts)
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec![
                    "stun:stun.iris.to:3478".to_string(),
                    "stun:stun.l.google.com:19302".to_string(),
                    "stun:stun.cloudflare.com:3478".to_string(),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let connection = Arc::new(api.new_peer_connection(config).await?);

        let peer = Self {
            remote_id,
            state: Arc::new(RwLock::new(PeerState::New)),
            connection,
            data_channel: Arc::new(RwLock::new(None)),
            pending_candidates: Arc::new(RwLock::new(Vec::new())),
            pending_requests: Arc::new(RwLock::new(HashMap::new())),
            their_requests: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(THEIR_REQUESTS_SIZE).unwrap(),
            ))),
            pending_reassemblies: Arc::new(RwLock::new(HashMap::new())),
            signaling_tx,
            local_store,
            local_peer_id,
            debug,
            htl_config: PeerHTLConfig::random(),
            forward_tx,
            on_forward_request: None,
        };

        peer.setup_handlers().await?;

        Ok(peer)
    }

    /// Setup connection event handlers
    async fn setup_handlers(&self) -> Result<(), PeerError> {
        let state = self.state.clone();
        let data_channel = self.data_channel.clone();
        let pending_requests = self.pending_requests.clone();
        let their_requests = self.their_requests.clone();
        let pending_reassemblies = self.pending_reassemblies.clone();
        let local_store = self.local_store.clone();
        let debug = self.debug;
        let htl_config = self.htl_config;
        let forward_tx = self.forward_tx.clone();
        let on_forward_request = self.on_forward_request.clone();
        let peer_id_str = self.remote_id.to_peer_string();

        // Handle connection state changes
        let state_clone = state.clone();
        self.connection
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                let state = state_clone.clone();
                Box::pin(async move {
                    if debug {
                        println!("[Peer] Connection state changed: {:?}", s);
                    }
                    let mut state = state.write().await;
                    match s {
                        RTCPeerConnectionState::Connected => {
                            *state = PeerState::Connected;
                            if debug {
                                println!("[Peer] Connection established");
                            }
                        }
                        RTCPeerConnectionState::Disconnected
                        | RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed => {
                            *state = PeerState::Disconnected;
                            if debug {
                                println!("[Peer] Connection closed: {:?}", s);
                            }
                        }
                        _ => {}
                    }
                })
            }));

        // Handle incoming data channels
        let data_channel_clone = data_channel.clone();
        let pending_requests_clone = pending_requests.clone();
        let their_requests_clone = their_requests.clone();
        let pending_reassemblies_clone = pending_reassemblies.clone();
        let local_store_clone = local_store.clone();
        let state_clone = state.clone();
        let forward_tx_clone = forward_tx.clone();
        let on_forward_clone = on_forward_request.clone();
        let peer_id_clone = peer_id_str.clone();
        self.connection.on_data_channel(Box::new(move |dc| {
            let data_channel = data_channel_clone.clone();
            let pending_requests = pending_requests_clone.clone();
            let their_requests = their_requests_clone.clone();
            let pending_reassemblies = pending_reassemblies_clone.clone();
            let local_store = local_store_clone.clone();
            let state = state_clone.clone();
            let forward_tx = forward_tx_clone.clone();
            let on_forward = on_forward_clone.clone();
            let peer_id = peer_id_clone.clone();

            Box::pin(async move {
                if dc.label() == DATA_CHANNEL_LABEL {
                    Self::setup_data_channel_handlers(
                        dc.clone(),
                        pending_requests,
                        their_requests,
                        pending_reassemblies,
                        local_store,
                        debug,
                        htl_config,
                        forward_tx,
                        on_forward,
                        peer_id,
                    )
                    .await;
                    *data_channel.write().await = Some(dc);
                    *state.write().await = PeerState::Ready;
                    if debug {
                        println!("[Peer] Data channel opened (incoming)");
                    }
                }
            })
        }));

        // Handle ICE candidates
        let signaling_tx = self.signaling_tx.clone();
        let local_peer_id = self.local_peer_id.clone();
        let remote_id = self.remote_id.to_peer_string();
        self.connection
            .on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
                let signaling_tx = signaling_tx.clone();
                let local_peer_id = local_peer_id.clone();
                let remote_id = remote_id.clone();

                Box::pin(async move {
                    if let Some(candidate) = candidate {
                        let json = candidate.to_json().unwrap();
                        let msg = SignalingMessage::Candidate {
                            peer_id: local_peer_id,
                            target_peer_id: remote_id,
                            candidate: json.candidate,
                            sdp_m_line_index: json.sdp_mline_index,
                            sdp_mid: json.sdp_mid,
                        };
                        let _ = signaling_tx.send(msg).await;
                    }
                })
            }));

        // Handle ICE connection state for debugging
        let debug_clone = debug;
        self.connection
            .on_ice_connection_state_change(Box::new(move |s| {
                if debug_clone {
                    println!("[Peer] ICE connection state: {:?}", s);
                }
                Box::pin(async {})
            }));

        // Handle ICE gathering state for debugging
        let debug_clone2 = debug;
        self.connection
            .on_ice_gathering_state_change(Box::new(move |s| {
                if debug_clone2 {
                    println!("[Peer] ICE gathering state: {:?}", s);
                }
                Box::pin(async {})
            }));

        Ok(())
    }

    /// Setup handlers for a data channel
    /// Uses binary MessagePack protocol compatible with hashtree-ts
    async fn setup_data_channel_handlers(
        dc: Arc<RTCDataChannel>,
        pending_requests: Arc<RwLock<HashMap<String, PendingRequest>>>,
        their_requests: Arc<RwLock<LruCache<String, TheirRequest>>>,
        pending_reassemblies: Arc<RwLock<HashMap<String, PendingReassembly>>>,
        local_store: Arc<S>,
        debug: bool,
        htl_config: PeerHTLConfig,
        forward_tx: Option<ForwardTx>,
        on_forward_request: Option<ForwardRequestCallback>,
        peer_id: String,
    ) {
        let pending_requests_clone = pending_requests.clone();
        let their_requests_clone = their_requests.clone();
        let pending_reassemblies_clone = pending_reassemblies.clone();
        let local_store_clone = local_store.clone();
        let dc_clone = dc.clone();
        let forward_tx_clone = forward_tx.clone();
        let on_forward_clone = on_forward_request.clone();
        let peer_id_clone = peer_id.clone();

        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let pending_requests = pending_requests_clone.clone();
            let their_requests = their_requests_clone.clone();
            let pending_reassemblies = pending_reassemblies_clone.clone();
            let local_store = local_store_clone.clone();
            let dc = dc_clone.clone();
            let forward_tx = forward_tx_clone.clone();
            let on_forward = on_forward_clone.clone();
            let peer_id = peer_id_clone.clone();

            Box::pin(async move {
                let data = msg.data.to_vec();
                if data.is_empty() {
                    return;
                }

                // Parse MessagePack binary protocol
                let parsed = match parse_message(&data) {
                    Some(m) => m,
                    None => {
                        if debug {
                            println!("[Peer] Failed to parse message");
                        }
                        return;
                    }
                };

                match parsed {
                    ProtoMessage::Request(req) => {
                        let htl = req.htl.unwrap_or(MAX_HTL);
                        let hash_key = hash_to_key(&req.h);

                        if debug {
                            println!(
                                "[Peer] Request: hash={}..., htl={}",
                                &hash_key[..16.min(hash_key.len())],
                                htl
                            );
                        }

                        // Convert to Hash type
                        let hash_bytes = match bytes_to_hash(&req.h) {
                            Some(h) => h,
                            None => return,
                        };

                        // Try local store first
                        let local_result = local_store.get(&hash_bytes).await;

                        if let Ok(Some(payload)) = local_result {
                            // Found locally - send response
                            Self::send_response(&dc, &hash_bytes, payload, debug).await;
                            return;
                        }

                        // Not found locally - try forwarding if HTL allows
                        let can_forward = forward_tx.is_some() || on_forward.is_some();
                        if can_forward && should_forward(htl) {
                            // Track request for later push
                            {
                                let mut their_reqs = their_requests.write().await;
                                their_reqs.put(
                                    hash_key.clone(),
                                    TheirRequest {
                                        hash: hash_bytes,
                                        requested_at: std::time::Instant::now(),
                                    },
                                );
                            }

                            // Decrement HTL before forwarding
                            let forward_htl = htl_config.decrement(htl);

                            if debug {
                                println!(
                                    "[Peer] Forwarding request htl={}->{}, hash={}...",
                                    htl,
                                    forward_htl,
                                    &hash_key[..16.min(hash_key.len())]
                                );
                            }

                            // Forward to other peers
                            let forward_result = if let Some(ref tx) = forward_tx {
                                forward_via_channel(tx, hash_bytes, peer_id.clone(), forward_htl)
                                    .await
                            } else if let Some(ref forward_cb) = on_forward {
                                forward_cb(hash_bytes, peer_id.clone(), forward_htl).await
                            } else {
                                None
                            };

                            if let Some(payload) = forward_result {
                                // Got it from another peer
                                their_requests.write().await.pop(&hash_key);
                                Self::send_response(&dc, &hash_bytes, payload, debug).await;

                                if debug {
                                    println!(
                                        "[Peer] Forward success for hash={}...",
                                        &hash_key[..16.min(hash_key.len())]
                                    );
                                }
                                return;
                            }
                        }

                        // Not found - stay silent (hashtree-ts behavior)
                        // Keep in their_requests for potential later push
                        {
                            let mut their_reqs = their_requests.write().await;
                            their_reqs.put(
                                hash_key,
                                TheirRequest {
                                    hash: hash_bytes,
                                    requested_at: std::time::Instant::now(),
                                },
                            );
                        }
                    }
                    ProtoMessage::Response(res) => {
                        let hash_key = hash_to_key(&res.h);

                        // Handle fragmented vs unfragmented responses
                        let final_data = if is_fragmented(&res) {
                            // Fragmented response - reassemble
                            Self::handle_fragment_response(
                                &res,
                                &pending_reassemblies,
                                debug,
                            )
                            .await
                        } else {
                            // Unfragmented response - use directly
                            Some(res.d)
                        };

                        let final_data = match final_data {
                            Some(d) => d,
                            None => return, // Incomplete fragment, wait for more
                        };

                        if debug {
                            println!(
                                "[Peer] Response: hash={}..., size={}",
                                &hash_key[..16.min(hash_key.len())],
                                final_data.len()
                            );
                        }

                        // Resolve pending request
                        let mut requests = pending_requests.write().await;
                        if let Some(request) = requests.remove(&hash_key) {
                            // Verify hash matches
                            let computed_hash = hashtree_core::sha256(&final_data);
                            if computed_hash.to_vec() == res.h {
                                let _ = request.response_tx.send(Some(final_data));
                            } else {
                                if debug {
                                    println!("[Peer] Hash mismatch for response");
                                }
                                let _ = request.response_tx.send(None);
                            }
                        }
                    }
                }
            })
        }));
    }

    /// Send a response (with fragmentation if needed)
    async fn send_response(dc: &Arc<RTCDataChannel>, hash: &Hash, data: Vec<u8>, debug: bool) {
        if data.len() <= FRAGMENT_SIZE {
            // Small enough - send unfragmented
            let res = create_response(hash, data);
            let encoded = encode_response(&res);
            let _ = dc.send(&Bytes::from(encoded)).await;
        } else {
            // Fragment large responses
            let total_fragments = ((data.len() + FRAGMENT_SIZE - 1) / FRAGMENT_SIZE) as u32;
            for i in 0..total_fragments {
                let start = (i as usize) * FRAGMENT_SIZE;
                let end = std::cmp::min(start + FRAGMENT_SIZE, data.len());
                let fragment = data[start..end].to_vec();

                let res = create_fragment_response(hash, fragment, i, total_fragments);
                let encoded = encode_response(&res);
                let _ = dc.send(&Bytes::from(encoded)).await;

                if debug && i == 0 {
                    println!(
                        "[Peer] Sending {} fragments for hash",
                        total_fragments
                    );
                }
            }
        }
    }

    /// Handle a fragmented response - buffer and reassemble
    async fn handle_fragment_response(
        res: &DataResponse,
        pending_reassemblies: &Arc<RwLock<HashMap<String, PendingReassembly>>>,
        debug: bool,
    ) -> Option<Vec<u8>> {
        let hash_key = hash_to_key(&res.h);
        let now = std::time::Instant::now();
        let index = res.i.unwrap();
        let total = res.n.unwrap();

        let mut reassemblies = pending_reassemblies.write().await;

        let pending = reassemblies.entry(hash_key.clone()).or_insert_with(|| {
            let hash = bytes_to_hash(&res.h).unwrap_or([0u8; 32]);
            PendingReassembly {
                hash,
                fragments: HashMap::new(),
                total_expected: total,
                received_bytes: 0,
                first_fragment_at: now,
                last_fragment_at: now,
            }
        });

        // Store fragment if not duplicate
        if !pending.fragments.contains_key(&index) {
            pending.received_bytes += res.d.len();
            pending.fragments.insert(index, res.d.clone());
            pending.last_fragment_at = now;
        }

        // Check if complete
        if pending.fragments.len() == pending.total_expected as usize {
            let total = pending.total_expected;
            let mut assembled = Vec::with_capacity(pending.received_bytes);
            for i in 0..total {
                if let Some(fragment) = pending.fragments.get(&i) {
                    assembled.extend_from_slice(fragment);
                }
            }
            reassemblies.remove(&hash_key);

            if debug {
                println!(
                    "[Peer] Reassembled {} fragments, {} bytes",
                    total,
                    assembled.len()
                );
            }

            Some(assembled)
        } else {
            None // Not yet complete
        }
    }

    /// Initiate connection as offerer
    pub async fn connect(&self) -> Result<(), PeerError> {
        *self.state.write().await = PeerState::Connecting;

        // Create data channel
        // Use unordered for better performance - protocol is stateless (each message self-describes)
        let dc_init = RTCDataChannelInit {
            ordered: Some(false),
            ..Default::default()
        };
        let dc = self
            .connection
            .create_data_channel(DATA_CHANNEL_LABEL, Some(dc_init))
            .await?;

        Self::setup_data_channel_handlers(
            dc.clone(),
            self.pending_requests.clone(),
            self.their_requests.clone(),
            self.pending_reassemblies.clone(),
            self.local_store.clone(),
            self.debug,
            self.htl_config,
            self.forward_tx.clone(),
            self.on_forward_request.clone(),
            self.remote_id.to_peer_string(),
        )
        .await;

        let data_channel = self.data_channel.clone();
        let state = self.state.clone();
        let debug = self.debug;
        dc.on_open(Box::new(move || {
            let _data_channel = data_channel.clone();
            let state = state.clone();

            Box::pin(async move {
                *state.write().await = PeerState::Ready;
                if debug {
                    println!("[Peer] Data channel opened (outgoing)");
                }
            })
        }));

        *self.data_channel.write().await = Some(dc);

        // Create and send offer
        let offer = self.connection.create_offer(None).await?;
        self.connection.set_local_description(offer.clone()).await?;

        let msg = SignalingMessage::Offer {
            peer_id: self.local_peer_id.clone(),
            target_peer_id: self.remote_id.to_peer_string(),
            sdp: offer.sdp,
        };
        self.signaling_tx
            .send(msg)
            .await
            .map_err(|_| PeerError::ChannelClosed)?;

        Ok(())
    }

    /// Handle incoming signaling message
    pub async fn handle_signaling(&self, msg: SignalingMessage) -> Result<(), PeerError> {
        match msg {
            SignalingMessage::Offer { sdp, .. } => {
                if self.debug {
                    println!("[Peer] Received offer, setting remote description");
                }
                let offer = RTCSessionDescription::offer(sdp)?;
                self.connection.set_remote_description(offer).await?;

                // Add any pending candidates
                let candidates = self.pending_candidates.write().await.drain(..).collect::<Vec<_>>();
                if self.debug && !candidates.is_empty() {
                    println!("[Peer] Adding {} pending candidates after offer", candidates.len());
                }
                for candidate in candidates {
                    self.connection.add_ice_candidate(candidate).await?;
                }

                // Create and send answer
                let answer = self.connection.create_answer(None).await?;
                self.connection.set_local_description(answer.clone()).await?;

                let msg = SignalingMessage::Answer {
                    peer_id: self.local_peer_id.clone(),
                    target_peer_id: self.remote_id.to_peer_string(),
                    sdp: answer.sdp,
                };
                self.signaling_tx
                    .send(msg)
                    .await
                    .map_err(|_| PeerError::ChannelClosed)?;

                *self.state.write().await = PeerState::Connecting;
            }
            SignalingMessage::Answer { sdp, .. } => {
                if self.debug {
                    println!("[Peer] Received answer, setting remote description");
                }
                let answer = RTCSessionDescription::answer(sdp)?;
                self.connection.set_remote_description(answer).await?;

                // Add any pending candidates
                let candidates = self.pending_candidates.write().await.drain(..).collect::<Vec<_>>();
                if self.debug && !candidates.is_empty() {
                    println!("[Peer] Adding {} pending candidates after answer", candidates.len());
                }
                for candidate in candidates {
                    self.connection.add_ice_candidate(candidate).await?;
                }
            }
            SignalingMessage::Candidate {
                candidate,
                sdp_m_line_index,
                sdp_mid,
                ..
            } => {
                let init = RTCIceCandidateInit {
                    candidate: candidate.clone(),
                    sdp_mid,
                    sdp_mline_index: sdp_m_line_index,
                    ..Default::default()
                };

                // Check if remote description is set
                if self.connection.remote_description().await.is_some() {
                    if self.debug {
                        println!("[Peer] Adding ICE candidate: {}...", &candidate[..candidate.len().min(50)]);
                    }
                    self.connection.add_ice_candidate(init).await?;
                } else {
                    if self.debug {
                        println!("[Peer] Queueing ICE candidate (no remote description yet)");
                    }
                    self.pending_candidates.write().await.push(init);
                }
            }
            SignalingMessage::Candidates { candidates, .. } => {
                for c in candidates {
                    let init = RTCIceCandidateInit {
                        candidate: c.candidate,
                        sdp_mid: c.sdp_mid,
                        sdp_mline_index: c.sdp_m_line_index,
                        ..Default::default()
                    };

                    if self.connection.remote_description().await.is_some() {
                        self.connection.add_ice_candidate(init).await?;
                    } else {
                        self.pending_candidates.write().await.push(init);
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Request data by hash with default HTL
    pub async fn request(&self, hash: &Hash) -> Result<Option<Vec<u8>>, PeerError> {
        self.request_with_htl(hash, MAX_HTL).await
    }

    /// Request data by hash with specified HTL
    /// Uses binary MessagePack protocol compatible with hashtree-ts
    pub async fn request_with_htl(&self, hash: &Hash, htl: u8) -> Result<Option<Vec<u8>>, PeerError> {
        let state = *self.state.read().await;
        if state != PeerState::Ready {
            return Err(PeerError::NotReady);
        }

        let dc = self.data_channel.read().await;
        let dc = dc.as_ref().ok_or(PeerError::NotReady)?;

        let hash_key = hash_to_key(hash);

        // Check if we already have a pending request for this hash
        {
            let requests = self.pending_requests.read().await;
            if requests.contains_key(&hash_key) {
                // Already requesting this hash - wait for it
                drop(requests);
                // Could implement deduplication here, but for now just proceed
            }
        }

        // Setup response channel
        let (tx, rx) = oneshot::channel();
        self.pending_requests.write().await.insert(
            hash_key.clone(),
            PendingRequest {
                hash: *hash,
                response_tx: tx,
            },
        );

        // Send request as binary MessagePack
        // Decrement HTL using our per-peer config before sending
        let send_htl = self.htl_config.decrement(htl);
        let req = create_request(hash, send_htl);
        let encoded = encode_request(&req);
        dc.send(&Bytes::from(encoded)).await?;

        if self.debug {
            println!(
                "[Peer] Sent request: htl={}, hash={}...",
                send_htl,
                &hash_key[..16.min(hash_key.len())]
            );
        }

        // Wait for response with timeout
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => Err(PeerError::ChannelClosed),
            Err(_) => {
                // Remove pending request on timeout
                self.pending_requests.write().await.remove(&hash_key);
                Err(PeerError::Timeout)
            }
        }
    }

    /// Send data response using binary MessagePack protocol
    /// Note: For found data, use the internal fragmentation-aware send_response
    /// This method is kept for API compatibility but now uses binary protocol
    pub async fn send_response_for_hash(
        &self,
        hash: &Hash,
        data: Option<&[u8]>,
    ) -> Result<(), PeerError> {
        let dc = self.data_channel.read().await;
        let dc = dc.as_ref().ok_or(PeerError::NotReady)?;

        if let Some(payload) = data {
            // Use the internal helper with fragmentation support
            Self::send_response(dc, hash, payload.to_vec(), self.debug).await;
        }
        // Note: In the new protocol, "not found" is implicit (no response sent)
        // If needed, we could send an empty response

        Ok(())
    }

    /// Get current connection state
    pub async fn state(&self) -> PeerState {
        *self.state.read().await
    }

    /// Close the connection
    pub async fn close(&self) -> Result<(), PeerError> {
        self.connection.close().await?;
        *self.state.write().await = PeerState::Disconnected;
        Ok(())
    }

    /// Set the forward request callback
    /// Called when this peer requests data we don't have locally
    /// Parameters: (hash, exclude_peer_id, htl)
    pub fn set_on_forward_request<F>(&mut self, callback: F)
    where
        F: Fn(Hash, String, u8) -> futures::future::BoxFuture<'static, Option<Vec<u8>>> + Send + Sync + 'static,
    {
        self.on_forward_request = Some(Arc::new(callback));
    }

    /// Get the peer's HTL config (for testing)
    pub fn htl_config(&self) -> PeerHTLConfig {
        self.htl_config
    }

    /// Send data to this peer for a hash they previously requested
    /// Returns true if this peer had requested this hash
    pub async fn send_data(&self, hash_hex: &str, data: &[u8]) -> Result<bool, PeerError> {
        let their_req = {
            let mut requests = self.their_requests.write().await;
            requests.pop(hash_hex)
        };

        let Some(their_req) = their_req else {
            return Ok(false);
        };

        let dc = self.data_channel.read().await;
        let dc = dc.as_ref().ok_or(PeerError::NotReady)?;

        // Send data response using binary MessagePack protocol with fragmentation
        Self::send_response(dc, &their_req.hash, data.to_vec(), self.debug).await;

        if self.debug {
            println!("[Peer] Sent data for hash: {}...", &hash_hex[..16.min(hash_hex.len())]);
        }

        Ok(true)
    }

    /// Check if this peer has requested a hash
    pub async fn has_requested(&self, hash_hex: &str) -> bool {
        self.their_requests.read().await.peek(hash_hex).is_some()
    }

    /// Get count of pending requests from this peer
    pub async fn their_request_count(&self) -> usize {
        self.their_requests.read().await.len()
    }

    /// Get count of pending requests we sent to this peer
    pub async fn our_request_count(&self) -> usize {
        self.pending_requests.read().await.len()
    }
}
