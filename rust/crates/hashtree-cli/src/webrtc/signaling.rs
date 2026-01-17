//! WebRTC signaling over Nostr relays
//!
//! Protocol (compatible with hashtree-ts):
//! - All signaling uses ephemeral kind 25050
//! - Hello messages: #l: "hello" tag, broadcast for peer discovery (unencrypted)
//! - Directed signaling (offer, answer, candidate, candidates): NIP-17 style
//!   gift wrap for privacy - wrapped with ephemeral key, #p tag with recipient
//!
//! Security: Directed messages use gift wrapping with ephemeral keys so that
//! relays cannot see the actual sender or correlate messages.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use nostr::{nips::nip44, ClientMessage, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, RelayMessage, Tag};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use super::peer::{ContentStore, Peer, PendingRequest};
use super::types::{
    PeerDirection, PeerId, PeerPool, PeerStateEvent, PeerStatus, SignalingMessage, WebRTCConfig, WEBRTC_KIND, HELLO_TAG,
};

/// Callback type for classifying peers into pools
pub type PeerClassifier = Arc<dyn Fn(&str) -> PeerPool + Send + Sync>;

/// Connection state for a peer
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Discovered,
    Connecting,
    Connected,
    Failed,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Discovered => write!(f, "discovered"),
            ConnectionState::Connecting => write!(f, "connecting"),
            ConnectionState::Connected => write!(f, "connected"),
            ConnectionState::Failed => write!(f, "failed"),
        }
    }
}

/// Peer entry in the manager
pub struct PeerEntry {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub state: ConnectionState,
    pub last_seen: Instant,
    pub peer: Option<Peer>,
    pub pool: PeerPool,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Shared state for WebRTC manager
pub struct WebRTCState {
    pub peers: RwLock<HashMap<String, PeerEntry>>,
    pub connected_count: std::sync::atomic::AtomicUsize,
    /// Total bytes sent across all peers (cumulative)
    pub bytes_sent: std::sync::atomic::AtomicU64,
    /// Total bytes received across all peers (cumulative)
    pub bytes_received: std::sync::atomic::AtomicU64,
}

impl WebRTCState {
    pub fn new() -> Self {
        Self {
            peers: RwLock::new(HashMap::new()),
            connected_count: std::sync::atomic::AtomicUsize::new(0),
            bytes_sent: std::sync::atomic::AtomicU64::new(0),
            bytes_received: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Get current bandwidth stats (bytes sent/received)
    pub fn get_bandwidth(&self) -> (u64, u64) {
        (
            self.bytes_sent.load(std::sync::atomic::Ordering::Relaxed),
            self.bytes_received.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Record bytes sent (global + per-peer)
    pub async fn record_sent(&self, peer_id: &str, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_sent += bytes;
        }
    }

    /// Record bytes received (global + per-peer)
    pub async fn record_received(&self, peer_id: &str, bytes: u64) {
        self.bytes_received.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        if let Some(entry) = self.peers.write().await.get_mut(peer_id) {
            entry.bytes_received += bytes;
        }
    }

    /// Request content by hash from connected peers
    /// Queries peers sequentially with 500ms intervals until one responds
    /// Returns the first successful response, or None if no peer has it
    pub async fn request_from_peers(&self, hash_hex: &str) -> Option<Vec<u8>> {
        use super::types::{DataRequest, MAX_HTL, encode_request};

        let peers = self.peers.read().await;

        // Collect connected peers with data channels
        // We need to collect the Arc references first, then acquire locks outside the iterator
        let peer_refs: Vec<_> = peers
            .values()
            .filter(|p| p.state == ConnectionState::Connected && p.peer.is_some())
            .filter_map(|p| {
                p.peer.as_ref().map(|peer| {
                    (p.peer_id.short(), peer.data_channel.clone(), peer.pending_requests.clone())
                })
            })
            .collect();

        drop(peers); // Release the read lock

        // Now acquire locks and filter to peers with active data channels
        let mut connected_peers: Vec<(String, Arc<Mutex<HashMap<String, PendingRequest>>>, Arc<webrtc::data_channel::RTCDataChannel>)> = Vec::new();
        for (peer_id, dc_mutex, pending) in peer_refs {
            let dc_guard = dc_mutex.lock().await;
            if let Some(dc) = dc_guard.as_ref() {
                connected_peers.push((peer_id, pending, dc.clone()));
            }
        }

        if connected_peers.is_empty() {
            debug!("No connected peers to query for {}", &hash_hex[..8.min(hash_hex.len())]);
            return None;
        }

        debug!(
            "Querying {} connected peers for {} (sequential with 500ms delay)",
            connected_peers.len(),
            &hash_hex[..8.min(hash_hex.len())]
        );

        // Convert hex to binary hash once
        let hash_bytes = match hex::decode(hash_hex) {
            Ok(b) => b,
            Err(_) => return None,
        };

        // Query peers sequentially with 500ms delay between each
        for (_i, (peer_id, pending_requests, dc)) in connected_peers.into_iter().enumerate() {
            debug!("Querying peer {} for {}", peer_id, &hash_hex[..8.min(hash_hex.len())]);

            // Create response channel
            let (tx, rx) = tokio::sync::oneshot::channel();

            // Store pending request
            {
                let mut pending = pending_requests.lock().await;
                pending.insert(
                    hash_hex.to_string(),
                    super::PendingRequest {
                        hash: hash_bytes.clone(),
                        response_tx: tx,
                    },
                );
            }

            // Send request
            let req = DataRequest {
                h: hash_bytes.clone(),
                htl: MAX_HTL,
            };
            if let Ok(wire) = encode_request(&req) {
                let wire_len = wire.len() as u64;
                if dc.send(&bytes::Bytes::from(wire)).await.is_ok() {
                    self.record_sent(&peer_id, wire_len).await;
                    // Wait 500ms for response from this peer
                    match tokio::time::timeout(std::time::Duration::from_millis(500), rx).await {
                        Ok(Ok(Some(data))) => {
                            self.record_received(&peer_id, data.len() as u64).await;
                            debug!("Got response from peer {} for {}", peer_id, &hash_hex[..8.min(hash_hex.len())]);
                            return Some(data);
                        }
                        _ => {
                            // Timeout or no data - clean up and try next peer
                            debug!("No response from peer {} for {}", peer_id, &hash_hex[..8.min(hash_hex.len())]);
                        }
                    }
                }
            }

            // Clean up pending request
            let mut pending = pending_requests.lock().await;
            pending.remove(hash_hex);
        }

        debug!("No peer had data for {}", &hash_hex[..8.min(hash_hex.len())]);
        None
    }
}

/// WebRTC manager handles peer discovery and connection management
pub struct WebRTCManager {
    config: WebRTCConfig,
    my_peer_id: PeerId,
    keys: Keys,
    state: Arc<WebRTCState>,
    shutdown: Arc<tokio::sync::watch::Sender<bool>>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// Channel to send signaling messages to relays
    signaling_tx: mpsc::Sender<SignalingMessage>,
    signaling_rx: Option<mpsc::Receiver<SignalingMessage>>,
    /// Optional content store for serving hash requests
    store: Option<Arc<dyn ContentStore>>,
    /// Peer classifier for pool assignment
    peer_classifier: PeerClassifier,
    /// Channel for peer state events (connection success/failure)
    state_event_tx: mpsc::Sender<PeerStateEvent>,
    state_event_rx: Option<mpsc::Receiver<PeerStateEvent>>,
}

impl WebRTCManager {
    /// Create a new WebRTC manager
    pub fn new(keys: Keys, config: WebRTCConfig) -> Self {
        let pubkey = keys.public_key().to_hex();
        let my_peer_id = PeerId::new(pubkey, None);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let (signaling_tx, signaling_rx) = mpsc::channel(100);
        let (state_event_tx, state_event_rx) = mpsc::channel(100);

        // Default classifier: all peers go to 'other' pool
        let peer_classifier: PeerClassifier = Arc::new(|_| PeerPool::Other);

        Self {
            config,
            my_peer_id,
            keys,
            state: Arc::new(WebRTCState::new()),
            shutdown: Arc::new(shutdown),
            shutdown_rx,
            signaling_tx,
            signaling_rx: Some(signaling_rx),
            store: None,
            peer_classifier,
            state_event_tx,
            state_event_rx: Some(state_event_rx),
        }
    }

    /// Create a new WebRTC manager with a peer classifier
    pub fn new_with_classifier(keys: Keys, config: WebRTCConfig, classifier: PeerClassifier) -> Self {
        let mut manager = Self::new(keys, config);
        manager.peer_classifier = classifier;
        manager
    }

    /// Create a new WebRTC manager with a content store for serving hash requests
    pub fn new_with_store(keys: Keys, config: WebRTCConfig, store: Arc<dyn ContentStore>) -> Self {
        let mut manager = Self::new(keys, config);
        manager.store = Some(store);
        manager
    }

    /// Create a new WebRTC manager with store and classifier
    pub fn new_with_store_and_classifier(
        keys: Keys,
        config: WebRTCConfig,
        store: Arc<dyn ContentStore>,
        classifier: PeerClassifier,
    ) -> Self {
        let mut manager = Self::new(keys, config);
        manager.store = Some(store);
        manager.peer_classifier = classifier;
        manager
    }

    /// Set the content store for serving hash requests
    pub fn set_store(&mut self, store: Arc<dyn ContentStore>) {
        self.store = Some(store);
    }

    /// Set the peer classifier
    pub fn set_peer_classifier(&mut self, classifier: PeerClassifier) {
        self.peer_classifier = classifier;
    }

    /// Get my peer ID
    pub fn my_peer_id(&self) -> &PeerId {
        &self.my_peer_id
    }

    /// Get shared state for external access
    pub fn state(&self) -> Arc<WebRTCState> {
        self.state.clone()
    }

    /// Signal shutdown
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Get connected peer count
    pub async fn connected_count(&self) -> usize {
        self.state
            .connected_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get all peer statuses
    pub async fn peer_statuses(&self) -> Vec<PeerStatus> {
        self.state
            .peers
            .read()
            .await
            .values()
            .map(|p| PeerStatus {
                peer_id: p.peer_id.to_string(),
                pubkey: p.peer_id.pubkey.clone(),
                state: p.state.to_string(),
                direction: p.direction,
                connected_at: Some(p.last_seen),
                pool: p.pool,
            })
            .collect()
    }

    /// Get pool counts
    /// Returns (follows_connected, follows_active, other_connected, other_active)
    /// "active" = Connected or Connecting (excludes Discovered and Failed)
    pub async fn get_pool_counts(&self) -> (usize, usize, usize, usize) {
        let peers = self.state.peers.read().await;
        let mut follows_connected = 0;
        let mut follows_active = 0;
        let mut other_connected = 0;
        let mut other_active = 0;

        for entry in peers.values() {
            // Only count Connected or Connecting as "active" connections
            // Discovered peers are just seen hellos, not real connections
            let is_active = entry.state == ConnectionState::Connected
                || entry.state == ConnectionState::Connecting;

            match entry.pool {
                PeerPool::Follows => {
                    if is_active {
                        follows_active += 1;
                    }
                    if entry.state == ConnectionState::Connected {
                        follows_connected += 1;
                    }
                }
                PeerPool::Other => {
                    if is_active {
                        other_active += 1;
                    }
                    if entry.state == ConnectionState::Connected {
                        other_connected += 1;
                    }
                }
            }
        }

        (follows_connected, follows_active, other_connected, other_active)
    }

    /// Check if we can accept a peer in a given pool
    fn can_accept_peer(&self, pool: PeerPool, pool_counts: &(usize, usize, usize, usize)) -> bool {
        let (_, follows_active, _, other_active) = *pool_counts;
        match pool {
            PeerPool::Follows => follows_active < self.config.pools.follows.max_connections,
            PeerPool::Other => other_active < self.config.pools.other.max_connections,
        }
    }

    /// Check if a pool is satisfied
    #[allow(dead_code)]
    fn is_pool_satisfied(&self, pool: PeerPool, pool_counts: &(usize, usize, usize, usize)) -> bool {
        let (follows_connected, _, other_connected, _) = *pool_counts;
        match pool {
            PeerPool::Follows => follows_connected >= self.config.pools.follows.satisfied_connections,
            PeerPool::Other => other_connected >= self.config.pools.other.satisfied_connections,
        }
    }

    /// Check if both pools are satisfied
    #[allow(dead_code)]
    fn is_satisfied(&self, pool_counts: &(usize, usize, usize, usize)) -> bool {
        self.is_pool_satisfied(PeerPool::Follows, pool_counts)
            && self.is_pool_satisfied(PeerPool::Other, pool_counts)
    }

    /// Check if we should initiate connection (tie-breaking)
    /// Lower UUID initiates - same as iris-client/hashtree-ts
    fn should_initiate(&self, their_uuid: &str) -> bool {
        self.my_peer_id.uuid < their_uuid.to_string()
    }

    /// Start the WebRTC manager - connects to relays and handles signaling
    pub async fn run(&mut self) -> Result<()> {
        info!(
            "Starting WebRTC manager with peer ID: {}",
            self.my_peer_id.short()
        );

        let (event_tx, mut event_rx) = mpsc::channel::<(String, nostr::Event)>(100);

        // Take the signaling receiver
        let mut signaling_rx = self.signaling_rx.take().expect("signaling_rx already taken");

        // Take the state event receiver
        let mut state_event_rx = self.state_event_rx.take().expect("state_event_rx already taken");

        // Create a shared write channel for all relay tasks
        let (relay_write_tx, _) = tokio::sync::broadcast::channel::<SignalingMessage>(100);

        // Spawn relay connections
        for relay_url in &self.config.relays {
            let url = relay_url.clone();
            let event_tx = event_tx.clone();
            let shutdown_rx = self.shutdown_rx.clone();
            let keys = self.keys.clone();
            let my_peer_id = self.my_peer_id.clone();
            let hello_interval = Duration::from_millis(self.config.hello_interval_ms);
            let relay_write_rx = relay_write_tx.subscribe();

            tokio::spawn(async move {
                if let Err(e) = Self::relay_task(
                    url.clone(),
                    event_tx,
                    shutdown_rx,
                    keys,
                    my_peer_id,
                    hello_interval,
                    relay_write_rx,
                )
                .await
                {
                    error!("Relay {} error: {}", url, e);
                }
            });
        }

        // Process incoming events and outgoing signaling messages
        let mut shutdown_rx = self.shutdown_rx.clone();
        // Cleanup interval - run every 30 seconds as a fallback (not for real-time sync)
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("WebRTC manager shutting down");
                        break;
                    }
                }
                Some((relay, event)) = event_rx.recv() => {
                    if let Err(e) = self.handle_event(&relay, &event, &relay_write_tx).await {
                        debug!("Error handling event from {}: {}", relay, e);
                    }
                }
                Some(msg) = signaling_rx.recv() => {
                    // Forward signaling messages to relay broadcast
                    let _ = relay_write_tx.send(msg);
                }
                Some(event) = state_event_rx.recv() => {
                    // Handle peer state events (connected, failed, disconnected)
                    self.handle_peer_state_event(event).await;
                }
                _ = cleanup_interval.tick() => {
                    // Periodic cleanup of stale peers and state sync (fallback)
                    self.cleanup_stale_peers().await;
                }
            }
        }

        Ok(())
    }

    /// Connect to a single relay and handle messages
    async fn relay_task(
        url: String,
        event_tx: mpsc::Sender<(String, nostr::Event)>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        keys: Keys,
        my_peer_id: PeerId,
        hello_interval: Duration,
        mut signaling_rx: tokio::sync::broadcast::Receiver<SignalingMessage>,
    ) -> Result<()> {
        info!("Connecting to relay: {}", url);

        let (ws_stream, _) = connect_async(&url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Subscribe to webrtc events - two filters:
        // 1. Hello messages: kind 25050 with #l: "hello" tag
        // 2. Directed messages: kind 25050 with #p tag (our pubkey)
        let hello_filter = Filter::new()
            .kind(Kind::Ephemeral(WEBRTC_KIND as u16))
            .custom_tag(
                nostr::SingleLetterTag::lowercase(nostr::Alphabet::L),
                vec![HELLO_TAG],
            )
            .since(nostr::Timestamp::now() - Duration::from_secs(60));

        let directed_filter = Filter::new()
            .kind(Kind::Ephemeral(WEBRTC_KIND as u16))
            .custom_tag(
                nostr::SingleLetterTag::lowercase(nostr::Alphabet::P),
                vec![keys.public_key().to_hex()],
            )
            .since(nostr::Timestamp::now() - Duration::from_secs(60));

        let sub_id = nostr::SubscriptionId::generate();
        let sub_msg = ClientMessage::req(sub_id.clone(), vec![hello_filter, directed_filter]);
        write.send(Message::Text(sub_msg.as_json().into())).await?;

        info!("Subscribed to {} for WebRTC events (kind {})", url, WEBRTC_KIND);

        let mut last_hello = Instant::now() - hello_interval; // Send immediately
        let mut hello_ticker = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = hello_ticker.tick() => {
                    // Send hello periodically
                    if last_hello.elapsed() >= hello_interval {
                        let hello = SignalingMessage::hello(&my_peer_id.uuid);
                        if let Ok(event) = Self::create_signaling_event(&keys, &hello).await {
                            let msg = ClientMessage::event(event);
                            if write.send(Message::Text(msg.as_json().into())).await.is_ok() {
                                debug!("Sent hello to {}", url);
                            }
                        }
                        last_hello = Instant::now();
                    }
                }
                // Handle outgoing signaling messages
                Ok(signaling_msg) = signaling_rx.recv() => {
                    info!("Sending {} via {}", signaling_msg.msg_type(), url);
                    if let Ok(event) = Self::create_signaling_event(&keys, &signaling_msg).await {
                        let event_id = event.id.to_string();
                        let msg = ClientMessage::event(event);
                        if write.send(Message::Text(msg.as_json().into())).await.is_ok() {
                            info!("Sent {} to {} (event id: {})", signaling_msg.msg_type(), url, &event_id[..16]);
                        }
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(relay_msg) = RelayMessage::from_json(&text) {
                                if let RelayMessage::Event { event, .. } = relay_msg {
                                    let _ = event_tx.send((url.clone(), *event)).await;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error from {}: {}", url, e);
                            break;
                        }
                        None => {
                            warn!("WebSocket closed: {}", url);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    /// Create a signaling event
    ///
    /// For directed messages (offer, answer, candidate, candidates), use NIP-17 style
    /// gift wrapping with ephemeral keys for privacy.
    /// Hello messages use kind 25050 with #l: "hello" tag and peerId.
    async fn create_signaling_event(keys: &Keys, msg: &SignalingMessage) -> Result<nostr::Event> {
        // Check if message has a recipient (needs gift wrapping)
        if let Some(recipient_str) = msg.recipient() {
            // Parse recipient to get their pubkey
            if let Some(peer_id) = PeerId::from_string(recipient_str) {
                let recipient_pubkey = PublicKey::from_hex(&peer_id.pubkey)?;

                // Create seal with sender's actual pubkey (the "rumor")
                let seal = serde_json::json!({
                    "pubkey": keys.public_key().to_hex(),
                    "kind": WEBRTC_KIND,
                    "content": serde_json::to_string(msg)?,
                    "tags": []
                });

                // Generate ephemeral keypair for the wrapper
                let ephemeral_keys = Keys::generate();

                // Encrypt the seal for the recipient using ephemeral key (NIP-44)
                let encrypted_content = nip44::encrypt(
                    ephemeral_keys.secret_key(),
                    &recipient_pubkey,
                    &seal.to_string(),
                    nip44::Version::V2
                )?;

                // Create wrapper event with ephemeral key
                let created_at = nostr::Timestamp::now();
                let expiration = created_at + Duration::from_secs(5 * 60); // 5 minutes

                let tags = vec![
                    Tag::parse(&["p", &recipient_pubkey.to_hex()])?,
                    Tag::parse(&["expiration", &expiration.as_u64().to_string()])?,
                ];

                let event = EventBuilder::new(Kind::Ephemeral(WEBRTC_KIND as u16), encrypted_content, tags)
                    .to_event(&ephemeral_keys)?;

                return Ok(event);
            }
        }

        // Hello messages - kind 25050 with #l: "hello" tag and peerId
        let tags = vec![
            Tag::parse(&["l", HELLO_TAG])?,
            Tag::parse(&["peerId", msg.peer_id()])?,
        ];

        let event = EventBuilder::new(Kind::Ephemeral(WEBRTC_KIND as u16), "", tags)
            .to_event(keys)?;

        Ok(event)
    }

    /// Handle an incoming event
    ///
    /// Messages may be:
    /// 1. Hello messages: kind 25050 with #l: "hello" tag and peerId
    /// 2. Gift-wrapped directed messages: kind 25050 with #p tag, encrypted with ephemeral key
    async fn handle_event(
        &self,
        relay: &str,
        event: &nostr::Event,
        relay_write_tx: &tokio::sync::broadcast::Sender<SignalingMessage>,
    ) -> Result<()> {
        // Must be kind 25050
        if event.kind != Kind::Ephemeral(WEBRTC_KIND as u16) {
            return Ok(());
        }

        // Helper to get tag value
        let get_tag = |name: &str| -> Option<String> {
            event.tags.iter().find_map(|tag| {
                let v: Vec<String> = tag.clone().to_vec();
                if v.len() >= 2 && v[0] == name {
                    Some(v[1].clone())
                } else {
                    None
                }
            })
        };

        // Check if this is a hello message (#l: "hello" tag)
        let l_tag = get_tag("l");
        if l_tag.as_deref() == Some(HELLO_TAG) {
            let sender_pubkey = event.pubkey.to_hex();

            // Skip our own hello messages
            if sender_pubkey == self.my_peer_id.pubkey {
                return Ok(());
            }

            if let Some(their_uuid) = get_tag("peerId") {
                debug!("Received hello from {} via {}", &sender_pubkey[..8], relay);
                self.handle_hello(&sender_pubkey, &their_uuid, relay_write_tx)
                    .await?;
            }
            return Ok(());
        }

        // Check if this is a directed message for us (#p tag with our pubkey)
        let p_tag = get_tag("p");
        if p_tag.as_deref() != Some(&self.keys.public_key().to_hex()) {
            // Not for us - ignore silently
            return Ok(());
        }

        // Gift-wrapped directed message - decrypt using our key and ephemeral sender's pubkey
        if event.content.is_empty() {
            return Ok(());
        }

        // Try to unwrap the gift - decrypt with our key and the ephemeral sender's pubkey
        let seal: serde_json::Value = match nip44::decrypt(self.keys.secret_key(), &event.pubkey, &event.content) {
            Ok(plaintext) => {
                match serde_json::from_str(&plaintext) {
                    Ok(v) => v,
                    Err(_) => return Ok(()),
                }
            }
            Err(_) => {
                // Can't decrypt - not for us or invalid
                return Ok(());
            }
        };

        // Extract the actual sender's pubkey and content from the seal
        let sender_pubkey = seal.get("pubkey")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing pubkey in seal"))?;

        // Skip our own messages
        if sender_pubkey == self.my_peer_id.pubkey {
            return Ok(());
        }

        let content = seal.get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing content in seal"))?;

        let raw_msg: serde_json::Value = serde_json::from_str(content)?;
        let msg_type = raw_msg.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Support hashtree-ts format: { type, peerId, targetPeerId, sdp/candidate/candidates }
        if raw_msg.get("targetPeerId").is_some() {
            let target_peer = raw_msg
                .get("targetPeerId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if target_peer != self.my_peer_id.to_string() {
                return Ok(());
            }

            let peer_id = raw_msg.get("peerId").and_then(|v| v.as_str()).unwrap_or("");
            let their_uuid = peer_id.split(':').nth(1).unwrap_or(peer_id);

            match msg_type {
                "offer" => {
                    let sdp = raw_msg.get("sdp").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing SDP in offer"))?;
                    let offer = serde_json::json!({ "type": "offer", "sdp": sdp });
                    self.handle_offer(&sender_pubkey, their_uuid, offer, relay_write_tx).await?;
                }
                "answer" => {
                    let sdp = raw_msg.get("sdp").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing SDP in answer"))?;
                    let answer = serde_json::json!({ "type": "answer", "sdp": sdp });
                    self.handle_answer(&sender_pubkey, their_uuid, answer).await?;
                }
                "candidate" => {
                    let candidate = raw_msg.get("candidate").and_then(|v| v.as_str()).unwrap_or("");
                    if !candidate.is_empty() {
                        let candidate_json = serde_json::json!({
                            "candidate": candidate,
                            "sdpMid": raw_msg.get("sdpMid"),
                            "sdpMLineIndex": raw_msg.get("sdpMLineIndex"),
                        });
                        self.handle_candidate(&sender_pubkey, their_uuid, candidate_json).await?;
                    }
                }
                "candidates" => {
                    let candidates = raw_msg
                        .get("candidates")
                        .and_then(|v| v.as_array())
                        .map(|entries| {
                            entries.iter().filter_map(|entry| {
                                if let Some(candidate_str) = entry.get("candidate").and_then(|v| v.as_str()).or_else(|| entry.as_str()) {
                                    Some(serde_json::json!({
                                        "candidate": candidate_str,
                                        "sdpMid": entry.get("sdpMid"),
                                        "sdpMLineIndex": entry.get("sdpMLineIndex"),
                                    }))
                                } else {
                                    None
                                }
                            }).collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    self.handle_candidates(&sender_pubkey, their_uuid, candidates).await?;
                }
                _ => {}
            }

            return Ok(());
        }

        let msg: SignalingMessage = serde_json::from_value(raw_msg)?;

        debug!(
            "Received {} from {} via {} (gift-wrapped)",
            msg.msg_type(),
            &sender_pubkey[..8],
            relay
        );

        match msg {
            SignalingMessage::Hello { .. } => {
                // Hello messages should come via tags, not gift wrap
                return Ok(());
            }
            SignalingMessage::Offer {
                recipient,
                peer_id: their_uuid,
                offer,
            } => {
                if recipient != self.my_peer_id.to_string() {
                    return Ok(()); // Not for us
                }
                if let Err(e) = self.handle_offer(&sender_pubkey, &their_uuid, offer, relay_write_tx)
                    .await {
                    error!("handle_offer FAILED: sender={}, uuid={}, error={:?}", &sender_pubkey[..8.min(sender_pubkey.len())], their_uuid, e);
                    return Err(e);
                }
            }
            SignalingMessage::Answer {
                recipient,
                peer_id: their_uuid,
                answer,
            } => {
                if recipient != self.my_peer_id.to_string() {
                    return Ok(());
                }
                self.handle_answer(&sender_pubkey, &their_uuid, answer)
                    .await?;
            }
            SignalingMessage::Candidate {
                recipient,
                peer_id: their_uuid,
                candidate,
            } => {
                if recipient != self.my_peer_id.to_string() {
                    return Ok(());
                }
                self.handle_candidate(&sender_pubkey, &their_uuid, candidate)
                    .await?;
            }
            SignalingMessage::Candidates {
                recipient,
                peer_id: their_uuid,
                candidates,
            } => {
                if recipient != self.my_peer_id.to_string() {
                    return Ok(());
                }
                self.handle_candidates(&sender_pubkey, &their_uuid, candidates)
                    .await?;
            }
        }

        Ok(())
    }

    /// Handle incoming hello message
    async fn handle_hello(
        &self,
        sender_pubkey: &str,
        their_uuid: &str,
        relay_write_tx: &tokio::sync::broadcast::Sender<SignalingMessage>,
    ) -> Result<()> {
        let full_peer_id = PeerId::new(sender_pubkey.to_string(), Some(their_uuid.to_string()));
        let peer_key = full_peer_id.to_string();

        // Check if we already have this peer
        {
            let peers = self.state.peers.read().await;
            if let Some(entry) = peers.get(&peer_key) {
                // Already connected or connecting, just update last_seen
                if entry.state == ConnectionState::Connected
                    || entry.state == ConnectionState::Connecting
                {
                    return Ok(());
                }
            }
        }

        // Classify the peer into a pool
        let pool = (self.peer_classifier)(sender_pubkey);

        // Check pool limits
        let pool_counts = self.get_pool_counts().await;
        if !self.can_accept_peer(pool, &pool_counts) {
            debug!("Ignoring hello from {} - pool {:?} is full", full_peer_id.short(), pool);
            return Ok(());
        }

        // Decide if we should initiate based on tie-breaking
        let should_initiate = self.should_initiate(their_uuid);

        // If pool is already satisfied, don't initiate new outbound connections
        // This reserves space for inbound connections
        let pool_satisfied = self.is_pool_satisfied(pool, &pool_counts);
        let will_initiate = should_initiate && !pool_satisfied;

        info!(
            "Discovered peer: {} (pool: {:?}, initiate: {}, pool_satisfied: {})",
            full_peer_id.short(),
            pool,
            will_initiate,
            pool_satisfied
        );

        // If we're not initiating and pool is satisfied, don't even add to discovered
        // (we won't accept their offer either since pool check happens in handle_offer)
        if !will_initiate && pool_satisfied {
            debug!("Pool {:?} is satisfied, not tracking peer {}", pool, full_peer_id.short());
            return Ok(());
        }

        // Create peer entry with pool assignment
        {
            let mut peers = self.state.peers.write().await;
            peers.insert(
                peer_key.clone(),
                PeerEntry {
                    peer_id: full_peer_id.clone(),
                    direction: if will_initiate {
                        PeerDirection::Outbound
                    } else {
                        PeerDirection::Inbound
                    },
                    state: ConnectionState::Discovered,
                    last_seen: Instant::now(),
                    peer: None,
                    pool,
                    bytes_sent: 0,
                    bytes_received: 0,
                },
            );
        }

        // If we should initiate, create offer
        if will_initiate {
            self.initiate_connection(&full_peer_id, pool, relay_write_tx)
                .await?;
        }

        Ok(())
    }

    /// Initiate a connection to a peer (create and send offer)
    async fn initiate_connection(
        &self,
        peer_id: &PeerId,
        pool: PeerPool,
        relay_write_tx: &tokio::sync::broadcast::Sender<SignalingMessage>,
    ) -> Result<()> {
        let peer_key = peer_id.to_string();

        info!("Initiating connection to {} (pool: {:?})", peer_id.short(), pool);

        // Create peer connection with content store and state events
        let mut peer = Peer::new_with_store_and_events(
            peer_id.clone(),
            PeerDirection::Outbound,
            self.my_peer_id.clone(),
            self.signaling_tx.clone(),
            self.config.stun_servers.clone(),
            self.store.clone(),
            Some(self.state_event_tx.clone()),
        )
        .await?;

        peer.setup_handlers().await?;

        // Create offer
        let offer = peer.connect().await?;

        // Update state
        {
            let mut peers = self.state.peers.write().await;
            if let Some(entry) = peers.get_mut(&peer_key) {
                entry.state = ConnectionState::Connecting;
                entry.peer = Some(peer);
                entry.pool = pool;
            }
        }

        // Send offer
        let offer_msg = SignalingMessage::Offer {
            offer,
            recipient: peer_id.to_string(),
            peer_id: self.my_peer_id.uuid.clone(),
        };
        if relay_write_tx.send(offer_msg).is_err() {
            warn!("Failed to broadcast offer to {}", peer_id.short());
        }

        info!("Sent offer to {}", peer_id.short());

        Ok(())
    }

    /// Handle incoming offer
    async fn handle_offer(
        &self,
        sender_pubkey: &str,
        their_uuid: &str,
        offer: serde_json::Value,
        relay_write_tx: &tokio::sync::broadcast::Sender<SignalingMessage>,
    ) -> Result<()> {
        debug!("handle_offer ENTRY: sender={}, uuid={}", &sender_pubkey[..8.min(sender_pubkey.len())], their_uuid);
        let full_peer_id = PeerId::new(sender_pubkey.to_string(), Some(their_uuid.to_string()));
        let peer_key = full_peer_id.to_string();

        // Classify the peer into a pool
        let pool = (self.peer_classifier)(sender_pubkey);

        info!("Received offer from {} (pool: {:?})", full_peer_id.short(), pool);

        // Check if we already have this peer with an actual connection
        {
            let peers = self.state.peers.read().await;
            debug!("Checking for existing peer, peer_key: {}, known_peers: {}", peer_key, peers.len());
            if let Some(entry) = peers.get(&peer_key) {
                // Only skip if we have an actual peer connection (not just discovered)
                if entry.peer.is_some() {
                    debug!("Already have peer {} with connection, skipping offer", full_peer_id.short());
                    return Ok(());
                }
                debug!("Peer {} exists but has no connection, proceeding", full_peer_id.short());
            } else {
                debug!("Peer {} not found in peers map, will create new entry", full_peer_id.short());
            }
        }

        // Check pool limits
        let pool_counts = self.get_pool_counts().await;
        debug!("Pool counts: {:?}, checking can_accept_peer for {:?}", pool_counts, pool);
        if !self.can_accept_peer(pool, &pool_counts) {
            warn!("Rejecting offer from {} - pool {:?} is full", full_peer_id.short(), pool);
            return Ok(());
        }
        debug!("Pool check passed for {}", full_peer_id.short());

        // Create peer connection with content store and state events
        debug!("Creating peer connection for {}", full_peer_id.short());
        let mut peer = Peer::new_with_store_and_events(
            full_peer_id.clone(),
            PeerDirection::Inbound,
            self.my_peer_id.clone(),
            self.signaling_tx.clone(),
            self.config.stun_servers.clone(),
            self.store.clone(),
            Some(self.state_event_tx.clone()),
        )
        .await?;
        debug!("Peer connection created for {}", full_peer_id.short());

        peer.setup_handlers().await?;
        debug!("Handlers set up for {}", full_peer_id.short());

        // Handle offer and create answer
        let answer = peer.handle_offer(offer).await?;
        debug!("Answer created for {}", full_peer_id.short());

        // Update state
        {
            let mut peers = self.state.peers.write().await;
            peers.insert(
                peer_key,
                PeerEntry {
                    peer_id: full_peer_id.clone(),
                    direction: PeerDirection::Inbound,
                    state: ConnectionState::Connecting,
                    last_seen: Instant::now(),
                    peer: Some(peer),
                    pool,
                    bytes_sent: 0,
                    bytes_received: 0,
                },
            );
        }

        // Send answer
        // Note: peer_id is just the UUID, not full pubkey:uuid
        // The recipient will construct full peer_id from sender pubkey + this UUID
        let answer_msg = SignalingMessage::Answer {
            answer,
            recipient: full_peer_id.to_string(),
            peer_id: self.my_peer_id.uuid.clone(),
        };
        if relay_write_tx.send(answer_msg).is_err() {
            warn!("Failed to send answer to {}", full_peer_id.short());
        }
        info!("Sent answer to {}", full_peer_id.short());

        Ok(())
    }

    /// Handle incoming answer
    async fn handle_answer(
        &self,
        sender_pubkey: &str,
        their_uuid: &str,
        answer: serde_json::Value,
    ) -> Result<()> {
        let full_peer_id = PeerId::new(sender_pubkey.to_string(), Some(their_uuid.to_string()));
        let peer_key = full_peer_id.to_string();

        info!("Received answer from {}", full_peer_id.short());

        let mut peers = self.state.peers.write().await;
        if let Some(entry) = peers.get_mut(&peer_key) {
            // Skip if already connected - duplicate answers from multiple relays
            if entry.state == ConnectionState::Connected {
                debug!("Ignoring duplicate answer from {} - already connected", full_peer_id.short());
                return Ok(());
            }
            if let Some(ref mut peer) = entry.peer {
                // Check WebRTC signaling state before applying answer
                use webrtc::peer_connection::signaling_state::RTCSignalingState;
                let signaling_state = peer.signaling_state();
                if signaling_state != RTCSignalingState::HaveLocalOffer {
                    debug!(
                        "Ignoring answer from {} - signaling state is {:?}, not HaveLocalOffer",
                        full_peer_id.short(),
                        signaling_state
                    );
                    return Ok(());
                }
                peer.handle_answer(answer).await?;
                info!("Applied answer from {}", full_peer_id.short());
            } else {
                debug!("Peer {} has no connection object", full_peer_id.short());
            }
        } else {
            debug!("No peer found for key: {}", peer_key);
        }

        Ok(())
    }

    /// Handle incoming ICE candidate
    async fn handle_candidate(
        &self,
        sender_pubkey: &str,
        their_uuid: &str,
        candidate: serde_json::Value,
    ) -> Result<()> {
        let full_peer_id = PeerId::new(sender_pubkey.to_string(), Some(their_uuid.to_string()));
        let peer_key = full_peer_id.to_string();

        info!("Received ICE candidate from {}", full_peer_id.short());

        let mut peers = self.state.peers.write().await;
        if let Some(entry) = peers.get_mut(&peer_key) {
            if let Some(ref mut peer) = entry.peer {
                peer.handle_candidate(candidate).await?;
            }
        }

        Ok(())
    }

    /// Handle batched ICE candidates
    async fn handle_candidates(
        &self,
        sender_pubkey: &str,
        their_uuid: &str,
        candidates: Vec<serde_json::Value>,
    ) -> Result<()> {
        let full_peer_id = PeerId::new(sender_pubkey.to_string(), Some(their_uuid.to_string()));
        let peer_key = full_peer_id.to_string();

        debug!(
            "Received {} candidates from {}",
            candidates.len(),
            full_peer_id.short()
        );

        let mut peers = self.state.peers.write().await;
        if let Some(entry) = peers.get_mut(&peer_key) {
            if let Some(ref mut peer) = entry.peer {
                for candidate in candidates {
                    if let Err(e) = peer.handle_candidate(candidate).await {
                        debug!("Failed to add candidate: {}", e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle peer state change events from peer connections
    async fn handle_peer_state_event(&self, event: PeerStateEvent) {
        match event {
            PeerStateEvent::Connected(peer_id) => {
                let peer_key = peer_id.to_string();
                let mut peers = self.state.peers.write().await;
                if let Some(entry) = peers.get_mut(&peer_key) {
                    if entry.state != ConnectionState::Connected {
                        info!("Peer {} connected (via state event)", peer_id.short());
                        entry.state = ConnectionState::Connected;
                        // Update connected count
                        self.state.connected_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            PeerStateEvent::Failed(peer_id) => {
                let peer_key = peer_id.to_string();
                info!("Peer {} connection failed - removing from pool", peer_id.short());
                let mut peers = self.state.peers.write().await;
                if let Some(entry) = peers.remove(&peer_key) {
                    // Decrement connected count if was connected
                    if entry.state == ConnectionState::Connected {
                        self.state.connected_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Close the peer connection if it exists
                    if let Some(peer) = entry.peer {
                        let _ = peer.close().await;
                    }
                }
            }
            PeerStateEvent::Disconnected(peer_id) => {
                let peer_key = peer_id.to_string();
                info!("Peer {} disconnected - removing from pool", peer_id.short());
                let mut peers = self.state.peers.write().await;
                if let Some(entry) = peers.remove(&peer_key) {
                    // Decrement connected count if was connected
                    if entry.state == ConnectionState::Connected {
                        self.state.connected_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Close the peer connection if it exists
                    if let Some(peer) = entry.peer {
                        let _ = peer.close().await;
                    }
                }
            }
        }
    }

    /// Cleanup stale peers and sync connection states (fallback, runs every 30s)
    async fn cleanup_stale_peers(&self) {
        let mut peers = self.state.peers.write().await;
        let mut connected_count = 0;
        let mut to_remove = Vec::new();
        let stale_timeout = Duration::from_secs(60); // Remove peers stuck in Discovered/Connecting for 60s

        for (key, entry) in peers.iter_mut() {
            if let Some(ref peer) = entry.peer {
                // Sync connected state as fallback (in case event was missed)
                if peer.is_connected() {
                    if entry.state != ConnectionState::Connected {
                        info!("Peer {} is now connected (sync fallback)", entry.peer_id.short());
                        entry.state = ConnectionState::Connected;
                    }
                    connected_count += 1;
                } else if entry.state == ConnectionState::Connecting && entry.last_seen.elapsed() > stale_timeout {
                    // Peer stuck in Connecting for too long - mark for removal
                    info!("Removing stale peer {} (stuck in Connecting for {:?})", entry.peer_id.short(), entry.last_seen.elapsed());
                    to_remove.push(key.clone());
                }
            } else if entry.state == ConnectionState::Discovered && entry.last_seen.elapsed() > stale_timeout {
                // Discovered peer with no actual connection - remove
                debug!("Removing stale discovered peer {}", entry.peer_id.short());
                to_remove.push(key.clone());
            }
        }

        // Remove stale peers
        for key in to_remove {
            if let Some(entry) = peers.remove(&key) {
                if let Some(peer) = entry.peer {
                    let _ = peer.close().await;
                }
            }
        }

        self.state
            .connected_count
            .store(connected_count, std::sync::atomic::Ordering::Relaxed);
    }
}

// Keep the old PeerState for backward compatibility with tests
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PeerState {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub state: String,
    pub last_seen: Instant,
}
