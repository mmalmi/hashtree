//! Relay transport abstraction
//!
//! Defines traits for relay communication and peer connections that can be
//! implemented by both real (Nostr + WebRTC) and mock implementations.

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

use crate::types::SignalingMessage;

/// Errors from relay transport operations
#[derive(Debug, Error, Clone)]
pub enum TransportError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Send failed: {0}")]
    SendFailed(String),
    #[error("Receive failed: {0}")]
    ReceiveFailed(String),
    #[error("Timeout")]
    Timeout,
    #[error("Disconnected")]
    Disconnected,
    #[error("Not connected")]
    NotConnected,
}

/// Relay transport for signaling messages
///
/// Abstracts the relay connection (Nostr or mock) so signaling logic
/// can be shared between real and simulated implementations.
#[async_trait]
pub trait RelayTransport: Send + Sync {
    /// Connect to relays and start listening
    async fn connect(&self, relays: &[String]) -> Result<(), TransportError>;

    /// Disconnect from relays
    async fn disconnect(&self);

    /// Publish a signaling message to the relay
    /// If target is Some, message is directed; otherwise broadcast
    async fn publish(&self, msg: SignalingMessage) -> Result<(), TransportError>;

    /// Receive the next signaling message from the relay (blocking)
    async fn recv(&self) -> Option<SignalingMessage>;

    /// Try to receive without blocking
    fn try_recv(&self) -> Option<SignalingMessage>;

    /// Get our peer ID (uuid part)
    fn peer_id(&self) -> &str;

    /// Get our public key
    fn pubkey(&self) -> &str;
}

/// Data channel for peer-to-peer communication
///
/// Abstracts the data channel (WebRTC DataChannel or mock) so data
/// transfer logic can be shared.
#[async_trait]
pub trait DataChannel: Send + Sync {
    /// Send data to the peer
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError>;

    /// Receive data from the peer
    async fn recv(&self) -> Option<Vec<u8>>;

    /// Check if the channel is open
    fn is_open(&self) -> bool;

    /// Close the channel
    async fn close(&self);
}

/// Factory for creating peer connections
///
/// When we receive an offer and want to accept, or when we want to
/// initiate a connection, this factory creates the appropriate channel.
#[async_trait]
pub trait PeerConnectionFactory: Send + Sync {
    /// Create an outgoing connection (we initiate)
    /// Returns (our_channel, offer_sdp)
    async fn create_offer(
        &self,
        target_peer_id: &str,
    ) -> Result<(Arc<dyn DataChannel>, String), TransportError>;

    /// Accept an incoming connection (they initiated)
    /// Returns (our_channel, answer_sdp)
    async fn accept_offer(
        &self,
        from_peer_id: &str,
        offer_sdp: &str,
    ) -> Result<(Arc<dyn DataChannel>, String), TransportError>;

    /// Complete a connection after receiving answer
    async fn handle_answer(
        &self,
        target_peer_id: &str,
        answer_sdp: &str,
    ) -> Result<Arc<dyn DataChannel>, TransportError>;
}

/// Configuration for signaling behavior
#[derive(Debug, Clone)]
pub struct SignalingConfig {
    /// Our peer ID
    pub peer_id: String,
    /// Maximum number of peers to connect to
    pub max_peers: usize,
    /// Interval between hello broadcasts (ms)
    pub hello_interval_ms: u64,
    /// Root hashes to advertise in hello messages
    pub roots: Vec<String>,
    /// Enable debug logging
    pub debug: bool,
}

impl Default for SignalingConfig {
    fn default() -> Self {
        Self {
            peer_id: String::new(),
            max_peers: 10,
            hello_interval_ms: 30000,
            roots: Vec::new(),
            debug: false,
        }
    }
}

// Blanket implementations for Arc<T> to allow calling trait methods on Arc-wrapped transports

#[async_trait]
impl<T: RelayTransport + ?Sized> RelayTransport for Arc<T> {
    async fn connect(&self, relays: &[String]) -> Result<(), TransportError> {
        (**self).connect(relays).await
    }

    async fn disconnect(&self) {
        (**self).disconnect().await
    }

    async fn publish(&self, msg: SignalingMessage) -> Result<(), TransportError> {
        (**self).publish(msg).await
    }

    async fn recv(&self) -> Option<SignalingMessage> {
        (**self).recv().await
    }

    fn try_recv(&self) -> Option<SignalingMessage> {
        (**self).try_recv()
    }

    fn peer_id(&self) -> &str {
        (**self).peer_id()
    }

    fn pubkey(&self) -> &str {
        (**self).pubkey()
    }
}

#[async_trait]
impl<T: DataChannel + ?Sized> DataChannel for Arc<T> {
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError> {
        (**self).send(data).await
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        (**self).recv().await
    }

    fn is_open(&self) -> bool {
        (**self).is_open()
    }

    async fn close(&self) {
        (**self).close().await
    }
}
