//! Shared signaling logic for peer discovery and connection management
//!
//! This module contains the core signaling logic used by both production WebRTCStore
//! and simulation. It handles:
//! - Hello broadcasts and discovery
//! - Pool management (follows vs other peers)
//! - Tie-breaking for connection initiation
//! - Offer/answer flow coordination

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::transport::{DataChannel, PeerConnectionFactory, RelayTransport, TransportError};
use crate::types::{is_polite_peer, ClassifyRequest, PeerPool, PoolSettings, SignalingMessage};

/// Peer entry with pool classification and channel
pub struct PeerEntry {
    pub channel: Arc<dyn DataChannel>,
    pub pool: PeerPool,
}

/// Signaling manager handles peer discovery and connection establishment
///
/// This is the shared code between production and simulation.
/// It uses traits for transport (relay) and connection factory (WebRTC or mock).
///
/// Uses WebRTC "perfect negotiation" pattern:
/// - Both peers can send offers when they discover each other
/// - On collision (both sent offers), "polite" peer backs off and accepts incoming
/// - This ensures connections form even when one peer is satisfied but can accept
pub struct SignalingManager<R: RelayTransport, F: PeerConnectionFactory> {
    /// Our peer ID (pubkey:uuid format)
    peer_id: String,
    /// Our pubkey (stored for future use in debugging/logging)
    #[allow(dead_code)]
    pubkey: String,
    /// Relay transport for signaling
    relay: Arc<R>,
    /// Connection factory for creating data channels
    conn_factory: Arc<F>,
    /// Connected peers
    peers: RwLock<HashMap<String, PeerEntry>>,
    /// Pending outbound offers (we sent offer, waiting for answer)
    pending_offers: RwLock<HashMap<String, ()>>,
    /// Pool settings
    pools: PoolSettings,
    /// Known peer roots (for future use)
    peer_roots: RwLock<HashMap<String, Vec<String>>>,
    /// Classifier channel (optional)
    classifier_tx: Option<tokio::sync::mpsc::Sender<ClassifyRequest>>,
    /// Debug mode
    debug: bool,
}

impl<R: RelayTransport + 'static, F: PeerConnectionFactory + 'static> SignalingManager<R, F> {
    /// Create a new signaling manager
    pub fn new(
        peer_id: String,
        pubkey: String,
        relay: Arc<R>,
        conn_factory: Arc<F>,
        pools: PoolSettings,
        debug: bool,
    ) -> Self {
        Self {
            peer_id,
            pubkey,
            relay,
            conn_factory,
            peers: RwLock::new(HashMap::new()),
            pending_offers: RwLock::new(HashMap::new()),
            pools,
            peer_roots: RwLock::new(HashMap::new()),
            classifier_tx: None,
            debug,
        }
    }

    /// Set classifier for peer pool assignment
    pub fn set_classifier(&mut self, tx: tokio::sync::mpsc::Sender<ClassifyRequest>) {
        self.classifier_tx = Some(tx);
    }

    /// Get our peer ID
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// Send hello broadcast
    pub async fn send_hello(&self, roots: Vec<String>) -> Result<(), TransportError> {
        let msg = SignalingMessage::Hello {
            peer_id: self.peer_id.clone(),
            roots,
        };
        self.relay.publish(msg).await
    }

    /// Count peers by pool
    async fn count_pools(&self) -> (usize, usize) {
        let peers = self.peers.read().await;
        let mut follows = 0;
        let mut other = 0;
        for entry in peers.values() {
            match entry.pool {
                PeerPool::Follows => follows += 1,
                PeerPool::Other => other += 1,
            }
        }
        (follows, other)
    }

    /// Classify a peer by pubkey
    async fn classify_peer(&self, pubkey: &str) -> PeerPool {
        if let Some(ref tx) = self.classifier_tx {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let request = ClassifyRequest {
                pubkey: pubkey.to_string(),
                response: response_tx,
            };
            if tx.send(request).await.is_ok() {
                if let Ok(pool) = response_rx.await {
                    return pool;
                }
            }
        }
        PeerPool::Other
    }

    /// Check if we can accept a peer in a given pool
    fn can_accept_peer(&self, pool: PeerPool, follows: usize, other: usize) -> bool {
        match pool {
            PeerPool::Follows => self.pools.follows.can_accept(follows),
            PeerPool::Other => self.pools.other.can_accept(other),
        }
    }

    /// Check if a pool needs more peers
    fn pool_needs_peers(&self, pool: PeerPool, follows: usize, other: usize) -> bool {
        match pool {
            PeerPool::Follows => self.pools.follows.needs_peers(follows),
            PeerPool::Other => self.pools.other.needs_peers(other),
        }
    }

    /// Handle incoming signaling message
    ///
    /// This is the core signaling logic shared between production and simulation.
    pub async fn handle_message(&self, msg: SignalingMessage) -> Result<(), TransportError> {
        match &msg {
            SignalingMessage::Hello { peer_id, roots } => {
                self.handle_hello(peer_id, roots).await
            }
            SignalingMessage::Offer {
                peer_id,
                target_peer_id,
                sdp,
            } => {
                if target_peer_id == &self.peer_id {
                    self.handle_offer(peer_id, sdp).await
                } else {
                    Ok(()) // Not for us
                }
            }
            SignalingMessage::Answer {
                peer_id,
                target_peer_id,
                sdp,
            } => {
                if target_peer_id == &self.peer_id {
                    self.handle_answer(peer_id, sdp).await
                } else {
                    Ok(()) // Not for us
                }
            }
            SignalingMessage::Candidate { .. } | SignalingMessage::Candidates { .. } => {
                // ICE candidates are handled by the connection factory
                // For mocks, these are no-ops
                Ok(())
            }
        }
    }

    /// Handle hello message - peer discovery (perfect negotiation)
    ///
    /// With perfect negotiation, we send an offer if we need peers.
    /// No tie-breaking here - collisions are handled in handle_offer.
    async fn handle_hello(
        &self,
        from_peer_id: &str,
        roots: &[String],
    ) -> Result<(), TransportError> {
        // Ignore our own hello
        if from_peer_id == self.peer_id {
            return Ok(());
        }

        // Extract pubkey from peer_id (format: "pubkey:uuid")
        let peer_pubkey = from_peer_id.split(':').next().unwrap_or("");

        // Classify the peer
        let pool = self.classify_peer(peer_pubkey).await;

        // Check pool limits
        let (follows_count, other_count) = self.count_pools().await;

        if !self.can_accept_peer(pool, follows_count, other_count) {
            if self.debug {
                println!(
                    "[Signaling] Ignoring hello from {} - {:?} pool full",
                    from_peer_id, pool
                );
            }
            return Ok(());
        }

        // Store peer roots
        self.peer_roots
            .write()
            .await
            .insert(from_peer_id.to_string(), roots.to_vec());

        // Perfect negotiation: send offer if we NEED more peers
        // Both sides may send offers - collision handled in handle_offer
        if self.pool_needs_peers(pool, follows_count, other_count) {
            // Check if already connected or pending
            if self.peers.read().await.contains_key(from_peer_id) {
                return Ok(());
            }
            if self.pending_offers.read().await.contains_key(from_peer_id) {
                return Ok(());
            }

            if self.debug {
                println!(
                    "[Signaling] Sending offer to {} (pool: {:?})",
                    from_peer_id, pool
                );
            }

            // Mark as pending before creating offer
            self.pending_offers
                .write()
                .await
                .insert(from_peer_id.to_string(), ());

            // Create offer
            let (channel, sdp) = self.conn_factory.create_offer(from_peer_id).await?;

            // Add peer (will be confirmed when we get answer)
            self.peers.write().await.insert(
                from_peer_id.to_string(),
                PeerEntry { channel, pool },
            );

            // Send offer
            let offer_msg = SignalingMessage::Offer {
                peer_id: self.peer_id.clone(),
                target_peer_id: from_peer_id.to_string(),
                sdp,
            };
            self.relay.publish(offer_msg).await?;
        }

        Ok(())
    }

    /// Handle offer message (perfect negotiation)
    ///
    /// Handles offer collision: if we also sent an offer to this peer,
    /// the "polite" peer (lower ID) backs off and accepts the incoming offer.
    async fn handle_offer(&self, from_peer_id: &str, sdp: &str) -> Result<(), TransportError> {
        // Extract pubkey
        let peer_pubkey = from_peer_id.split(':').next().unwrap_or("");

        // Classify and check limits
        let pool = self.classify_peer(peer_pubkey).await;
        let (follows_count, other_count) = self.count_pools().await;

        if !self.can_accept_peer(pool, follows_count, other_count) {
            if self.debug {
                println!(
                    "[Signaling] Ignoring offer from {} - {:?} pool full",
                    from_peer_id, pool
                );
            }
            return Ok(());
        }

        // Check for offer collision (we also sent an offer to them)
        let have_pending = self.pending_offers.read().await.contains_key(from_peer_id);
        if have_pending {
            // Collision! Use polite/impolite pattern
            let we_are_polite = is_polite_peer(&self.peer_id, from_peer_id);

            if we_are_polite {
                // We're polite - back off, accept their offer
                // Remove our pending offer and peer entry
                self.pending_offers.write().await.remove(from_peer_id);
                self.peers.write().await.remove(from_peer_id);

                if self.debug {
                    println!(
                        "[Signaling] Collision with {} - we're polite, accepting their offer",
                        from_peer_id
                    );
                }
            } else {
                // We're impolite - ignore their offer, wait for answer to ours
                if self.debug {
                    println!(
                        "[Signaling] Collision with {} - we're impolite, ignoring their offer",
                        from_peer_id
                    );
                }
                return Ok(());
            }
        }

        // Check if already connected (no collision case)
        if self.peers.read().await.contains_key(from_peer_id) {
            return Ok(());
        }

        if self.debug {
            println!("[Signaling] Accepting offer from {}", from_peer_id);
        }

        // Accept offer
        let (channel, answer_sdp) = self.conn_factory.accept_offer(from_peer_id, sdp).await?;

        // Add peer
        self.peers.write().await.insert(
            from_peer_id.to_string(),
            PeerEntry { channel, pool },
        );

        // Send answer
        let answer_msg = SignalingMessage::Answer {
            peer_id: self.peer_id.clone(),
            target_peer_id: from_peer_id.to_string(),
            sdp: answer_sdp,
        };
        self.relay.publish(answer_msg).await?;

        Ok(())
    }

    /// Handle answer message
    async fn handle_answer(&self, from_peer_id: &str, sdp: &str) -> Result<(), TransportError> {
        if self.debug {
            println!("[Signaling] Received answer from {}", from_peer_id);
        }

        // Complete connection
        let _channel = self.conn_factory.handle_answer(from_peer_id, sdp).await?;

        // Peer should already be in our map from when we sent the offer
        // The channel returned here is the same one we stored

        Ok(())
    }

    /// Get connected peer count
    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    /// Get peer IDs
    pub async fn peer_ids(&self) -> Vec<String> {
        self.peers.read().await.keys().cloned().collect()
    }

    /// Get a peer's channel
    pub async fn get_channel(&self, peer_id: &str) -> Option<Arc<dyn DataChannel>> {
        self.peers.read().await.get(peer_id).map(|e| e.channel.clone())
    }

    /// Check if we need more peers (below satisfied in any pool)
    pub async fn needs_peers(&self) -> bool {
        let (follows, other) = self.count_pools().await;
        self.pools.follows.needs_peers(follows) || self.pools.other.needs_peers(other)
    }

    /// Check if we can accept more peers (below max in any pool)
    pub async fn can_accept(&self) -> bool {
        let (follows, other) = self.count_pools().await;
        self.pools.follows.can_accept(follows) || self.pools.other.can_accept(other)
    }
}
