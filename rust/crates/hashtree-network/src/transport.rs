//! Signaling and peer-link transport abstractions
//!
//! Defines traits for signaling transports and direct peer links that can be
//! implemented by Nostr websockets, LAN buses, FIPS, BLE, or mocks.

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

use crate::types::{IceCandidate, SignalingMessage};

/// Errors from signaling and peer-link transport operations.
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

/// Signaling transport for peer discovery and negotiation messages.
///
/// Abstracts the message bus used to exchange signaling frames so router logic
/// can be shared between production and simulation.
#[async_trait]
pub trait SignalingTransport: Send + Sync {
    /// Connect the signaling transport and start listening.
    async fn connect(&self, relays: &[String]) -> Result<(), TransportError>;

    /// Disconnect from the signaling transport.
    async fn disconnect(&self);

    /// Publish a signaling message to the transport.
    async fn publish(&self, msg: SignalingMessage) -> Result<(), TransportError>;

    /// Receive the next signaling message (blocking).
    async fn recv(&self) -> Option<SignalingMessage>;

    /// Try to receive without blocking.
    fn try_recv(&self) -> Option<SignalingMessage>;

    /// Get our peer ID.
    fn peer_id(&self) -> &str;
}

/// Bidirectional peer link for direct data exchange.
///
/// Abstracts the underlying byte stream so the data protocol can be shared
/// across FIPS, BLE, and mock links.
#[async_trait]
pub trait PeerLink: Send + Sync {
    /// Send data to the peer.
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError>;

    /// Receive data from the peer.
    async fn recv(&self) -> Option<Vec<u8>>;

    /// Try to receive data without blocking.
    /// Returns `None` when no message is currently available.
    fn try_recv(&self) -> Option<Vec<u8>> {
        None
    }

    /// Check if the link is open.
    fn is_open(&self) -> bool;

    /// Close the link.
    async fn close(&self);
}

/// Factory for creating negotiated direct peer links.
///
/// When we receive an offer and want to accept, or when we want to
/// initiate a connection, this factory creates the appropriate link.
#[async_trait]
pub trait PeerLinkFactory: Send + Sync {
    /// Create an outgoing negotiated link.
    /// Returns `(our_link, offer_sdp)`.
    async fn create_offer(
        &self,
        target_peer_id: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError>;

    /// Accept an incoming negotiated link.
    /// Returns `(our_link, answer_sdp)`.
    async fn accept_offer(
        &self,
        from_peer_id: &str,
        offer_sdp: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError>;

    /// Complete a link after receiving the answer.
    async fn handle_answer(
        &self,
        target_peer_id: &str,
        answer_sdp: &str,
    ) -> Result<Arc<dyn PeerLink>, TransportError>;

    /// Apply a trickle candidate update for an in-flight link, if relevant.
    async fn handle_candidate(
        &self,
        _peer_id: &str,
        _candidate: IceCandidate,
    ) -> Result<(), TransportError> {
        Ok(())
    }

    /// Apply a batch of trickle candidate updates.
    async fn handle_candidates(
        &self,
        peer_id: &str,
        candidates: Vec<IceCandidate>,
    ) -> Result<(), TransportError> {
        for candidate in candidates {
            self.handle_candidate(peer_id, candidate).await?;
        }
        Ok(())
    }

    /// Drop any factory-owned state for a peer that has been removed.
    async fn remove_peer(&self, _peer_id: &str) -> Result<(), TransportError> {
        Ok(())
    }
}

// Blanket implementations for Arc<T> to allow calling trait methods on Arc-wrapped transports

#[async_trait]
impl<T: SignalingTransport + ?Sized> SignalingTransport for Arc<T> {
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
}

#[async_trait]
impl<T: PeerLink + ?Sized> PeerLink for Arc<T> {
    async fn send(&self, data: Vec<u8>) -> Result<(), TransportError> {
        (**self).send(data).await
    }

    async fn recv(&self) -> Option<Vec<u8>> {
        (**self).recv().await
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        (**self).try_recv()
    }

    fn is_open(&self) -> bool {
        (**self).is_open()
    }

    async fn close(&self) {
        (**self).close().await
    }
}
