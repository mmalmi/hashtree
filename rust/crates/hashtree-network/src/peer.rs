//! WebRTC peer connection for hashtree data exchange

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tracing::{debug, error, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::cashu::CashuQuoteState;
use crate::relay_bridge::SharedMeshRelayClient;
use crate::runtime_control::PeerStateEvent;
use crate::runtime_peer::PeerDirection;
use crate::transport::{PeerLink as RoutedPeerLink, TransportError as RoutedTransportError};
use crate::types::{
    validate_mesh_frame, MeshNostrFrame, PeerHTLConfig, PeerId, SignalingMessage,
    BLOB_REQUEST_POLICY, DATA_CHANNEL_LABEL,
};
use crate::{
    encode_payment_ack, encode_peer_hints, encode_quote_response, encode_request, encode_response,
    hash_to_key, parse_message, DataChunk, DataMessage, DataRequest, DataResponse, PeerHints,
};
use nostr_sdk::nostr::{
    ClientMessage as NostrClientMessage, Event, Filter as NostrFilter, JsonUtil as NostrJsonUtil,
    RelayMessage as NostrRelayMessage, SubscriptionId as NostrSubscriptionId,
};

mod payments;

use payments::{
    handle_payment_ack_message, handle_payment_message, handle_quote_request_message,
    process_chunk_message, send_quoted_chunk,
};

/// Trait for content storage that can be used by WebRTC peers
pub trait ContentStore: Send + Sync + 'static {
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>>;
}

/// Pending request tracking (keyed by hash hex)
pub struct PendingRequest {
    pub hash: Vec<u8>,
    pub response_tx: oneshot::Sender<Option<Vec<u8>>>,
    pub quoted: Option<PendingQuotedRequest>,
}

pub struct PendingQuotedRequest {
    pub quote_id: u64,
    pub mint_url: String,
    pub total_payment_sat: u64,
    pub confirmed_payment_sat: u64,
    pub next_chunk_index: u32,
    pub total_chunks: Option<u32>,
    pub assembled_data: Vec<u8>,
    pub in_flight_payment: Option<PendingChunkPayment>,
    pub buffered_chunk: Option<DataChunk>,
}

pub struct PendingChunkPayment {
    pub chunk_index: u32,
    pub amount_sat: u64,
    pub mint_url: String,
    pub operation_id: String,
    pub final_chunk: bool,
}

impl PendingRequest {
    pub fn standard(hash: Vec<u8>, response_tx: oneshot::Sender<Option<Vec<u8>>>) -> Self {
        Self {
            hash,
            response_tx,
            quoted: None,
        }
    }

    pub fn quoted(
        hash: Vec<u8>,
        response_tx: oneshot::Sender<Option<Vec<u8>>>,
        quote_id: u64,
        mint_url: String,
        total_payment_sat: u64,
    ) -> Self {
        Self {
            hash,
            response_tx,
            quoted: Some(PendingQuotedRequest {
                quote_id,
                mint_url,
                total_payment_sat,
                confirmed_payment_sat: 0,
                next_chunk_index: 0,
                total_chunks: None,
                assembled_data: Vec::new(),
                in_flight_payment: None,
                buffered_chunk: None,
            }),
        }
    }
}

/// WebRTC peer connection with data channel protocol
pub struct Peer {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub created_at: std::time::Instant,
    pub connected_at: Option<std::time::Instant>,

    pc: Arc<RTCPeerConnection>,
    pub data_channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
    signaling_tx: mpsc::Sender<SignalingMessage>,
    my_peer_id: PeerId,
    store: Option<Arc<dyn ContentStore>>,
    pub pending_requests: Arc<Mutex<HashMap<String, PendingRequest>>>,
    pending_nostr_queries: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<NostrRelayMessage>>>>,
    #[allow(dead_code)]
    message_tx: mpsc::Sender<(DataMessage, Option<Vec<u8>>)>,
    #[allow(dead_code)]
    message_rx: Option<mpsc::Receiver<(DataMessage, Option<Vec<u8>>)>>,
    state_event_tx: Option<mpsc::Sender<PeerStateEvent>>,
    nostr_relay: Option<SharedMeshRelayClient>,
    mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
    cashu_quotes: Option<Arc<CashuQuoteState>>,
    htl_config: PeerHTLConfig,
    signal_urls: Vec<String>,
}

impl Peer {
    pub async fn new(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
    ) -> Result<Self> {
        Self::new_with_store_and_events(
            peer_id,
            direction,
            my_peer_id,
            signaling_tx,
            stun_servers,
            None,
            None,
            None,
            None,
            None,
        )
        .await
    }

    pub async fn new_with_store(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
        store: Option<Arc<dyn ContentStore>>,
    ) -> Result<Self> {
        Self::new_with_store_and_events(
            peer_id,
            direction,
            my_peer_id,
            signaling_tx,
            stun_servers,
            store,
            None,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_store_and_events(
        peer_id: PeerId,
        direction: PeerDirection,
        my_peer_id: PeerId,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        stun_servers: Vec<String>,
        store: Option<Arc<dyn ContentStore>>,
        state_event_tx: Option<mpsc::Sender<PeerStateEvent>>,
        nostr_relay: Option<SharedMeshRelayClient>,
        mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
        cashu_quotes: Option<Arc<CashuQuoteState>>,
    ) -> Result<Self> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;

        let setting_engine = SettingEngine::default();
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

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
            pending_nostr_queries: Arc::new(Mutex::new(HashMap::new())),
            message_tx,
            message_rx: Some(message_rx),
            state_event_tx,
            nostr_relay,
            mesh_frame_tx,
            cashu_quotes,
            htl_config: PeerHTLConfig::random(),
            signal_urls: Vec::new(),
        })
    }

    pub fn set_signal_urls(&mut self, signal_urls: Vec<String>) {
        self.signal_urls = signal_urls;
    }

    pub fn set_store(&mut self, store: Arc<dyn ContentStore>) {
        self.store = Some(store);
    }

    pub fn state(&self) -> RTCPeerConnectionState {
        self.pc.connection_state()
    }

    pub fn signaling_state(&self) -> webrtc::peer_connection::signaling_state::RTCSignalingState {
        self.pc.signaling_state()
    }

    pub fn is_connected(&self) -> bool {
        self.pc.connection_state() == RTCPeerConnectionState::Connected
    }

    pub fn htl_config(&self) -> &PeerHTLConfig {
        &self.htl_config
    }

    pub async fn setup_handlers(&self) -> Result<()> {
        let peer_id = self.peer_id.clone();
        let signaling_tx = self.signaling_tx.clone();
        let my_peer_id_str = self.my_peer_id.to_string();
        let target_peer_id = self.peer_id.to_string();

        self.pc
            .on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
                let signaling_tx = signaling_tx.clone();
                let my_peer_id_str = my_peer_id_str.clone();
                let target_peer_id = target_peer_id.clone();

                Box::pin(async move {
                    if let Some(c) = candidate {
                        if let Ok(init) = c.to_json() {
                            info!(
                                "ICE candidate generated: {}",
                                &init.candidate[..init.candidate.len().min(60)]
                            );
                            let msg = SignalingMessage::Candidate {
                                peer_id: my_peer_id_str.clone(),
                                target_peer_id: target_peer_id.clone(),
                                candidate: init.candidate,
                                sdp_m_line_index: init.sdp_mline_index,
                                sdp_mid: init.sdp_mid,
                            };
                            if let Err(e) = signaling_tx.send(msg).await {
                                error!("Failed to send ICE candidate: {}", e);
                            }
                        }
                    }
                })
            }));

        let peer_id_log = peer_id.clone();
        let state_event_tx = self.state_event_tx.clone();
        self.pc
            .on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
                let peer_id = peer_id_log.clone();
                let state_event_tx = state_event_tx.clone();
                Box::pin(async move {
                    info!("Peer {} connection state: {:?}", peer_id.short(), state);

                    if let Some(tx) = state_event_tx {
                        let event = match state {
                            RTCPeerConnectionState::Connected => {
                                Some(PeerStateEvent::Connected(peer_id))
                            }
                            RTCPeerConnectionState::Failed => Some(PeerStateEvent::Failed(peer_id)),
                            RTCPeerConnectionState::Disconnected
                            | RTCPeerConnectionState::Closed => {
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

    pub async fn connect(&self) -> Result<serde_json::Value> {
        let dc_init = RTCDataChannelInit {
            ordered: Some(false),
            ..Default::default()
        };
        let dc = self
            .pc
            .create_data_channel(DATA_CHANNEL_LABEL, Some(dc_init))
            .await?;
        self.setup_data_channel(dc.clone()).await?;
        {
            let mut dc_guard = self.data_channel.lock().await;
            *dc_guard = Some(dc);
        }

        let offer = self.pc.create_offer(None).await?;
        let mut gathering_complete = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(offer).await?;

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gathering_complete.recv(),
        )
        .await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("No local description after gathering"))?;

        debug!(
            "Offer created, SDP len: {}, ice_gathering: {:?}",
            local_desc.sdp.len(),
            self.pc.ice_gathering_state()
        );

        Ok(serde_json::json!({
            "type": local_desc.sdp_type.to_string().to_lowercase(),
            "sdp": local_desc.sdp
        }))
    }

    pub async fn handle_offer(&self, offer: serde_json::Value) -> Result<serde_json::Value> {
        let sdp = offer
            .get("sdp")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing SDP in offer"))?;

        let peer_id = self.peer_id.clone();
        let message_tx = self.message_tx.clone();
        let pending_requests = self.pending_requests.clone();
        let pending_nostr_queries = self.pending_nostr_queries.clone();
        let store = self.store.clone();
        let data_channel_holder = self.data_channel.clone();
        let nostr_relay = self.nostr_relay.clone();
        let mesh_frame_tx = self.mesh_frame_tx.clone();
        let cashu_quotes = self.cashu_quotes.clone();
        let peer_pubkey = Some(self.peer_id.pubkey.clone());
        let state_event_tx = self.state_event_tx.clone();
        let signal_urls = self.signal_urls.clone();

        self.pc
            .on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let peer_id = peer_id.clone();
                let message_tx = message_tx.clone();
                let pending_requests = pending_requests.clone();
                let pending_nostr_queries = pending_nostr_queries.clone();
                let store = store.clone();
                let data_channel_holder = data_channel_holder.clone();
                let nostr_relay = nostr_relay.clone();
                let mesh_frame_tx = mesh_frame_tx.clone();
                let cashu_quotes = cashu_quotes.clone();
                let peer_pubkey = peer_pubkey.clone();
                let state_event_tx = state_event_tx.clone();
                let signal_urls = signal_urls.clone();

                Box::pin(async move {
                    info!(
                        "Peer {} received data channel: {}",
                        peer_id.short(),
                        dc.label()
                    );

                    {
                        let mut dc_guard = data_channel_holder.lock().await;
                        *dc_guard = Some(dc.clone());
                    }

                    Self::setup_dc_handlers(
                        dc.clone(),
                        peer_id,
                        message_tx,
                        pending_requests,
                        pending_nostr_queries.clone(),
                        store,
                        nostr_relay,
                        mesh_frame_tx,
                        cashu_quotes,
                        peer_pubkey,
                        state_event_tx,
                        signal_urls,
                    )
                    .await;
                })
            }));

        let offer_desc = RTCSessionDescription::offer(sdp.to_string())?;
        self.pc.set_remote_description(offer_desc).await?;

        let answer = self.pc.create_answer(None).await?;
        let mut gathering_complete = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(answer).await?;

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gathering_complete.recv(),
        )
        .await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("No local description after gathering"))?;

        debug!(
            "Answer created, SDP len: {}, ice_gathering: {:?}",
            local_desc.sdp.len(),
            self.pc.ice_gathering_state()
        );

        Ok(serde_json::json!({
            "type": local_desc.sdp_type.to_string().to_lowercase(),
            "sdp": local_desc.sdp
        }))
    }

    pub async fn handle_answer(&self, answer: serde_json::Value) -> Result<()> {
        let sdp = answer
            .get("sdp")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing SDP in answer"))?;

        let answer_desc = RTCSessionDescription::answer(sdp.to_string())?;
        self.pc.set_remote_description(answer_desc).await?;
        Ok(())
    }

    pub async fn handle_candidate(&self, candidate: serde_json::Value) -> Result<()> {
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

    async fn setup_data_channel(&self, dc: Arc<RTCDataChannel>) -> Result<()> {
        let peer_id = self.peer_id.clone();
        let message_tx = self.message_tx.clone();
        let pending_requests = self.pending_requests.clone();
        let store = self.store.clone();
        let nostr_relay = self.nostr_relay.clone();
        let mesh_frame_tx = self.mesh_frame_tx.clone();
        let cashu_quotes = self.cashu_quotes.clone();
        let peer_pubkey = Some(self.peer_id.pubkey.clone());
        let state_event_tx = self.state_event_tx.clone();
        let signal_urls = self.signal_urls.clone();

        Self::setup_dc_handlers(
            dc,
            peer_id,
            message_tx,
            pending_requests,
            self.pending_nostr_queries.clone(),
            store,
            nostr_relay,
            mesh_frame_tx,
            cashu_quotes,
            peer_pubkey,
            state_event_tx,
            signal_urls,
        )
        .await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn setup_dc_handlers(
        dc: Arc<RTCDataChannel>,
        peer_id: PeerId,
        message_tx: mpsc::Sender<(DataMessage, Option<Vec<u8>>)>,
        pending_requests: Arc<Mutex<HashMap<String, PendingRequest>>>,
        pending_nostr_queries: Arc<
            Mutex<HashMap<String, mpsc::UnboundedSender<NostrRelayMessage>>>,
        >,
        store: Option<Arc<dyn ContentStore>>,
        nostr_relay: Option<SharedMeshRelayClient>,
        mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
        cashu_quotes: Option<Arc<CashuQuoteState>>,
        peer_pubkey: Option<String>,
        state_event_tx: Option<mpsc::Sender<PeerStateEvent>>,
        signal_urls: Vec<String>,
    ) {
        let label = dc.label().to_string();
        let peer_short = peer_id.short();
        let _pending_binary: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let open_notify = nostr_relay.as_ref().map(|_| Arc::new(Notify::new()));
        if let Some(ref notify) = open_notify {
            if dc.ready_state() == RTCDataChannelState::Open {
                notify.notify_one();
            }
        }

        let mut nostr_client_id: Option<u64> = None;
        if let Some(relay) = nostr_relay.clone() {
            let client_id = relay.next_client_id();
            let (nostr_tx, mut nostr_rx) = mpsc::unbounded_channel::<String>();
            relay
                .register_client(client_id, nostr_tx, peer_pubkey.clone())
                .await;
            nostr_client_id = Some(client_id);

            if let Some(notify) = open_notify.clone() {
                let dc_for_send = dc.clone();
                tokio::spawn(async move {
                    notify.notified().await;
                    while let Some(text) = nostr_rx.recv().await {
                        if dc_for_send.send_text(text).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }

        if let (Some(relay), Some(client_id)) = (nostr_relay.clone(), nostr_client_id) {
            dc.on_close(Box::new(move || {
                let relay = relay.clone();
                Box::pin(async move {
                    relay.unregister_client(client_id).await;
                })
            }));
        }

        let open_notify_clone = open_notify.clone();
        let dc_for_hints = dc.clone();
        let signal_urls_for_open = signal_urls.clone();
        let peer_short_open = peer_short.clone();
        let label_clone = label.clone();
        dc.on_open(Box::new(move || {
            let peer_short_open = peer_short_open.clone();
            let label_clone = label_clone.clone();
            let open_notify = open_notify_clone.clone();
            let dc_for_hints = dc_for_hints.clone();
            let signal_urls = signal_urls_for_open.clone();
            Box::pin(async move {
                info!(
                    "[Peer {}] Data channel '{}' open",
                    peer_short_open, label_clone
                );
                if let Some(notify) = open_notify {
                    notify.notify_one();
                }
                if !signal_urls.is_empty() {
                    let hints = PeerHints { signal_urls };
                    let _ = dc_for_hints
                        .send(&Bytes::from(encode_peer_hints(&hints)))
                        .await;
                }
            })
        }));

        if dc.ready_state() == RTCDataChannelState::Open && !signal_urls.is_empty() {
            let hints = PeerHints { signal_urls };
            let _ = dc.send(&Bytes::from(encode_peer_hints(&hints))).await;
        }

        let dc_for_msg = dc.clone();
        let peer_short_msg = peer_short.clone();
        let _pending_binary_clone = _pending_binary.clone();
        let store_clone = store.clone();
        let nostr_relay_for_msg = nostr_relay.clone();
        let nostr_client_id_for_msg = nostr_client_id;
        let pending_nostr_queries_for_msg = pending_nostr_queries.clone();
        let mesh_frame_tx_for_msg = mesh_frame_tx.clone();
        let peer_id_for_msg = peer_id.clone();

        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let dc = dc_for_msg.clone();
            let peer_short = peer_short_msg.clone();
            let pending_requests = pending_requests.clone();
            let _pending_binary = _pending_binary_clone.clone();
            let _message_tx = message_tx.clone();
            let store = store_clone.clone();
            let nostr_relay = nostr_relay_for_msg.clone();
            let nostr_client_id = nostr_client_id_for_msg;
            let pending_nostr_queries = pending_nostr_queries_for_msg.clone();
            let mesh_frame_tx = mesh_frame_tx_for_msg.clone();
            let cashu_quotes = cashu_quotes.clone();
            let state_event_tx = state_event_tx.clone();
            let peer_id = peer_id_for_msg.clone();
            let msg_data = msg.data.clone();

            Box::pin(async move {
                if msg.is_string {
                    if let Ok(text) = std::str::from_utf8(&msg_data) {
                        if let Ok(mesh_frame) = serde_json::from_str::<MeshNostrFrame>(text) {
                            match validate_mesh_frame(&mesh_frame) {
                                Ok(()) => {
                                    if let Some(tx) = mesh_frame_tx {
                                        let _ = tx.send((peer_id.clone(), mesh_frame)).await;
                                    }
                                    return;
                                }
                                Err(reason) => {
                                    debug!(
                                        "[Peer {}] Ignoring invalid mesh frame: {}",
                                        peer_short, reason
                                    );
                                }
                            }
                        }

                        if let Ok(relay_msg) = NostrRelayMessage::from_json(text) {
                            if let Some(sub_id) = relay_subscription_id(&relay_msg) {
                                let sender = {
                                    let pending = pending_nostr_queries.lock().await;
                                    pending.get(&sub_id).cloned()
                                };
                                if let Some(tx) = sender {
                                    debug!(
                                        "[Peer {}] Routed Nostr relay message for subscription {}",
                                        peer_short, sub_id
                                    );
                                    let _ = tx.send(relay_msg);
                                    return;
                                } else {
                                    debug!(
                                        "[Peer {}] Dropping Nostr relay message for unknown subscription {}",
                                        peer_short, sub_id
                                    );
                                }
                            }
                        }

                        if let Some(relay) = nostr_relay {
                            if let Ok(nostr_msg) = NostrClientMessage::from_json(text) {
                                if let Some(client_id) = nostr_client_id {
                                    relay.handle_client_message(client_id, nostr_msg).await;
                                }
                            }
                        }
                    }
                    return;
                }

                debug!(
                    "[Peer {}] Received {} bytes on data channel",
                    peer_short,
                    msg_data.len()
                );
                match parse_message(&msg_data) {
                    Some(data_msg) => match data_msg {
                        DataMessage::Request(req) => {
                            let hash_hex = hash_to_hex(&req.h);
                            let hash_short = &hash_hex[..8.min(hash_hex.len())];
                            info!("[Peer {}] Received request for {}", peer_short, hash_short);

                            if let Some(cashu_quotes) = cashu_quotes.as_ref() {
                                if cashu_quotes
                                    .should_refuse_requests_from_peer(&peer_id.to_string())
                                    .await
                                {
                                    info!(
                                        "[Peer {}] Refusing request from peer with unpaid defaults",
                                        peer_short
                                    );
                                    return;
                                }
                            }

                            let quoted_settlement = if let Some(quote_id) = req.q {
                                let Some(cashu_quotes) = cashu_quotes.as_ref() else {
                                    info!(
                                        "[Peer {}] Ignoring quoted request without Cashu settlement state",
                                        peer_short
                                    );
                                    return;
                                };
                                match cashu_quotes
                                    .take_valid_quote(&peer_id.to_string(), &req.h, quote_id)
                                    .await
                                {
                                    Some(settlement) => Some((quote_id, settlement)),
                                    None => {
                                        info!(
                                            "[Peer {}] Ignoring request with invalid or expired quote {}",
                                            peer_short, quote_id
                                        );
                                        return;
                                    }
                                }
                            } else {
                                None
                            };

                            let data = if let Some(ref store) = store {
                                match store.get(&hash_hex) {
                                    Ok(Some(data)) => {
                                        info!(
                                            "[Peer {}] Found {} in store ({} bytes)",
                                            peer_short,
                                            hash_short,
                                            data.len()
                                        );
                                        Some(data)
                                    }
                                    Ok(None) => {
                                        info!(
                                            "[Peer {}] Hash {} not in store",
                                            peer_short, hash_short
                                        );
                                        None
                                    }
                                    Err(e) => {
                                        warn!("[Peer {}] Store error: {}", peer_short, e);
                                        None
                                    }
                                }
                            } else {
                                warn!(
                                    "[Peer {}] No store configured - cannot serve requests",
                                    peer_short
                                );
                                None
                            };

                            if let Some(data) = data {
                                let data_len = data.len();
                                if let (Some(cashu_quotes), Some((quote_id, settlement))) =
                                    (cashu_quotes.as_ref(), quoted_settlement)
                                {
                                    match cashu_quotes
                                        .prepare_quoted_transfer(
                                            &peer_id.to_string(),
                                            &req.h,
                                            quote_id,
                                            &settlement,
                                            data,
                                        )
                                        .await
                                    {
                                        Some((first_chunk, first_expected)) => {
                                            if send_quoted_chunk(
                                                &dc,
                                                &peer_id,
                                                &peer_short,
                                                cashu_quotes,
                                                first_chunk,
                                                first_expected,
                                            )
                                            .await
                                            {
                                                info!(
                                                    "[Peer {}] Started quoted chunked response for {} ({} bytes)",
                                                    peer_short, hash_short, data_len
                                                );
                                            }
                                        }
                                        None => {
                                            warn!(
                                                "[Peer {}] Failed to prepare quoted transfer for {}",
                                                peer_short, hash_short
                                            );
                                        }
                                    }
                                } else {
                                    let response = DataResponse {
                                        h: req.h,
                                        d: data,
                                        i: None,
                                        n: None,
                                    };
                                    let wire = encode_response(&response);
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
                                peer_short,
                                hash_short,
                                res.d.len()
                            );

                            let mut pending = pending_requests.lock().await;
                            if let Some(req) = pending.remove(&hash_hex) {
                                let _ = req.response_tx.send(Some(res.d));
                            }
                        }
                        DataMessage::QuoteRequest(req) => {
                            let response = handle_quote_request_message(
                                &peer_short,
                                &peer_id,
                                &store,
                                cashu_quotes.as_ref(),
                                &req,
                            )
                            .await;
                            if let Some(response) = response {
                                let wire = encode_quote_response(&response);
                                if let Err(e) = dc.send(&Bytes::from(wire)).await {
                                    warn!(
                                        "[Peer {}] Failed to send quote response: {}",
                                        peer_short, e
                                    );
                                }
                            }
                        }
                        DataMessage::QuoteResponse(res) => {
                            if let Some(cashu_quotes) = cashu_quotes.as_ref() {
                                let _ = cashu_quotes
                                    .handle_quote_response(&peer_id.to_string(), res)
                                    .await;
                            }
                        }
                        DataMessage::Chunk(chunk) => {
                            process_chunk_message(
                                &peer_short,
                                &peer_id,
                                &dc,
                                &pending_requests,
                                cashu_quotes.as_ref(),
                                chunk,
                            )
                            .await;
                        }
                        DataMessage::Payment(req) => {
                            let outcome =
                                handle_payment_message(&peer_id, cashu_quotes.as_ref(), &req).await;
                            let wire = encode_payment_ack(&outcome.ack);
                            if let Err(e) = dc.send(&Bytes::from(wire)).await {
                                warn!(
                                    "[Peer {}] Failed to send payment ack: {}",
                                    peer_short, e
                                );
                            }
                            if let (Some(cashu_quotes), Some((next_chunk, next_expected))) =
                                (cashu_quotes.as_ref(), outcome.next_chunk)
                            {
                                let _ = send_quoted_chunk(
                                    &dc,
                                    &peer_id,
                                    &peer_short,
                                    cashu_quotes,
                                    next_chunk,
                                    next_expected,
                                )
                                .await;
                            }
                        }
                        DataMessage::PaymentAck(res) => {
                            handle_payment_ack_message(
                                &peer_short,
                                &peer_id,
                                &dc,
                                &pending_requests,
                                cashu_quotes.as_ref(),
                                res,
                            )
                            .await;
                        }
                        DataMessage::PeerHints(hints) => {
                            if let Some(tx) = state_event_tx {
                                let _ = tx
                                    .send(PeerStateEvent::SignalHints(
                                        peer_id.clone(),
                                        hints.signal_urls,
                                    ))
                                    .await;
                            }
                        }
                    },
                    None => {
                        warn!("[Peer {}] Failed to parse message", peer_short);
                        let hex_dump: String = msg_data
                            .iter()
                            .take(50)
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        warn!("[Peer {}] Message hex: {}", peer_short, hex_dump);
                    }
                }
            })
        }));
    }

    pub fn has_data_channel(&self) -> bool {
        self.data_channel
            .try_lock()
            .map(|guard| {
                guard
                    .as_ref()
                    .map(|dc| dc.ready_state() == RTCDataChannelState::Open)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    pub async fn request(&self, hash_hex: &str) -> Result<Option<Vec<u8>>> {
        self.request_with_timeout(hash_hex, std::time::Duration::from_secs(10))
            .await
    }

    pub async fn request_with_timeout(
        &self,
        hash_hex: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<Vec<u8>>> {
        let dc_guard = self.data_channel.lock().await;
        let dc = dc_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No data channel"))?
            .clone();
        drop(dc_guard);

        let hash = hex::decode(hash_hex).map_err(|e| anyhow::anyhow!("Invalid hex hash: {}", e))?;
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(
                hash_hex.to_string(),
                PendingRequest::standard(hash.clone(), tx),
            );
        }

        let req = DataRequest {
            h: hash,
            htl: BLOB_REQUEST_POLICY.max_htl,
            q: None,
        };
        let wire = encode_request(&req);
        dc.send(&Bytes::from(wire)).await?;

        debug!(
            "[Peer {}] Sent request for {}",
            self.peer_id.short(),
            &hash_hex[..8.min(hash_hex.len())]
        );

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => Ok(None),
            Err(_) => {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(hash_hex);
                Ok(None)
            }
        }
    }

    pub async fn query_nostr_events(
        &self,
        filters: Vec<NostrFilter>,
        timeout: std::time::Duration,
    ) -> Result<Vec<Event>> {
        let dc_guard = self.data_channel.lock().await;
        let dc = dc_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No data channel"))?
            .clone();
        drop(dc_guard);

        let subscription_id = NostrSubscriptionId::generate();
        let subscription_key = subscription_id.to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<NostrRelayMessage>();

        {
            let mut pending = self.pending_nostr_queries.lock().await;
            pending.insert(subscription_key.clone(), tx);
        }

        let req = NostrClientMessage::req(subscription_id.clone(), filters);
        if let Err(e) = dc.send_text(req.as_json()).await {
            let mut pending = self.pending_nostr_queries.lock().await;
            pending.remove(&subscription_key);
            return Err(e.into());
        }
        debug!(
            "[Peer {}] Sent Nostr REQ subscription {}",
            self.peer_id.short(),
            subscription_id
        );

        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;

            let next = tokio::time::timeout(remaining, rx.recv()).await;
            match next {
                Ok(Some(NostrRelayMessage::Event {
                    subscription_id: sid,
                    event,
                })) if sid == subscription_id => {
                    debug!(
                        "[Peer {}] Received Nostr EVENT for subscription {}",
                        self.peer_id.short(),
                        subscription_id
                    );
                    events.push(*event);
                }
                Ok(Some(NostrRelayMessage::EndOfStoredEvents(sid))) if sid == subscription_id => {
                    debug!(
                        "[Peer {}] Received Nostr EOSE for subscription {}",
                        self.peer_id.short(),
                        subscription_id
                    );
                    break;
                }
                Ok(Some(NostrRelayMessage::Closed {
                    subscription_id: sid,
                    message,
                })) if sid == subscription_id => {
                    warn!(
                        "[Peer {}] Nostr query closed for subscription {}: {}",
                        self.peer_id.short(),
                        subscription_id,
                        message
                    );
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    warn!(
                        "[Peer {}] Nostr query timed out for subscription {}",
                        self.peer_id.short(),
                        subscription_id
                    );
                    break;
                }
            }
        }

        let close = NostrClientMessage::close(subscription_id.clone());
        let _ = dc.send_text(close.as_json()).await;

        let mut pending = self.pending_nostr_queries.lock().await;
        pending.remove(&subscription_key);
        debug!(
            "[Peer {}] Nostr query subscription {} collected {} event(s)",
            self.peer_id.short(),
            subscription_id,
            events.len()
        );

        Ok(events)
    }

    pub async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()> {
        let dc_guard = self.data_channel.lock().await;
        let dc = dc_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No data channel"))?
            .clone();
        drop(dc_guard);

        let text = serde_json::to_string(frame)?;
        dc.send_text(text).await?;
        Ok(())
    }

    pub async fn send_message(&self, msg: &DataMessage) -> Result<()> {
        let dc_guard = self.data_channel.lock().await;
        if let Some(ref dc) = *dc_guard {
            let wire = encode_data_message(msg);
            dc.send(&Bytes::from(wire)).await?;
        }
        Ok(())
    }

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

fn hash_to_hex(hash: &[u8]) -> String {
    hash_to_key(hash)
}

fn encode_data_message(msg: &DataMessage) -> Vec<u8> {
    match msg {
        DataMessage::Request(req) => encode_request(req),
        DataMessage::Response(res) => encode_response(res),
        DataMessage::QuoteRequest(req) => crate::encode_quote_request(req),
        DataMessage::QuoteResponse(res) => encode_quote_response(res),
        DataMessage::Payment(req) => crate::encode_payment(req),
        DataMessage::PaymentAck(res) => crate::encode_payment_ack(res),
        DataMessage::Chunk(chunk) => crate::encode_chunk(chunk),
        DataMessage::PeerHints(hints) => crate::encode_peer_hints(hints),
    }
}

fn relay_subscription_id(msg: &NostrRelayMessage) -> Option<String> {
    match msg {
        NostrRelayMessage::Event {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        NostrRelayMessage::EndOfStoredEvents(subscription_id) => Some(subscription_id.to_string()),
        NostrRelayMessage::Closed {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        NostrRelayMessage::Count {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        _ => None,
    }
}

#[async_trait]
impl RoutedPeerLink for Peer {
    async fn send(&self, data: Vec<u8>) -> std::result::Result<(), RoutedTransportError> {
        let dc = self
            .data_channel
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or(RoutedTransportError::NotConnected)?;
        dc.send(&Bytes::from(data))
            .await
            .map(|_| ())
            .map_err(|e| RoutedTransportError::SendFailed(e.to_string()))
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        None
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        None
    }

    fn is_open(&self) -> bool {
        self.has_data_channel()
    }

    async fn close(&self) {
        let _ = Peer::close(self).await;
    }
}
