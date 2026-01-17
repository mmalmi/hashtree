//! Real WebRTC peer connection factory
//!
//! Wraps the webrtc crate to implement PeerConnectionFactory for production use.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::transport::{DataChannel, PeerConnectionFactory, TransportError};
use crate::types::DATA_CHANNEL_LABEL;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

/// Wrapper around RTCDataChannel that implements our DataChannel trait
pub struct RealDataChannel {
    dc: Arc<RTCDataChannel>,
    /// Receiver for incoming messages (populated by on_message callback)
    msg_rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl RealDataChannel {
    /// Create a new RealDataChannel with message handling
    pub fn new(dc: Arc<RTCDataChannel>) -> Arc<Self> {
        let (msg_tx, msg_rx) = mpsc::channel(100);

        // Set up on_message handler to forward messages to channel
        let tx = msg_tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            let data = msg.data.to_vec();
            Box::pin(async move {
                let _ = tx.send(data).await;
            })
        }));

        Arc::new(Self {
            dc,
            msg_rx: Mutex::new(msg_rx),
        })
    }
}

#[async_trait]
impl DataChannel for RealDataChannel {
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError> {
        self.dc
            .send(&bytes::Bytes::from(data))
            .await
            .map(|_| ())
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        self.msg_rx.lock().await.recv().await
    }

    fn is_open(&self) -> bool {
        self.dc.ready_state() == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
    }

    async fn close(&self) {
        let _ = self.dc.close().await;
    }
}

/// Pending connection state
struct PendingConnection {
    connection: Arc<RTCPeerConnection>,
    data_channel: Option<Arc<RTCDataChannel>>,
}

/// Real WebRTC peer connection factory
///
/// Creates actual WebRTC connections using the webrtc crate.
pub struct RealPeerConnectionFactory {
    /// Pending outbound connections (we sent offer, waiting for answer)
    pending: RwLock<HashMap<String, PendingConnection>>,
    /// Pending inbound connections (we received offer, sent answer)
    inbound: RwLock<HashMap<String, PendingConnection>>,
    /// STUN servers for ICE
    stun_servers: Vec<String>,
}

impl RealPeerConnectionFactory {
    pub fn new() -> Self {
        Self::with_stun_servers(vec![
            "stun:stun.iris.to:3478".to_string(),
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun.cloudflare.com:3478".to_string(),
        ])
    }

    pub fn with_stun_servers(stun_servers: Vec<String>) -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            inbound: RwLock::new(HashMap::new()),
            stun_servers,
        }
    }

    async fn create_connection(&self) -> Result<Arc<RTCPeerConnection>, TransportError> {
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: self.stun_servers.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };

        api.new_peer_connection(config)
            .await
            .map(Arc::new)
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))
    }

    /// Wait for ICE gathering to complete and return the SDP with embedded candidates
    async fn wait_for_ice_gathering(
        connection: &Arc<RTCPeerConnection>,
    ) -> Result<String, TransportError> {
        let mut gathering_complete = connection.gathering_complete_promise().await;

        // Wait for ICE gathering to complete (with timeout)
        let _ = tokio::time::timeout(Duration::from_secs(10), gathering_complete.recv()).await;

        // Get the local description with ICE candidates embedded
        let local_desc = connection
            .local_description()
            .await
            .ok_or_else(|| TransportError::ConnectionFailed("No local description after ICE gathering".to_string()))?;

        Ok(local_desc.sdp)
    }
}

impl Default for RealPeerConnectionFactory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PeerConnectionFactory for RealPeerConnectionFactory {
    async fn create_offer(
        &self,
        target_peer_id: &str,
    ) -> Result<(Arc<dyn DataChannel>, String), TransportError> {
        let connection = self.create_connection().await?;

        // Create data channel (unordered for better performance - protocol is stateless)
        let dc_init = RTCDataChannelInit {
            ordered: Some(false),
            ..Default::default()
        };
        let dc = connection
            .create_data_channel(DATA_CHANNEL_LABEL, Some(dc_init))
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        // Create offer and set local description to start ICE gathering
        let offer = connection
            .create_offer(None)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        connection
            .set_local_description(offer)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        // Wait for ICE gathering to complete - this embeds ICE candidates in the SDP
        let sdp = Self::wait_for_ice_gathering(&connection).await?;

        // Store pending connection (we'll need it when answer arrives)
        self.pending.write().await.insert(
            target_peer_id.to_string(),
            PendingConnection {
                connection,
                data_channel: Some(dc.clone()),
            },
        );

        // Create channel wrapper with message handling
        let channel: Arc<dyn DataChannel> = RealDataChannel::new(dc);
        Ok((channel, sdp))
    }

    async fn accept_offer(
        &self,
        from_peer_id: &str,
        offer_sdp: &str,
    ) -> Result<(Arc<dyn DataChannel>, String), TransportError> {
        let connection = self.create_connection().await?;

        // Set up data channel callback BEFORE setting remote description
        // This ensures we catch the data channel when it arrives
        let (dc_tx, dc_rx) = tokio::sync::oneshot::channel::<Arc<RTCDataChannel>>();
        let dc_tx = Arc::new(Mutex::new(Some(dc_tx)));

        connection.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let dc_tx = dc_tx.clone();
            Box::pin(async move {
                if let Some(tx) = dc_tx.lock().await.take() {
                    let _ = tx.send(dc);
                }
            })
        }));

        // Set remote description (the offer)
        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;
        connection
            .set_remote_description(offer)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        // Create answer and set local description to start ICE gathering
        let answer = connection
            .create_answer(None)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;
        connection
            .set_local_description(answer)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        // Wait for ICE gathering to complete - this embeds ICE candidates in the SDP
        let sdp = Self::wait_for_ice_gathering(&connection).await?;

        // Wait for data channel from remote peer (with timeout)
        let dc = tokio::time::timeout(Duration::from_secs(30), dc_rx)
            .await
            .map_err(|_| TransportError::ConnectionFailed("Timeout waiting for data channel".to_string()))?
            .map_err(|_| TransportError::ConnectionFailed("Data channel sender dropped".to_string()))?;

        // Store connection for potential future use
        self.inbound.write().await.insert(
            from_peer_id.to_string(),
            PendingConnection {
                connection,
                data_channel: Some(dc.clone()),
            },
        );

        // Create channel wrapper with message handling
        let channel: Arc<dyn DataChannel> = RealDataChannel::new(dc);
        Ok((channel, sdp))
    }

    async fn handle_answer(
        &self,
        target_peer_id: &str,
        answer_sdp: &str,
    ) -> Result<Arc<dyn DataChannel>, TransportError> {
        let pending = self
            .pending
            .write()
            .await
            .remove(target_peer_id)
            .ok_or_else(|| TransportError::ConnectionFailed("No pending connection".to_string()))?;

        // Set remote description (the answer)
        let answer = RTCSessionDescription::answer(answer_sdp.to_string())
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;
        pending
            .connection
            .set_remote_description(answer)
            .await
            .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

        // Return the data channel we created earlier with message handling
        let dc = pending
            .data_channel
            .ok_or_else(|| TransportError::ConnectionFailed("No data channel".to_string()))?;

        Ok(RealDataChannel::new(dc))
    }
}
