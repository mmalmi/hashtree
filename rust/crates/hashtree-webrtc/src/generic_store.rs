//! Generic P2P store using abstract transports
//!
//! This module provides a Store implementation that works with any
//! RelayTransport and PeerConnectionFactory. Both production (real WebRTC)
//! and simulation (mocks) use this same code.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, RwLock};

use hashtree_core::{Hash, Store, StoreError};

use crate::protocol::{create_request, create_response, encode_request, encode_response, hash_to_key, parse_message, DataMessage};
use crate::signaling::SignalingManager;
use crate::transport::{PeerConnectionFactory, RelayTransport, TransportError};
use crate::types::{SignalingMessage, MAX_HTL, PeerHTLConfig};

/// Pending request awaiting response
struct PendingRequest {
    response_tx: oneshot::Sender<Option<Vec<u8>>>,
}

/// Generic P2P store that works with any transport implementation
///
/// This is the shared code between production and simulation.
/// - Production: GenericStore<NostrRelayTransport, RealPeerConnectionFactory>
/// - Simulation: GenericStore<MockRelayTransport, MockConnectionFactory>
pub struct GenericStore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: RelayTransport + Send + Sync + 'static,
    F: PeerConnectionFactory + Send + Sync + 'static,
{
    /// Local backing store
    local_store: Arc<S>,
    /// Signaling manager (handles peer discovery and connection)
    signaling: Arc<SignalingManager<R, F>>,
    /// Per-peer HTL config
    htl_configs: RwLock<HashMap<String, PeerHTLConfig>>,
    /// Pending requests we sent
    pending_requests: RwLock<HashMap<String, PendingRequest>>,
    /// Request timeout
    request_timeout: Duration,
    /// Debug mode
    debug: bool,
    /// Running flag
    running: RwLock<bool>,
}

impl<S, R, F> GenericStore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: RelayTransport + Send + Sync + 'static,
    F: PeerConnectionFactory + Send + Sync + 'static,
{
    /// Create a new generic store
    pub fn new(
        local_store: Arc<S>,
        signaling: Arc<SignalingManager<R, F>>,
        request_timeout: Duration,
        debug: bool,
    ) -> Self {
        Self {
            local_store,
            signaling,
            htl_configs: RwLock::new(HashMap::new()),
            pending_requests: RwLock::new(HashMap::new()),
            request_timeout,
            debug,
            running: RwLock::new(false),
        }
    }

    /// Start the store (begin listening for messages)
    pub async fn start(&self) -> Result<(), TransportError> {
        *self.running.write().await = true;

        // Send initial hello
        self.signaling.send_hello(vec![]).await?;

        Ok(())
    }

    /// Stop the store
    pub async fn stop(&self) {
        *self.running.write().await = false;
    }

    /// Process incoming signaling message
    pub async fn process_signaling(&self, msg: SignalingMessage) -> Result<(), TransportError> {
        // When a new peer connects, initialize their HTL config
        let peer_id = msg.peer_id().to_string();
        {
            let mut configs = self.htl_configs.write().await;
            if !configs.contains_key(&peer_id) {
                configs.insert(peer_id, PeerHTLConfig::random());
            }
        }

        self.signaling.handle_message(msg).await
    }

    /// Get signaling manager reference
    pub fn signaling(&self) -> &Arc<SignalingManager<R, F>> {
        &self.signaling
    }

    /// Get peer count
    pub async fn peer_count(&self) -> usize {
        self.signaling.peer_count().await
    }

    /// Check if we need more peers
    pub async fn needs_peers(&self) -> bool {
        self.signaling.needs_peers().await
    }

    /// Request data from peers
    async fn request_from_peers(&self, hash: &Hash) -> Option<Vec<u8>> {
        let peer_ids = self.signaling.peer_ids().await;
        if peer_ids.is_empty() {
            return None;
        }

        let hash_key = hash_to_key(hash);

        // Try each peer
        for peer_id in peer_ids {
            let channel = match self.signaling.get_channel(&peer_id).await {
                Some(c) => c,
                None => continue,
            };

            // Get HTL config for this peer
            let htl_config = {
                let configs = self.htl_configs.read().await;
                configs.get(&peer_id).cloned().unwrap_or_else(PeerHTLConfig::random)
            };

            // Create request
            let send_htl = htl_config.decrement(MAX_HTL);
            let req = create_request(hash, send_htl);
            let request_bytes = encode_request(&req);

            // Setup response channel
            let (tx, rx) = oneshot::channel();
            self.pending_requests
                .write()
                .await
                .insert(hash_key.clone(), PendingRequest { response_tx: tx });

            // Send request
            if let Err(e) = channel.send(request_bytes).await {
                if self.debug {
                    println!("[GenericStore] Failed to send request: {:?}", e);
                }
                self.pending_requests.write().await.remove(&hash_key);
                continue;
            }

            // Wait for response
            match tokio::time::timeout(self.request_timeout, rx).await {
                Ok(Ok(Some(data))) => {
                    // Verify hash
                    if hashtree_core::sha256(&data) == *hash {
                        // Cache locally
                        let _ = self.local_store.put(*hash, data.clone()).await;
                        return Some(data);
                    }
                }
                Ok(Ok(None)) => {
                    // Peer doesn't have data
                    continue;
                }
                _ => {
                    // Timeout or error
                    self.pending_requests.write().await.remove(&hash_key);
                    continue;
                }
            }
        }

        None
    }

    /// Handle incoming data message
    pub async fn handle_data_message(&self, from_peer: &str, data: &[u8]) {
        let parsed = match parse_message(data) {
            Some(m) => m,
            None => return,
        };

        match parsed {
            DataMessage::Request(req) => {
                let hash = match crate::protocol::bytes_to_hash(&req.h) {
                    Some(h) => h,
                    None => return,
                };

                // Check local store
                if let Ok(Some(data)) = self.local_store.get(&hash).await {
                    // Send response
                    let res = create_response(&hash, data);
                    let response_bytes = encode_response(&res);
                    if let Some(channel) = self.signaling.get_channel(from_peer).await {
                        let _ = channel.send(response_bytes).await;
                    }
                }
                // For now, don't forward - keep it simple
            }
            DataMessage::Response(res) => {
                let hash_key = hash_to_key(&res.h);

                // Resolve pending request
                if let Some(pending) = self.pending_requests.write().await.remove(&hash_key) {
                    // Verify hash
                    let hash = match crate::protocol::bytes_to_hash(&res.h) {
                        Some(h) => h,
                        None => {
                            let _ = pending.response_tx.send(None);
                            return;
                        }
                    };

                    if hashtree_core::sha256(&res.d) == hash {
                        let _ = pending.response_tx.send(Some(res.d));
                    } else {
                        let _ = pending.response_tx.send(None);
                    }
                }
            }
        }
    }
}

#[async_trait]
impl<S, R, F> Store for GenericStore<S, R, F>
where
    S: Store + Send + Sync + 'static,
    R: RelayTransport + Send + Sync + 'static,
    F: PeerConnectionFactory + Send + Sync + 'static,
{
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local_store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        // Try local first
        if let Some(data) = self.local_store.get(hash).await? {
            return Ok(Some(data));
        }

        // Try peers
        Ok(self.request_from_peers(hash).await)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.delete(hash).await
    }
}

/// Type alias for simulation store
pub type SimStore<S> = GenericStore<S, crate::mock::MockRelayTransport, crate::mock::MockConnectionFactory>;

/// Type alias for production store (using real WebRTC)
pub type ProductionStore<S> = GenericStore<S, crate::nostr::NostrRelayTransport, crate::real_factory::RealPeerConnectionFactory>;
