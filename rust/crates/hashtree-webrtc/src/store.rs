//! WebRTC-backed store implementation
//!
//! Implements the Store trait by fetching data from connected WebRTC peers.
//! Uses Nostr relays for peer discovery and signaling.

use crate::peer::{Peer, PeerError};
use crate::peer_selector::PeerSelector;
use crate::types::{
    ClassifyRequest, ForwardRx, ForwardTx, PeerId, PeerPool, PeerState, SignalingMessage,
    WebRTCStats, WebRTCStoreConfig, NOSTR_KIND_HASHTREE,
};
use async_trait::async_trait;
use hashtree_core::{to_hex, Hash, Store, StoreError};
use nostr_sdk::prelude::*;
use nostr_sdk::ClientBuilder;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, RwLock};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum WebRTCStoreError {
    #[error("Peer error: {0}")]
    Peer(#[from] PeerError),
    #[error("Nostr error: {0}")]
    Nostr(String),
    #[error("No peers available")]
    NoPeers,
    #[error("Data not found")]
    NotFound,
    #[error("Store error: {0}")]
    Store(#[from] StoreError),
}

/// Peer entry with pool classification
struct PeerEntry<S: Store> {
    peer: Arc<Peer<S>>,
    pool: PeerPool,
}

/// WebRTC store that fetches data from P2P network
pub struct WebRTCStore<S: Store> {
    /// Local backing store
    local_store: Arc<S>,
    /// Configuration
    config: WebRTCStoreConfig,
    /// Nostr client for signaling
    client: Option<Client>,
    /// Local peer identifier
    peer_id: PeerId,
    /// Connected peers with pool classification
    peers: Arc<RwLock<HashMap<String, PeerEntry<S>>>>,
    /// Known peer roots (peer_id -> Vec<root_hash>)
    peer_roots: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Signaling message sender
    signaling_tx: mpsc::Sender<SignalingMessage>,
    /// Signaling message receiver
    signaling_rx: Arc<RwLock<Option<mpsc::Receiver<SignalingMessage>>>>,
    /// Forward request sender (for peers to request forwarding)
    forward_tx: ForwardTx,
    /// Forward request receiver
    forward_rx: Arc<RwLock<Option<ForwardRx>>>,
    /// Running flag
    running: Arc<RwLock<bool>>,
    /// Statistics
    stats: Arc<RwLock<WebRTCStats>>,
    /// Adaptive peer selector for intelligent peer ordering
    peer_selector: Arc<RwLock<PeerSelector>>,
}

impl<S: Store + 'static> WebRTCStore<S> {
    /// Create a new WebRTC store
    pub fn new(local_store: Arc<S>, config: WebRTCStoreConfig) -> Self {
        let (signaling_tx, signaling_rx) = mpsc::channel(100);
        let (forward_tx, forward_rx) = mpsc::channel(100);

        let peer_id = PeerId::new(String::new(), Uuid::new_v4().to_string());

        Self {
            local_store,
            config,
            client: None,
            peer_id,
            peers: Arc::new(RwLock::new(HashMap::new())),
            peer_roots: Arc::new(RwLock::new(HashMap::new())),
            signaling_tx,
            signaling_rx: Arc::new(RwLock::new(Some(signaling_rx))),
            forward_tx,
            forward_rx: Arc::new(RwLock::new(Some(forward_rx))),
            running: Arc::new(RwLock::new(false)),
            stats: Arc::new(RwLock::new(WebRTCStats::default())),
            peer_selector: Arc::new(RwLock::new(PeerSelector::new())),
        }
    }

    /// Get the forward request sender (for passing to peers)
    pub fn forward_tx(&self) -> ForwardTx {
        self.forward_tx.clone()
    }

    /// Start the WebRTC store (connect to relays, begin peer discovery)
    pub async fn start(&mut self, keys: Keys) -> Result<(), WebRTCStoreError> {
        // Update peer ID with actual pubkey
        self.peer_id.pubkey = keys.public_key().to_hex();

        // Create Nostr client with its own separate database to avoid event deduplication
        // across multiple clients in the same process (important for tests)
        let client = ClientBuilder::new()
            .signer(keys.clone())
            .database(nostr_sdk::database::MemoryDatabase::new())
            .build();

        // Add relays
        for relay in &self.config.relays {
            client
                .add_relay(relay)
                .await
                .map_err(|e| WebRTCStoreError::Nostr(e.to_string()))?;
        }

        // Connect to relays
        client.connect().await;

        self.client = Some(client.clone());
        *self.running.write().await = true;

        // Subscribe to hashtree signaling events
        // Filter by our pubkey in #p tag to only get events meant for us (or broadcasts)
        let filter = Filter::new()
            .kind(Kind::Custom(NOSTR_KIND_HASHTREE))
            .since(Timestamp::now());

        client
            .subscribe(vec![filter], None)
            .await
            .map_err(|e| WebRTCStoreError::Nostr(e.to_string()))?;

        // Send initial hello
        self.send_hello().await?;

        // Start background tasks
        self.start_event_handler(client.clone()).await;
        self.start_signaling_sender(client).await;
        self.start_hello_timer().await;
        self.start_forward_handler().await;

        Ok(())
    }

    /// Start handler for forward requests from peers
    async fn start_forward_handler(&self) {
        let mut rx = self.forward_rx.write().await.take().unwrap();
        let peers = self.peers.clone();
        let peer_selector = self.peer_selector.clone();
        let local_store = self.local_store.clone();
        let running = self.running.clone();
        let debug = self.config.debug;

        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                if !*running.read().await {
                    break;
                }

                if debug {
                    println!(
                        "[Store] Forward request: hash={}..., htl={}, exclude={}",
                        &to_hex(&req.hash)[..16],
                        req.htl,
                        &req.exclude_peer_id[..req.exclude_peer_id.len().min(16)]
                    );
                }

                // Get ordered peer list from selector
                let ordered_peer_ids = peer_selector.write().await.select_peers();

                // Get other peers (excluding the requester), prioritize follows, use selector order
                let peers_read = peers.read().await;
                let mut follows_peers: Vec<(String, Arc<Peer<S>>)> = Vec::new();
                let mut other_peers: Vec<(String, Arc<Peer<S>>)> = Vec::new();

                for peer_id in &ordered_peer_ids {
                    if *peer_id != req.exclude_peer_id {
                        if let Some(entry) = peers_read.get(peer_id) {
                            if entry.peer.state().await == PeerState::Ready {
                                match entry.pool {
                                    PeerPool::Follows => follows_peers.push((peer_id.clone(), entry.peer.clone())),
                                    PeerPool::Other => other_peers.push((peer_id.clone(), entry.peer.clone())),
                                }
                            }
                        }
                    }
                }
                drop(peers_read);

                // Request size estimate for metrics
                let request_bytes = 40u64;

                // Query peers sequentially (follows first, then others) in selector order
                let mut result = None;
                for (peer_id, peer) in follows_peers.into_iter().chain(other_peers.into_iter()) {
                    // Record request being sent
                    peer_selector.write().await.record_request(&peer_id, request_bytes);
                    let start_time = std::time::Instant::now();

                    // Use request_with_htl to forward with the given HTL
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(500), // Short timeout per peer
                        peer.request_with_htl(&req.hash, req.htl),
                    )
                    .await
                    {
                        Ok(Ok(Some(data))) => {
                            // Verify hash
                            if hashtree_core::sha256(&data) == req.hash {
                                // Record success with RTT
                                let rtt_ms = start_time.elapsed().as_millis() as u64;
                                peer_selector.write().await.record_success(&peer_id, rtt_ms, data.len() as u64);

                                // Store locally for future requests
                                let _ = local_store.put(req.hash, data.clone()).await;
                                result = Some(data);
                                break;
                            } else {
                                // Hash mismatch
                                peer_selector.write().await.record_failure(&peer_id);
                            }
                        }
                        Ok(Ok(None)) => {
                            // Peer doesn't have data - not a failure
                            continue;
                        }
                        Ok(Err(_)) => {
                            // Error from peer
                            peer_selector.write().await.record_failure(&peer_id);
                            continue;
                        }
                        Err(_) => {
                            // Timeout
                            peer_selector.write().await.record_timeout(&peer_id);
                            continue;
                        }
                    }
                }

                if debug {
                    println!(
                        "[Store] Forward result: hash={}..., found={}",
                        &to_hex(&req.hash)[..16],
                        result.is_some()
                    );
                }

                let _ = req.response.send(result);
            }
        });
    }

    /// Send hello message to discover peers
    async fn send_hello(&self) -> Result<(), WebRTCStoreError> {
        let roots: Vec<String> = self.config.roots.iter().map(to_hex).collect();

        let msg = SignalingMessage::Hello {
            peer_id: self.peer_id.to_peer_string(),
            roots,
        };

        self.signaling_tx
            .send(msg)
            .await
            .map_err(|_| WebRTCStoreError::Nostr("Channel closed".to_string()))?;

        Ok(())
    }

    /// Start event handler for incoming Nostr events
    async fn start_event_handler(&self, client: Client) {
        let peers = self.peers.clone();
        let peer_roots = self.peer_roots.clone();
        let local_peer_id = self.peer_id.to_peer_string();
        let signaling_tx = self.signaling_tx.clone();
        let forward_tx = self.forward_tx.clone();
        let local_store = self.local_store.clone();
        let running = self.running.clone();
        let config = self.config.clone();
        let stats = self.stats.clone();
        let peer_selector = self.peer_selector.clone();

        // Get our own broadcast receiver for notifications
        // Each call to notifications() returns a new receiver that receives all notifications
        let mut notifications = client.notifications();

        tokio::spawn(async move {
            loop {
                if !*running.read().await {
                    break;
                }

                // Use tokio timeout to periodically check running flag
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    notifications.recv(),
                )
                .await
                {
                    Ok(Ok(notification)) => {
                        if let RelayPoolNotification::Event { event, .. } = notification {
                            // Only process our custom kind
                            if event.kind == Kind::Custom(NOSTR_KIND_HASHTREE) {
                                if config.debug {
                                    let content_preview = if event.content.len() > 80 {
                                        format!("{}...", &event.content[..80])
                                    } else {
                                        event.content.clone()
                                    };
                                    println!("[Store] Received event: {}", content_preview);
                                }
                                if let Ok(msg) =
                                    serde_json::from_str::<SignalingMessage>(&event.content)
                                {
                                    Self::handle_signaling_message(
                                        msg,
                                        &local_peer_id,
                                        peers.clone(),
                                        peer_roots.clone(),
                                        signaling_tx.clone(),
                                        forward_tx.clone(),
                                        local_store.clone(),
                                        &config,
                                        stats.clone(),
                                        peer_selector.clone(),
                                    )
                                    .await;
                                } else if config.debug {
                                    println!(
                                        "[Store] Failed to parse signaling message from event"
                                    );
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        // Channel closed or lagged
                        if config.debug {
                            println!("[Store] Notification channel error: {:?}", e);
                        }
                        // For lagged errors, we can continue
                        // For closed errors, break
                        if matches!(e, tokio::sync::broadcast::error::RecvError::Closed) {
                            break;
                        }
                    }
                    Err(_) => {
                        // Timeout, continue loop
                    }
                }
            }
        });
    }

    /// Classify a peer using the classifier channel
    async fn classify_peer(pubkey: &str, config: &WebRTCStoreConfig) -> PeerPool {
        if let Some(ref classifier_tx) = config.classifier_tx {
            let (response_tx, response_rx) = oneshot::channel();
            let request = ClassifyRequest {
                pubkey: pubkey.to_string(),
                response: response_tx,
            };
            if classifier_tx.send(request).await.is_ok() {
                if let Ok(pool) = response_rx.await {
                    return pool;
                }
            }
        }
        PeerPool::Other
    }

    /// Count peers by pool
    async fn count_pools(peers: &HashMap<String, PeerEntry<S>>) -> (usize, usize) {
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

    /// Check if we can accept a new peer in a given pool
    fn can_accept_peer(pool: PeerPool, follows_count: usize, other_count: usize, config: &WebRTCStoreConfig) -> bool {
        match pool {
            PeerPool::Follows => follows_count < config.pools.follows.max_connections,
            PeerPool::Other => other_count < config.pools.other.max_connections,
        }
    }

    /// Check if a pool needs more connections
    fn pool_needs_peers(pool: PeerPool, follows_count: usize, other_count: usize, config: &WebRTCStoreConfig) -> bool {
        match pool {
            PeerPool::Follows => follows_count < config.pools.follows.satisfied_connections,
            PeerPool::Other => other_count < config.pools.other.satisfied_connections,
        }
    }

    /// Handle incoming signaling message
    async fn handle_signaling_message(
        msg: SignalingMessage,
        local_peer_id: &str,
        peers: Arc<RwLock<HashMap<String, PeerEntry<S>>>>,
        peer_roots: Arc<RwLock<HashMap<String, Vec<String>>>>,
        signaling_tx: mpsc::Sender<SignalingMessage>,
        forward_tx: ForwardTx,
        local_store: Arc<S>,
        config: &WebRTCStoreConfig,
        stats: Arc<RwLock<WebRTCStats>>,
        peer_selector: Arc<RwLock<PeerSelector>>,
    ) {
        match &msg {
            SignalingMessage::Hello { peer_id, roots } => {
                if peer_id == local_peer_id {
                    return; // Ignore own messages
                }

                // Extract pubkey from peer_id (format: "pubkey:uuid")
                let peer_pubkey = peer_id.split(':').next().unwrap_or("");

                // Classify the peer
                let pool = Self::classify_peer(peer_pubkey, config).await;

                // Check pool limits
                let peers_read = peers.read().await;
                let (follows_count, other_count) = Self::count_pools(&peers_read).await;
                drop(peers_read);

                if !Self::can_accept_peer(pool, follows_count, other_count, config) {
                    if config.debug {
                        println!("[Store] Ignoring hello from {} - {:?} pool full", peer_id, pool);
                    }
                    return;
                }

                if config.debug {
                    println!("[Store] Received hello from {} (pool: {:?})", peer_id, pool);
                }

                // Store peer roots
                peer_roots.write().await.insert(peer_id.clone(), roots.clone());

                // Perfect negotiation: send offer if we NEED more peers
                // Both sides may send offers - collisions handled in offer handler
                if Self::pool_needs_peers(pool, follows_count, other_count, config) {
                    if let Some(remote_id) = PeerId::from_peer_string(peer_id) {
                        if !peers.read().await.contains_key(peer_id) {
                            if config.debug {
                                println!("[Store] Initiating connection to {} (pool: {:?})", peer_id, pool);
                            }
                            // Create peer and add to map BEFORE connecting to avoid race with incoming answer
                            if let Ok(peer) = Peer::with_forward_channel(
                                remote_id,
                                local_peer_id.to_string(),
                                signaling_tx.clone(),
                                local_store.clone(),
                                config.debug,
                                Some(forward_tx.clone()),
                            )
                            .await
                            {
                                let peer = Arc::new(peer);
                                peers.write().await.insert(peer_id.clone(), PeerEntry { peer: peer.clone(), pool });
                                stats.write().await.connected_peers += 1;

                                // Add to peer selector for adaptive selection
                                peer_selector.write().await.add_peer(peer_id.clone());

                                // Spawn connection in separate task to not block event processing
                                tokio::spawn(async move {
                                    let _ = peer.connect().await;
                                });
                            }
                        }
                    }
                }
            }
            SignalingMessage::Offer {
                peer_id,
                target_peer_id,
                ..
            }
            | SignalingMessage::Answer {
                peer_id,
                target_peer_id,
                ..
            }
            | SignalingMessage::Candidate {
                peer_id,
                target_peer_id,
                ..
            }
            | SignalingMessage::Candidates {
                peer_id,
                target_peer_id,
                ..
            } => {
                if target_peer_id != local_peer_id {
                    return; // Not for us
                }

                // Extract pubkey from peer_id
                let peer_pubkey = peer_id.split(':').next().unwrap_or("");

                // Classify the peer
                let pool = Self::classify_peer(peer_pubkey, config).await;

                // Check pool limits
                let peers_read = peers.read().await;
                let (follows_count, other_count) = Self::count_pools(&peers_read).await;
                drop(peers_read);

                if !Self::can_accept_peer(pool, follows_count, other_count, config) {
                    if config.debug {
                        println!("[Store] Ignoring signaling from {} - {:?} pool full", peer_id, pool);
                    }
                    return;
                }

                // Get or create peer
                let peer = {
                    let peers_read = peers.read().await;
                    peers_read.get(peer_id).map(|e| e.peer.clone())
                };

                let peer = match peer {
                    Some(p) => p,
                    None => {
                        if let Some(remote_id) = PeerId::from_peer_string(peer_id) {
                            if let Ok(p) = Peer::with_forward_channel(
                                remote_id,
                                local_peer_id.to_string(),
                                signaling_tx.clone(),
                                local_store.clone(),
                                config.debug,
                                Some(forward_tx.clone()),
                            )
                            .await
                            {
                                let p = Arc::new(p);
                                peers.write().await.insert(peer_id.clone(), PeerEntry { peer: p.clone(), pool });
                                stats.write().await.connected_peers += 1;

                                // Add to peer selector for adaptive selection
                                peer_selector.write().await.add_peer(peer_id.clone());
                                p
                            } else {
                                return;
                            }
                        } else {
                            return;
                        }
                    }
                };

                let _ = peer.handle_signaling(msg).await;
            }
        }
    }

    /// Start signaling message sender
    async fn start_signaling_sender(&self, client: Client) {
        let mut rx = self.signaling_rx.write().await.take().unwrap();
        let running = self.running.clone();

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if !*running.read().await {
                    break;
                }

                let json = serde_json::to_string(&msg).unwrap();
                println!("[Store] Sending signaling: {}", &json[..json.len().min(100)]);
                let builder =
                    EventBuilder::new(Kind::Custom(NOSTR_KIND_HASHTREE), json, []);

                match client.send_event_builder(builder).await {
                    Ok(output) => {
                        // Check if event was actually sent
                        if output.success.is_empty() {
                            eprintln!("[Store] Warning: Event not sent to any relay");
                        }
                    }
                    Err(e) => {
                        eprintln!("[Store] Error sending event: {:?}", e);
                    }
                }
            }
        });
    }

    /// Start periodic hello sender
    async fn start_hello_timer(&self) {
        let signaling_tx = self.signaling_tx.clone();
        let peer_id = self.peer_id.to_peer_string();
        let roots: Vec<String> = self.config.roots.iter().map(to_hex).collect();
        let interval_ms = self.config.hello_interval_ms;
        let running = self.running.clone();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(interval_ms));

            loop {
                interval.tick().await;

                if !*running.read().await {
                    break;
                }

                let msg = SignalingMessage::Hello {
                    peer_id: peer_id.clone(),
                    roots: roots.clone(),
                };

                let _ = signaling_tx.send(msg).await;
            }
        });
    }

    /// Stop the WebRTC store
    pub async fn stop(&self) {
        *self.running.write().await = false;

        // Close all peer connections
        let peers = self.peers.read().await;
        for entry in peers.values() {
            let _ = entry.peer.close().await;
        }

        // Disconnect from relays
        if let Some(ref client) = self.client {
            let _ = client.disconnect().await;
        }
    }

    /// Get statistics
    pub async fn stats(&self) -> WebRTCStats {
        self.stats.read().await.clone()
    }

    /// Get connected peer count
    pub async fn peer_count(&self) -> usize {
        let peers = self.peers.read().await;
        let mut count = 0;
        for entry in peers.values() {
            if entry.peer.state().await == PeerState::Ready {
                count += 1;
            }
        }
        count
    }

    /// Request data from peers using adaptive peer selection
    ///
    /// Uses PeerSelector to order peers by performance (success rate, RTT).
    /// Follows pool is still prioritized, but ordering within each pool uses selector.
    async fn request_from_peers(&self, hash: &Hash) -> Result<Option<Vec<u8>>, WebRTCStoreError> {
        // Get ordered peer list from selector
        let ordered_peer_ids = self.peer_selector.write().await.select_peers();

        let peers = self.peers.read().await;

        // Build ordered list of ready peers, prioritizing follows pool but using selector order within each
        let mut follows_peers: Vec<(String, Arc<Peer<S>>)> = Vec::new();
        let mut other_peers: Vec<(String, Arc<Peer<S>>)> = Vec::new();

        for peer_id in &ordered_peer_ids {
            if let Some(entry) = peers.get(peer_id) {
                if entry.peer.state().await == PeerState::Ready {
                    match entry.pool {
                        PeerPool::Follows => follows_peers.push((peer_id.clone(), entry.peer.clone())),
                        PeerPool::Other => other_peers.push((peer_id.clone(), entry.peer.clone())),
                    }
                }
            }
        }
        drop(peers);

        // Request size estimate for metrics (hash request is ~40 bytes)
        let request_bytes = 40u64;

        // Try follows first, then others (in selector order within each pool)
        for (peer_id, peer) in follows_peers.into_iter().chain(other_peers.into_iter()) {
            // Record request being sent
            self.peer_selector.write().await.record_request(&peer_id, request_bytes);
            let start_time = std::time::Instant::now();

            match peer.request(hash).await {
                Ok(Some(data)) => {
                    // Verify hash
                    if hashtree_core::sha256(&data) == *hash {
                        // Record success with RTT
                        let rtt_ms = start_time.elapsed().as_millis() as u64;
                        self.peer_selector.write().await.record_success(&peer_id, rtt_ms, data.len() as u64);

                        // Store locally for future requests
                        let _ = self.local_store.put(*hash, data.clone()).await;
                        let mut stats = self.stats.write().await;
                        stats.requests_fulfilled += 1;
                        stats.bytes_received += data.len() as u64;
                        return Ok(Some(data));
                    } else {
                        // Hash mismatch - record as failure
                        self.peer_selector.write().await.record_failure(&peer_id);
                    }
                }
                Ok(None) => {
                    // Peer doesn't have data - not a failure, just continue
                    continue;
                }
                Err(PeerError::Timeout) => {
                    // Record timeout
                    self.peer_selector.write().await.record_timeout(&peer_id);
                    continue;
                }
                Err(_) => {
                    // Other errors (disconnect, etc.) - record as failure
                    self.peer_selector.write().await.record_failure(&peer_id);
                    continue;
                }
            }
        }

        Ok(None)
    }

    /// Get peer selector summary statistics
    pub async fn selector_summary(&self) -> crate::peer_selector::SelectorSummary {
        self.peer_selector.read().await.summary()
    }
}

#[async_trait]
impl<S: Store + 'static> Store for WebRTCStore<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local_store.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        // Try local first
        if let Some(data) = self.local_store.get(hash).await? {
            return Ok(Some(data));
        }

        // Update stats
        self.stats.write().await.requests_made += 1;

        // Try peers
        match self.request_from_peers(hash).await {
            Ok(data) => Ok(data),
            Err(_) => Ok(None),
        }
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local_store.delete(hash).await
    }
}
