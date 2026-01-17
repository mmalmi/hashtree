//! Mock implementations for testing and simulation
//!
//! Provides mock relay transport and peer connection factory that use
//! in-memory channels instead of real Nostr relays and WebRTC.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::transport::{DataChannel, PeerConnectionFactory, RelayTransport, TransportError};
use crate::types::SignalingMessage;

// Global registry for mock channels (shared between offer/answer sides)
lazy_static::lazy_static! {
    static ref CHANNEL_REGISTRY: RwLock<HashMap<String, Arc<MockDataChannel>>> = RwLock::new(HashMap::new());
}

/// Clear the channel registry (call between tests)
pub async fn clear_channel_registry() {
    CHANNEL_REGISTRY.write().await.clear();
}

// ============================================================================
// Mock Relay Transport
// ============================================================================

/// Mock relay for in-memory signaling
pub struct MockRelay {
    /// Broadcast channel for all messages
    tx: broadcast::Sender<SignalingMessage>,
}

impl MockRelay {
    /// Create a new mock relay
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(1000);
        Arc::new(Self { tx })
    }

    /// Create a transport connected to this relay
    pub fn create_transport(&self, peer_id: String, pubkey: String) -> MockRelayTransport {
        MockRelayTransport {
            peer_id,
            pubkey,
            tx: self.tx.clone(),
            rx: tokio::sync::Mutex::new(self.tx.subscribe()),
            buffer: tokio::sync::Mutex::new(Vec::new()),
            connected: AtomicBool::new(false),
        }
    }
}

impl Default for MockRelay {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(1000);
        Self { tx }
    }
}

/// Mock relay transport using broadcast channels
pub struct MockRelayTransport {
    peer_id: String,
    pubkey: String,
    tx: broadcast::Sender<SignalingMessage>,
    rx: tokio::sync::Mutex<broadcast::Receiver<SignalingMessage>>,
    buffer: tokio::sync::Mutex<Vec<SignalingMessage>>,
    connected: AtomicBool,
}

impl MockRelayTransport {
    /// Get our peer ID
    pub fn peer_id_owned(&self) -> String {
        self.peer_id.clone()
    }

    /// Get our pubkey
    pub fn pubkey_owned(&self) -> String {
        self.pubkey.clone()
    }
}

#[async_trait]
impl RelayTransport for MockRelayTransport {
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
        self.tx
            .send(msg)
            .map_err(|e| TransportError::SendFailed(e.to_string()))?;
        Ok(())
    }

    async fn recv(&self) -> Option<SignalingMessage> {
        // Check buffer first
        {
            let mut buffer = self.buffer.lock().await;
            if !buffer.is_empty() {
                return Some(buffer.remove(0));
            }
        }

        // Wait for next message
        let mut rx = self.rx.lock().await;
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    // Filter: only return messages for us or broadcasts
                    if msg.is_for(&self.peer_id) || msg.target_peer_id().is_none() {
                        return Some(msg);
                    }
                    // Skip messages for other peers
                }
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    }

    fn try_recv(&self) -> Option<SignalingMessage> {
        // Check buffer first
        if let Ok(mut buffer) = self.buffer.try_lock() {
            if !buffer.is_empty() {
                return Some(buffer.remove(0));
            }
        }

        // Try non-blocking receive
        if let Ok(mut rx) = self.rx.try_lock() {
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        if msg.is_for(&self.peer_id) || msg.target_peer_id().is_none() {
                            return Some(msg);
                        }
                        // Skip messages for other peers
                    }
                    Err(_) => return None,
                }
            }
        }
        None
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }

    fn pubkey(&self) -> &str {
        &self.pubkey
    }
}

// ============================================================================
// Mock Data Channel
// ============================================================================

/// Mock data channel using mpsc channels
pub struct MockDataChannel {
    peer_id: u64,
    tx: mpsc::Sender<Vec<u8>>,
    rx: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    open: AtomicBool,
    /// Simulated latency per message (ms)
    latency_ms: u64,
}

impl MockDataChannel {
    /// Create a connected pair of mock channels
    pub fn pair(id_a: u64, id_b: u64) -> (Self, Self) {
        Self::pair_with_latency(id_a, id_b, 0)
    }

    /// Create a connected pair with simulated latency
    pub fn pair_with_latency(id_a: u64, id_b: u64, latency_ms: u64) -> (Self, Self) {
        let (tx_a, rx_a) = mpsc::channel(100);
        let (tx_b, rx_b) = mpsc::channel(100);

        let chan_a = Self {
            peer_id: id_a,
            tx: tx_b, // A sends to B's receiver
            rx: tokio::sync::Mutex::new(rx_a),
            open: AtomicBool::new(true),
            latency_ms,
        };

        let chan_b = Self {
            peer_id: id_b,
            tx: tx_a, // B sends to A's receiver
            rx: tokio::sync::Mutex::new(rx_b),
            open: AtomicBool::new(true),
            latency_ms,
        };

        (chan_a, chan_b)
    }

    /// Get peer ID
    pub fn peer_id(&self) -> u64 {
        self.peer_id
    }
}

#[async_trait]
impl DataChannel for MockDataChannel {
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError> {
        if !self.open.load(Ordering::Relaxed) {
            return Err(TransportError::Disconnected);
        }

        // Simulate latency
        if self.latency_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.latency_ms)).await;
        }

        self.tx
            .send(data)
            .await
            .map_err(|_| TransportError::Disconnected)
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        let mut rx = self.rx.lock().await;
        rx.recv().await
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    async fn close(&self) {
        self.open.store(false, Ordering::Relaxed);
    }
}

// ============================================================================
// Mock Peer Connection Factory
// ============================================================================

/// Mock peer connection factory
///
/// Creates mock data channels instead of real WebRTC connections.
/// Uses a global registry to connect offer/answer sides.
pub struct MockConnectionFactory {
    our_peer_id: String,
    our_node_id: u64,
    /// Simulated latency per link (ms)
    latency_ms: u64,
    /// Pending outbound channels (we sent offer, waiting for answer)
    pending: RwLock<HashMap<String, Arc<MockDataChannel>>>,
}

impl MockConnectionFactory {
    /// Create a new mock connection factory
    pub fn new(peer_id: String, latency_ms: u64) -> Self {
        let node_id = peer_id.parse().unwrap_or(0);
        Self {
            our_peer_id: peer_id,
            our_node_id: node_id,
            latency_ms,
            pending: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl PeerConnectionFactory for MockConnectionFactory {
    async fn create_offer(
        &self,
        target_peer_id: &str,
    ) -> Result<(Arc<dyn DataChannel>, String), TransportError> {
        let target_node_id: u64 = target_peer_id.parse().unwrap_or(0);

        // Create channel pair
        let (our_chan, their_chan) =
            MockDataChannel::pair_with_latency(self.our_node_id, target_node_id, self.latency_ms);
        let our_chan = Arc::new(our_chan);
        let their_chan = Arc::new(their_chan);

        // Channel ID is used to link offer/answer
        let channel_id = format!("{}_{}", self.our_peer_id, target_peer_id);

        // Store our channel for when answer comes back
        self.pending
            .write()
            .await
            .insert(target_peer_id.to_string(), our_chan.clone());

        // Store their channel in global registry for answerer to find
        CHANNEL_REGISTRY
            .write()
            .await
            .insert(channel_id.clone(), their_chan);

        Ok((our_chan, channel_id))
    }

    async fn accept_offer(
        &self,
        _from_peer_id: &str,
        offer_sdp: &str,
    ) -> Result<(Arc<dyn DataChannel>, String), TransportError> {
        // offer_sdp is the channel_id
        let channel_id = offer_sdp;

        // Get our channel from the registry
        let channel = CHANNEL_REGISTRY
            .write()
            .await
            .remove(channel_id)
            .ok_or_else(|| TransportError::ConnectionFailed("Channel not found".to_string()))?;

        // Answer SDP is just the channel ID (for mock, we don't need real SDP)
        Ok((channel, channel_id.to_string()))
    }

    async fn handle_answer(
        &self,
        target_peer_id: &str,
        _answer_sdp: &str,
    ) -> Result<Arc<dyn DataChannel>, TransportError> {
        // Get our pending channel
        let channel = self
            .pending
            .write()
            .await
            .remove(target_peer_id)
            .ok_or_else(|| {
                TransportError::ConnectionFailed("No pending connection".to_string())
            })?;

        Ok(channel)
    }
}
