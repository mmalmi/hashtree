//! Shared signaling logic for peer discovery and connection management.
//!
//! This module contains the core signaling logic used by both the default
//! production mesh transport stack and simulation. It handles:
//! - Hello broadcasts and discovery
//! - Pool management (follows vs other peers)
//! - Tie-breaking for connection initiation
//! - Offer/answer flow coordination

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::transport::{PeerLink, PeerLinkFactory, SignalingTransport, TransportError};
use crate::types::{is_polite_peer, ClassifyRequest, PeerPool, PoolSettings, SignalingMessage};

/// Peer entry with pool classification and channel
pub struct PeerEntry {
    pub channel: Arc<dyn PeerLink>,
    pub pool: PeerPool,
    pub hash_get: bool,
}

/// Mesh router handles peer discovery and negotiated link establishment.
///
/// This is the shared routing logic between production transports and simulation.
/// It uses traits for signaling transport and negotiated link factories so the
/// same router can drive Nostr websockets, LAN buses, BLE, WebRTC, or mocks.
///
/// Uses the standard concurrent-offer "perfect negotiation" pattern:
/// - Both peers can send offers when they discover each other
/// - On collision (both sent offers), "polite" peer backs off and accepts incoming
/// - This ensures connections form even when one peer is satisfied but can accept
pub struct MeshRouter<R: SignalingTransport, F: PeerLinkFactory> {
    /// Our peer ID (pubkey format)
    peer_id: String,
    /// Relay transport for signaling
    transport: Arc<R>,
    /// Link factory for creating negotiated peer links
    conn_factory: Arc<F>,
    /// Connected peers
    peers: RwLock<HashMap<String, PeerEntry>>,
    /// Pending outbound offers (we sent offer, waiting for answer)
    pending_offers: RwLock<HashMap<String, ()>>,
    /// Pool settings
    pools: PoolSettings,
    /// Known peer roots (for future use)
    peer_roots: RwLock<HashMap<String, Vec<String>>>,
    /// Last advertised `hash_get` capability per peer.
    peer_hash_get: RwLock<HashMap<String, bool>>,
    /// Classifier channel (optional)
    classifier_tx: Option<tokio::sync::mpsc::Sender<ClassifyRequest>>,
    /// Debug mode
    debug: bool,
    /// Whether local node accepts `hash_get` lookups.
    hash_get_enabled: bool,
}

impl<R: SignalingTransport + 'static, F: PeerLinkFactory + 'static> MeshRouter<R, F> {
    /// Create a new mesh router.
    pub fn new(
        peer_id: String,
        transport: Arc<R>,
        conn_factory: Arc<F>,
        pools: PoolSettings,
        debug: bool,
    ) -> Self {
        Self {
            peer_id,
            transport,
            conn_factory,
            peers: RwLock::new(HashMap::new()),
            pending_offers: RwLock::new(HashMap::new()),
            pools,
            peer_roots: RwLock::new(HashMap::new()),
            peer_hash_get: RwLock::new(HashMap::new()),
            classifier_tx: None,
            debug,
            hash_get_enabled: true,
        }
    }

    /// Set classifier for peer pool assignment
    pub fn set_classifier(&mut self, tx: tokio::sync::mpsc::Sender<ClassifyRequest>) {
        self.classifier_tx = Some(tx);
    }

    pub fn set_hash_get_enabled(&mut self, enabled: bool) {
        self.hash_get_enabled = enabled;
    }

    pub async fn set_peer_hash_get(&self, peer_id: &str, enabled: bool) {
        self.peer_hash_get
            .write()
            .await
            .insert(peer_id.to_string(), enabled);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.hash_get = enabled;
        }
    }

    pub async fn peer_supports_hash_get(&self, peer_id: &str) -> bool {
        self.peer_hash_get
            .read()
            .await
            .get(peer_id)
            .copied()
            .unwrap_or(true)
    }

    pub async fn hash_get_peer_ids(&self) -> Vec<String> {
        let peers = self.peers.read().await;
        let peer_hash_get = self.peer_hash_get.read().await;
        peers
            .keys()
            .filter(|peer_id| peer_hash_get.get(*peer_id).copied().unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Get our peer ID
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// Send hello broadcast
    pub async fn send_hello(&self, roots: Vec<String>) -> Result<(), TransportError> {
        self.send_hello_with_addresses(roots, Vec::new()).await
    }

    /// Send hello broadcast with direct peer address hints.
    pub async fn send_hello_with_addresses(
        &self,
        roots: Vec<String>,
        addresses: Vec<String>,
    ) -> Result<(), TransportError> {
        let msg = SignalingMessage::Hello {
            peer_id: self.peer_id.clone(),
            roots,
            hash_get: self.hash_get_enabled,
            addresses,
        };
        self.transport.publish(msg).await
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
            SignalingMessage::Hello {
                peer_id,
                roots,
                hash_get,
                ..
            } => {
                self.set_peer_hash_get(peer_id, *hash_get).await;
                self.handle_hello(peer_id, roots, *hash_get).await
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
            SignalingMessage::Candidate {
                peer_id,
                target_peer_id,
                candidate,
                sdp_m_line_index,
                sdp_mid,
            } => {
                if target_peer_id == &self.peer_id {
                    self.conn_factory
                        .handle_candidate(
                            peer_id,
                            crate::types::IceCandidate {
                                candidate: candidate.clone(),
                                sdp_m_line_index: *sdp_m_line_index,
                                sdp_mid: sdp_mid.clone(),
                            },
                        )
                        .await
                } else {
                    Ok(())
                }
            }
            SignalingMessage::Candidates {
                peer_id,
                target_peer_id,
                candidates,
            } => {
                if target_peer_id == &self.peer_id {
                    self.conn_factory
                        .handle_candidates(peer_id, candidates.clone())
                        .await
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Handle hello message using the shared concurrent-offer negotiation flow.
    ///
    /// With perfect negotiation, we send an offer if we need peers.
    /// No tie-breaking here - collisions are handled in handle_offer.
    async fn handle_hello(
        &self,
        from_peer_id: &str,
        roots: &[String],
        hash_get: bool,
    ) -> Result<(), TransportError> {
        // Ignore our own hello
        if from_peer_id == self.peer_id {
            return Ok(());
        }

        self.peer_roots
            .write()
            .await
            .insert(from_peer_id.to_string(), roots.to_vec());
        if let Some(entry) = self.peers.write().await.get_mut(from_peer_id) {
            entry.hash_get = hash_get;
            return Ok(());
        }

        let peer_pubkey = crate::types::PeerId::from_peer_string(from_peer_id)
            .map(|peer_id| peer_id.pubkey)
            .unwrap_or_else(|| from_peer_id.to_string());

        // Classify the peer
        let pool = self.classify_peer(&peer_pubkey).await;

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

        // Shared perfect negotiation: send offer if we NEED more peers
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
                PeerEntry {
                    channel,
                    pool,
                    hash_get,
                },
            );

            // Send offer
            let offer_msg = SignalingMessage::Offer {
                peer_id: self.peer_id.clone(),
                target_peer_id: from_peer_id.to_string(),
                sdp,
            };
            self.transport.publish(offer_msg).await?;
        }

        Ok(())
    }

    /// Handle offer message in the shared concurrent-offer negotiation flow.
    ///
    /// Handles offer collision: if we also sent an offer to this peer,
    /// the "polite" peer (lower ID) backs off and accepts the incoming offer.
    async fn handle_offer(&self, from_peer_id: &str, sdp: &str) -> Result<(), TransportError> {
        // Extract pubkey
        let peer_pubkey = crate::types::PeerId::from_peer_string(from_peer_id)
            .map(|peer_id| peer_id.pubkey)
            .unwrap_or_else(|| from_peer_id.to_string());

        // Classify and check limits
        let pool = self.classify_peer(&peer_pubkey).await;
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
        let hash_get = self.peer_supports_hash_get(from_peer_id).await;

        // Add peer
        self.peers.write().await.insert(
            from_peer_id.to_string(),
            PeerEntry {
                channel,
                pool,
                hash_get,
            },
        );

        // Send answer
        let answer_msg = SignalingMessage::Answer {
            peer_id: self.peer_id.clone(),
            target_peer_id: from_peer_id.to_string(),
            sdp: answer_sdp,
        };
        self.transport.publish(answer_msg).await?;

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
    pub async fn get_channel(&self, peer_id: &str) -> Option<Arc<dyn PeerLink>> {
        self.peers
            .read()
            .await
            .get(peer_id)
            .map(|e| e.channel.clone())
    }

    /// Remove a peer and any pending offer state.
    pub async fn remove_peer(&self, peer_id: &str) -> Option<Arc<dyn PeerLink>> {
        self.pending_offers.write().await.remove(peer_id);
        self.peer_roots.write().await.remove(peer_id);
        let _ = self.conn_factory.remove_peer(peer_id).await;
        self.peers
            .write()
            .await
            .remove(peer_id)
            .map(|entry| entry.channel)
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    use crate::types::{IceCandidate, PoolConfig, PoolSettings};

    #[derive(Default)]
    struct NoopTransport;

    #[async_trait]
    impl SignalingTransport for NoopTransport {
        async fn connect(&self, _relays: &[String]) -> Result<(), TransportError> {
            Ok(())
        }

        async fn disconnect(&self) {}

        async fn publish(&self, _msg: SignalingMessage) -> Result<(), TransportError> {
            Ok(())
        }

        async fn recv(&self) -> Option<SignalingMessage> {
            None
        }

        fn try_recv(&self) -> Option<SignalingMessage> {
            None
        }

        fn peer_id(&self) -> &str {
            "local"
        }
    }

    #[derive(Default)]
    struct RecordingFactory {
        candidates: Mutex<Vec<(String, IceCandidate)>>,
        removed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PeerLinkFactory for RecordingFactory {
        async fn create_offer(
            &self,
            _target_peer_id: &str,
        ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
            Err(TransportError::ConnectionFailed(
                "not used in this test".to_string(),
            ))
        }

        async fn accept_offer(
            &self,
            _from_peer_id: &str,
            _offer_sdp: &str,
        ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
            Err(TransportError::ConnectionFailed(
                "not used in this test".to_string(),
            ))
        }

        async fn handle_answer(
            &self,
            _target_peer_id: &str,
            _answer_sdp: &str,
        ) -> Result<Arc<dyn PeerLink>, TransportError> {
            Err(TransportError::ConnectionFailed(
                "not used in this test".to_string(),
            ))
        }

        async fn handle_candidate(
            &self,
            peer_id: &str,
            candidate: IceCandidate,
        ) -> Result<(), TransportError> {
            self.candidates
                .lock()
                .await
                .push((peer_id.to_string(), candidate));
            Ok(())
        }

        async fn remove_peer(&self, peer_id: &str) -> Result<(), TransportError> {
            self.removed.lock().await.push(peer_id.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn routes_targeted_candidates_to_factory() {
        let router = MeshRouter::new(
            "local".to_string(),
            Arc::new(NoopTransport),
            Arc::new(RecordingFactory::default()),
            PoolSettings {
                follows: PoolConfig::default(),
                other: PoolConfig::default(),
            },
            false,
        );

        router
            .handle_message(SignalingMessage::Candidate {
                peer_id: "remote:peer".to_string(),
                target_peer_id: "local".to_string(),
                candidate: "candidate:1".to_string(),
                sdp_m_line_index: Some(0),
                sdp_mid: Some("data".to_string()),
            })
            .await
            .expect("candidate should route");

        let factory = router.conn_factory.clone();
        let recorded = factory
            .candidates
            .lock()
            .await
            .iter()
            .map(|(peer_id, candidate)| (peer_id.clone(), candidate.candidate.clone()))
            .collect::<Vec<_>>();

        assert_eq!(
            recorded,
            vec![("remote:peer".to_string(), "candidate:1".to_string())]
        );
    }

    #[tokio::test]
    async fn remove_peer_cleans_factory_state() {
        let factory = Arc::new(RecordingFactory::default());
        let router = MeshRouter::new(
            "local".to_string(),
            Arc::new(NoopTransport),
            factory.clone(),
            PoolSettings {
                follows: PoolConfig::default(),
                other: PoolConfig::default(),
            },
            false,
        );

        let removed = router.remove_peer("remote:peer").await;
        assert!(removed.is_none());
        assert_eq!(
            factory.removed.lock().await.as_slice(),
            &["remote:peer".to_string()]
        );
    }
}
