//! WebRTC peer connection for hashtree data exchange

use anyhow::Result;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use super::types::{DataMessage, DataRequest, DataResponse, PeerDirection, PeerId, PeerStateEvent, SignalingMessage, encode_message, encode_request, encode_response, parse_message, hash_to_hex};

/// Trait for content storage that can be used by WebRTC peers
pub trait ContentStore: Send + Sync + 'static {
    /// Get content by hex hash
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>>;
}

/// Pending request tracking (keyed by hash hex)
pub struct PendingRequest {
    pub hash: Vec<u8>,
    pub response_tx: oneshot::Sender<Option<Vec<u8>>>,
}

/// WebRTC peer connection with data channel protocol
pub struct Peer {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub created_at: std::time::Instant,
    pub connected_at: Option<std::time::Instant>,

    pc: Arc<RTCPeerConnection>,
    /// Data channel - can be set from callback when receiving channel from peer
    pub data_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    signaling_tx: mpsc::Sender<SignalingMessage>,
    my_peer_id: PeerId,

    // Content store for serving requests
    store: Option<Arc<dyn ContentStore>>,

    // Track pending outgoing requests (keyed by hash hex)
    pub pending_requests: Arc<Mutex<HashMap<String, PendingRequest>>>,

    // Channel for incoming data messages
    #[allow(dead_code)]
    message_tx: mpsc::Sender<(DataMessage, Option<Vec<u8>>)>,
    #[allow(dead_code)]
    message_rx: Option<mpsc::Receiver<(DataMessage, Option<Vec<u8>>)>>,

    // Optional channel to notify signaling layer of state changes
    state_event_tx: Option<mpsc::Sender<PeerStateEvent>>,
}

impl Peer {
    /// Create a new peer connection
    pub async fn new(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
    ) -> Result<Self> {
        Self::new_with_store_and_events(peer_id, direction, my_peer_id, signaling_tx, stun_servers, None, None).await
    }

    /// Create a new peer connection with content store
    pub async fn new_with_store(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
        store: Option<Arc<dyn ContentStore>>,
    ) -> Result<Self> {
        Self::new_with_store_and_events(peer_id, direction, my_peer_id, signaling_tx, stun_servers, store, None).await
    }

    /// Create a new peer connection with content store and state event channel
    pub async fn new_with_store_and_events(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
        store: Option<Arc<dyn ContentStore>>,
        state_event_tx: Option<mpsc::Sender<PeerStateEvent>>,
    ) -> Result<Self> {
        // Create WebRTC API
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;

        // Enable mDNS temporarily for debugging
        // Previously disabled due to https://github.com/webrtc-rs/webrtc/issues/616
        let setting_engine = SettingEngine::default();
        // Note: mDNS enabled by default

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        // Configure ICE servers
        let ice_servers: Vec<RTCIceServer> = stun_servers
            .iter()
            .map(|url| RTCIceServer {
                urls: vec![url.clone()],
                ..Default::default()
            })
            .collect();

        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);
        let (message_tx, message_rx) = mpsc::channel(100);
        Ok(Self {
            peer_id,
            direction,
            created_at: std::time::Instant::now(),
            connected_at: None,
            pc,
            data_channel: Arc::new(Mutex::new(None)),
            signaling_tx,
            my_peer_id,
            store,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            message_tx,
            message_rx: Some(message_rx),
            state_event_tx,
        })
    }

    /// Set content store
    pub fn set_store(&mut self, store: Arc<dyn ContentStore>) {
        self.store = Some(store);
    }

    /// Get connection state
    pub fn state(&self) -> RTCPeerConnectionState {
        self.pc.connection_state()
    }

    /// Get signaling state
    pub fn signaling_state(&self) -> webrtc::peer_connection::signaling_state::RTCSignalingState {
        self.pc.signaling_state()
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        self.pc.connection_state() == RTCPeerConnectionState::Connected
    }

    /// Setup event handlers for the peer connection
    pub async fn setup_handlers(&mut self) -> Result<()> {
        let peer_id = self.peer_id.clone();
        let signaling_tx = self.signaling_tx.clone();
        let my_peer_id_str = self.my_peer_id.to_string();
        let recipient = self.peer_id.to_string();

        // Handle ICE candidates - work MUST be inside the returned future
        self.pc
            .on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
                let signaling_tx = signaling_tx.clone();
                let my_peer_id_str = my_peer_id_str.clone();
                let recipient = recipient.clone();

                Box::pin(async move {
                    if let Some(c) = candidate {
                        if let Some(init) = c.to_json().ok() {
                            info!("ICE candidate generated: {}", &init.candidate[..init.candidate.len().min(60)]);
                            let msg = SignalingMessage::candidate(
                                serde_json::to_value(&init).unwrap_or_default(),
                                &recipient,
                                &my_peer_id_str,
                            );
                            if let Err(e) = signaling_tx.send(msg).await {
                                error!("Failed to send ICE candidate: {}", e);
                            }
                        }
                    }
                })
            }));

        // Handle connection state changes - work MUST be inside the returned future
        let peer_id_log = peer_id.clone();
        let state_event_tx = self.state_event_tx.clone();
        self.pc
            .on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
                let peer_id = peer_id_log.clone();
                let state_event_tx = state_event_tx.clone();
                Box::pin(async move {
                    info!("Peer {} connection state: {:?}", peer_id.short(), state);

                    // Notify signaling layer of state changes
                    if let Some(tx) = state_event_tx {
                        let event = match state {
                            RTCPeerConnectionState::Connected => Some(PeerStateEvent::Connected(peer_id)),
                            RTCPeerConnectionState::Failed => Some(PeerStateEvent::Failed(peer_id)),
                            RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Closed => {
                                Some(PeerStateEvent::Disconnected(peer_id))
                            }
                            _ => None,
                        };
                        if let Some(event) = event {
                            if let Err(e) = tx.send(event).await {
                                error!("Failed to send peer state event: {}", e);
                            }
                        }
                    }
                })
            }));

        Ok(())
    }

    /// Initiate connection (create offer) - for outbound connections
    pub async fn connect(&mut self) -> Result<serde_json::Value> {
        println!("[Peer {}] Creating data channel...", self.peer_id.short());
        // Create data channel first
        // Use unordered for better performance - protocol is stateless (each message self-describes)
        let dc_init = RTCDataChannelInit {
            ordered: Some(false),
            ..Default::default()
        };
        let dc = self.pc.create_data_channel("hashtree", Some(dc_init)).await?;
        println!("[Peer {}] Data channel created, setting up handlers...", self.peer_id.short());
        self.setup_data_channel(dc.clone()).await?;
        println!("[Peer {}] Handlers set up, storing data channel...", self.peer_id.short());
        {
            let mut dc_guard = self.data_channel.lock().await;
            *dc_guard = Some(dc);
        }
        println!("[Peer {}] Data channel stored", self.peer_id.short());

        // Create offer and wait for ICE gathering to complete
        // This ensures all ICE candidates are embedded in the SDP
        let offer = self.pc.create_offer(None).await?;
        let mut gathering_complete = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(offer).await?;

        // Wait for ICE gathering to complete (with timeout)
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gathering_complete.recv()
        ).await;

        // Get the local description with ICE candidates embedded
        let local_desc = self.pc.local_description().await
            .ok_or_else(|| anyhow::anyhow!("No local description after gathering"))?;

        debug!("Offer created, SDP len: {}, ice_gathering: {:?}",
            local_desc.sdp.len(), self.pc.ice_gathering_state());

        // Return offer as JSON
        let offer_json = serde_json::json!({
            "type": local_desc.sdp_type.to_string().to_lowercase(),
            "sdp": local_desc.sdp
        });

        Ok(offer_json)
    }

    /// Handle incoming offer and create answer
    pub async fn handle_offer(&mut self, offer: serde_json::Value) -> Result<serde_json::Value> {
        let sdp = offer
            .get("sdp")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing SDP in offer"))?;

        // Setup data channel handler BEFORE set_remote_description
        // This ensures the handler is registered before any data channel events fire
        let peer_id = self.peer_id.clone();
        let message_tx = self.message_tx.clone();
        let pending_requests = self.pending_requests.clone();
        let store = self.store.clone();
        let data_channel_holder = self.data_channel.clone();

        self.pc
            .on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let peer_id = peer_id.clone();
                let message_tx = message_tx.clone();
                let pending_requests = pending_requests.clone();
                let store = store.clone();
                let data_channel_holder = data_channel_holder.clone();

                // Work MUST be inside the returned future
                Box::pin(async move {
                    info!("Peer {} received data channel: {}", peer_id.short(), dc.label());

                    // Store the received data channel
                    {
                        let mut dc_guard = data_channel_holder.lock().await;
                        *dc_guard = Some(dc.clone());
                    }

                    // Set up message handlers
                    Self::setup_dc_handlers(
                        dc.clone(),
                        peer_id,
                        message_tx,
                        pending_requests,
                        store,
                    )
                    .await;
                })
            }));

        // Set remote description after handler is registered
        let offer_desc = RTCSessionDescription::offer(sdp.to_string())?;
        self.pc.set_remote_description(offer_desc).await?;

        // Create answer and wait for ICE gathering to complete
        // This ensures all ICE candidates are embedded in the SDP
        let answer = self.pc.create_answer(None).await?;
        let mut gathering_complete = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(answer).await?;

        // Wait for ICE gathering to complete (with timeout)
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gathering_complete.recv()
        ).await;

        // Get the local description with ICE candidates embedded
        let local_desc = self.pc.local_description().await
            .ok_or_else(|| anyhow::anyhow!("No local description after gathering"))?;

        debug!("Answer created, SDP len: {}, ice_gathering: {:?}",
            local_desc.sdp.len(), self.pc.ice_gathering_state());

        let answer_json = serde_json::json!({
            "type": local_desc.sdp_type.to_string().to_lowercase(),
            "sdp": local_desc.sdp
        });

        Ok(answer_json)
    }

    /// Handle incoming answer
    pub async fn handle_answer(&mut self, answer: serde_json::Value) -> Result<()> {
        let sdp = answer
            .get("sdp")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing SDP in answer"))?;

        let answer_desc = RTCSessionDescription::answer(sdp.to_string())?;
        self.pc.set_remote_description(answer_desc).await?;

        Ok(())
    }

    /// Handle incoming ICE candidate
    pub async fn handle_candidate(&mut self, candidate: serde_json::Value) -> Result<()> {
        let candidate_str = candidate
            .get("candidate")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        let sdp_mid = candidate
            .get("sdpMid")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());

        let sdp_mline_index = candidate
            .get("sdpMLineIndex")
            .and_then(|i| i.as_u64())
            .map(|i| i as u16);

        if !candidate_str.is_empty() {
            use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
            let init = RTCIceCandidateInit {
                candidate: candidate_str.to_string(),
                sdp_mid,
                sdp_mline_index,
                username_fragment: candidate
                    .get("usernameFragment")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string()),
            };
            self.pc.add_ice_candidate(init).await?;
        }

        Ok(())
    }

    /// Setup data channel handlers
    async fn setup_data_channel(&mut self, dc: Arc<RTCDataChannel>) -> Result<()> {
        let peer_id = self.peer_id.clone();
        let message_tx = self.message_tx.clone();
        let pending_requests = self.pending_requests.clone();
        let store = self.store.clone();

        Self::setup_dc_handlers(dc, peer_id, message_tx, pending_requests, store).await;
        Ok(())
    }

    /// Setup handlers for a data channel (shared between outbound and inbound)
    async fn setup_dc_handlers(
        dc: Arc<RTCDataChannel>,
        peer_id: PeerId,
        message_tx: mpsc::Sender<(DataMessage, Option<Vec<u8>>)>,
        pending_requests: Arc<Mutex<HashMap<String, PendingRequest>>>,
        store: Option<Arc<dyn ContentStore>>,
    ) {
        let label = dc.label().to_string();
        let peer_short = peer_id.short();

        // Track pending binary data (request_id -> expected after response)
        let _pending_binary: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let _dc_for_open = dc.clone();
        let peer_short_open = peer_short.clone();
        let label_clone = label.clone();
        dc.on_open(Box::new(move || {
            let peer_short_open = peer_short_open.clone();
            let label_clone = label_clone.clone();
            // Work MUST be inside the returned future
            Box::pin(async move {
                info!("[Peer {}] Data channel '{}' open", peer_short_open, label_clone);
            })
        }));

        let dc_for_msg = dc.clone();
        let peer_short_msg = peer_short.clone();
        let _pending_binary_clone = _pending_binary.clone();
        let store_clone = store.clone();

        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let dc = dc_for_msg.clone();
            let peer_short = peer_short_msg.clone();
            let pending_requests = pending_requests.clone();
            let _pending_binary = _pending_binary_clone.clone();
            let _message_tx = message_tx.clone();
            let store = store_clone.clone();
            let msg_data = msg.data.clone();

            // Work MUST be inside the returned future
            Box::pin(async move {
                // All messages are binary with type prefix + MessagePack body
                debug!("[Peer {}] Received {} bytes on data channel", peer_short, msg_data.len());
                match parse_message(&msg_data) {
                    Ok(data_msg) => match data_msg {
                        DataMessage::Request(req) => {
                            let hash_hex = hash_to_hex(&req.h);
                            let hash_short = &hash_hex[..8.min(hash_hex.len())];
                            info!(
                                "[Peer {}] Received request for {}",
                                peer_short, hash_short
                            );

                            // Handle request - look up in store
                            let data = if let Some(ref store) = store {
                                match store.get(&hash_hex) {
                                    Ok(Some(data)) => {
                                        info!("[Peer {}] Found {} in store ({} bytes)", peer_short, hash_short, data.len());
                                        Some(data)
                                    },
                                    Ok(None) => {
                                        info!("[Peer {}] Hash {} not in store", peer_short, hash_short);
                                        None
                                    },
                                    Err(e) => {
                                        warn!("[Peer {}] Store error: {}", peer_short, e);
                                        None
                                    }
                                }
                            } else {
                                warn!("[Peer {}] No store configured - cannot serve requests", peer_short);
                                None
                            };

                            // Send response only if we have data
                            if let Some(data) = data {
                                let data_len = data.len();
                                let response = DataResponse {
                                    h: req.h,
                                    d: data,
                                };
                                if let Ok(wire) = encode_response(&response) {
                                    if let Err(e) = dc.send(&Bytes::from(wire)).await {
                                        error!(
                                            "[Peer {}] Failed to send response: {}",
                                            peer_short, e
                                        );
                                    } else {
                                        info!(
                                            "[Peer {}] Sent response for {} ({} bytes)",
                                            peer_short, hash_short, data_len
                                        );
                                    }
                                }
                            } else {
                                info!("[Peer {}] Content not found for {}", peer_short, hash_short);
                            }
                        }
                        DataMessage::Response(res) => {
                            let hash_hex = hash_to_hex(&res.h);
                            let hash_short = &hash_hex[..8.min(hash_hex.len())];
                            debug!(
                                "[Peer {}] Received response for {} ({} bytes)",
                                peer_short, hash_short, res.d.len()
                            );

                            // Resolve the pending request by hash
                            let mut pending = pending_requests.lock().await;
                            if let Some(req) = pending.remove(&hash_hex) {
                                let _ = req.response_tx.send(Some(res.d));
                            }
                        }
                    },
                    Err(e) => {
                        warn!("[Peer {}] Failed to parse message: {:?}", peer_short, e);
                        // Log hex dump of first 50 bytes for debugging
                        let hex_dump: String = msg_data.iter().take(50).map(|b| format!("{:02x}", b)).collect();
                        warn!("[Peer {}] Message hex: {}", peer_short, hex_dump);
                    }
                }
            })
        }));
    }

    /// Check if data channel is ready
    pub fn has_data_channel(&self) -> bool {
        // Use try_lock for non-async context
        self.data_channel
            .try_lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Request content by hash from this peer
    pub async fn request(&self, hash_hex: &str) -> Result<Option<Vec<u8>>> {
        let dc_guard = self.data_channel.lock().await;
        let dc = dc_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No data channel"))?
            .clone();
        drop(dc_guard);  // Release lock before async operations

        // Convert hex to binary hash
        let hash = hex::decode(hash_hex)
            .map_err(|e| anyhow::anyhow!("Invalid hex hash: {}", e))?;

        // Create response channel
        let (tx, rx) = oneshot::channel();

        // Store pending request (keyed by hash hex)
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(
                hash_hex.to_string(),
                PendingRequest {
                    hash: hash.clone(),
                    response_tx: tx,
                },
            );
        }

        // Send request with MAX_HTL (fresh request from us)
        let req = DataRequest {
            h: hash,
            htl: crate::webrtc::types::MAX_HTL,
        };
        let wire = encode_request(&req)?;
        dc.send(&Bytes::from(wire)).await?;

        debug!(
            "[Peer {}] Sent request for {}",
            self.peer_id.short(),
            &hash_hex[..8.min(hash_hex.len())]
        );

        // Wait for response with timeout
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => {
                // Channel closed
                Ok(None)
            }
            Err(_) => {
                // Timeout - clean up pending request
                let mut pending = self.pending_requests.lock().await;
                pending.remove(hash_hex);
                Ok(None)
            }
        }
    }

    /// Send a message over the data channel
    pub async fn send_message(&self, msg: &DataMessage) -> Result<()> {
        let dc_guard = self.data_channel.lock().await;
        if let Some(ref dc) = *dc_guard {
            let wire = encode_message(msg)?;
            dc.send(&Bytes::from(wire)).await?;
        }
        Ok(())
    }

    /// Close the connection
    pub async fn close(&self) -> Result<()> {
        {
            let dc_guard = self.data_channel.lock().await;
            if let Some(ref dc) = *dc_guard {
                dc.close().await?;
            }
        }
        self.pc.close().await?;
        Ok(())
    }
}
