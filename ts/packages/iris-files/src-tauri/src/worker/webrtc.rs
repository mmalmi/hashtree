//! WebRTC peer connection manager for Tauri
//!
//! Integrates hashtree-webrtc with Tauri, sharing the Nostr client
//! with NostrManager to avoid duplicate relay connections.

use hashtree_webrtc::{
    ClassifyRequest, NostrRelayTransport, PeerPool, PoolConfig, PoolSettings,
    RealPeerConnectionFactory, RelayTransport, SignalingManager,
};
use nostr_sdk::{Client, Keys};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Peer statistics for frontend display
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerStats {
    pub peer_id: String,
    pub connected: bool,
    pub pool: String,
}

/// WebRTC manager that shares Nostr client with NostrManager
#[derive(Clone)]
pub struct WebRTCManager {
    /// Transport (needed for recv loop)
    transport: Arc<RwLock<Option<Arc<NostrRelayTransport>>>>,
    /// Signaling manager (handles peer discovery and connections)
    signaling:
        Arc<RwLock<Option<Arc<SignalingManager<NostrRelayTransport, RealPeerConnectionFactory>>>>>,
    /// Our peer UUID (unique per session)
    peer_uuid: String,
    /// Pool settings
    pools: Arc<RwLock<PoolSettings>>,
    /// Follows set for peer classification
    follows: Arc<RwLock<HashSet<String>>>,
    /// Classifier channel sender
    classifier_tx: Arc<RwLock<Option<mpsc::Sender<ClassifyRequest>>>>,
    /// Running flag for background task
    running: Arc<RwLock<bool>>,
    /// Debug mode
    debug: bool,
}

impl WebRTCManager {
    pub fn new() -> Self {
        Self {
            transport: Arc::new(RwLock::new(None)),
            signaling: Arc::new(RwLock::new(None)),
            peer_uuid: Uuid::new_v4().to_string(),
            pools: Arc::new(RwLock::new(PoolSettings {
                follows: PoolConfig {
                    max_connections: 20,
                    satisfied_connections: 10,
                },
                other: PoolConfig {
                    max_connections: 10,
                    satisfied_connections: 2,
                },
            })),
            follows: Arc::new(RwLock::new(HashSet::new())),
            classifier_tx: Arc::new(RwLock::new(None)),
            running: Arc::new(RwLock::new(false)),
            debug: false,
        }
    }

    /// Initialize WebRTC with a shared Nostr client
    ///
    /// Call this after NostrManager has been initialized and identity has been set.
    pub async fn init(&self, client: Client, keys: Keys) -> Result<(), String> {
        // Check if already initialized
        if self.signaling.read().await.is_some() {
            debug!("WebRTC already initialized");
            return Ok(());
        }

        info!("Initializing WebRTC with shared Nostr client...");

        // Create transport using the shared client
        let transport = Arc::new(NostrRelayTransport::with_client(
            client,
            keys,
            self.peer_uuid.clone(),
            self.debug,
        ));

        // Connect transport (relays already added by NostrManager, pass empty slice)
        transport
            .connect(&[])
            .await
            .map_err(|e| format!("Failed to connect transport: {:?}", e))?;

        // Store transport
        *self.transport.write().await = Some(transport.clone());

        // Create peer connection factory
        let factory = Arc::new(RealPeerConnectionFactory::new());

        // Get pool settings
        let pools = self.pools.read().await.clone();

        // Get peer_id and pubkey from transport
        let peer_id = transport.peer_id().to_string();
        let pubkey = transport.pubkey().to_string();

        // Create signaling manager
        let mut signaling = SignalingManager::new(
            peer_id, pubkey, transport, factory, pools, self.debug,
        );

        // Set up classifier channel for follows/other pool assignment
        let (classifier_tx, classifier_rx) = mpsc::channel::<ClassifyRequest>(100);
        signaling.set_classifier(classifier_tx.clone());
        *self.classifier_tx.write().await = Some(classifier_tx);

        let signaling = Arc::new(signaling);
        *self.signaling.write().await = Some(signaling.clone());

        // Start running
        *self.running.write().await = true;

        // Start classifier handler
        self.start_classifier_handler(classifier_rx).await;

        // Start message receive loop
        self.start_recv_loop().await;

        // Send initial hello
        info!("Sending initial WebRTC hello...");
        match signaling.send_hello(vec![]).await {
            Ok(()) => info!("Initial hello sent successfully"),
            Err(e) => {
                warn!("Failed to send initial hello: {:?}", e);
                return Err(format!("Failed to send hello: {:?}", e));
            }
        }

        // Start hello timer
        self.start_hello_timer().await;

        info!("WebRTC initialized successfully");
        Ok(())
    }

    /// Start the classifier handler that determines peer pool assignment
    async fn start_classifier_handler(&self, mut rx: mpsc::Receiver<ClassifyRequest>) {
        let follows = self.follows.clone();
        let running = self.running.clone();

        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                if !*running.read().await {
                    break;
                }

                // Check if pubkey is in follows set
                let pool = if follows.read().await.contains(&req.pubkey) {
                    PeerPool::Follows
                } else {
                    PeerPool::Other
                };

                let _ = req.response.send(pool);
            }
        });
    }

    /// Start the message receive loop
    async fn start_recv_loop(&self) {
        let transport = self.transport.clone();
        let signaling = self.signaling.clone();
        let running = self.running.clone();

        tokio::spawn(async move {
            loop {
                if !*running.read().await {
                    break;
                }

                // Get transport
                let transport = transport.read().await.clone();
                let signaling = signaling.read().await.clone();

                if let (Some(transport), Some(signaling)) = (transport, signaling) {
                    // Use try_recv instead of recv to avoid losing the receiver on timeout
                    if let Some(msg) = transport.try_recv() {
                        debug!("Received signaling message: {:?}", msg);
                        if let Err(e) = signaling.handle_message(msg).await {
                            warn!("Failed to handle signaling message: {:?}", e);
                        }
                    } else {
                        // No message, wait a bit before polling again
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                } else {
                    // Not initialized, wait a bit
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        });
    }

    /// Start periodic hello sender
    async fn start_hello_timer(&self) {
        let signaling = self.signaling.clone();
        let running = self.running.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

            loop {
                interval.tick().await;

                if !*running.read().await {
                    break;
                }

                if let Some(sig) = signaling.read().await.as_ref() {
                    match sig.send_hello(vec![]).await {
                        Ok(()) => debug!("Periodic hello sent"),
                        Err(e) => warn!("Failed to send periodic hello: {:?}", e),
                    }
                }
            }
        });
    }

    /// Send a hello message to discover peers
    pub async fn send_hello(&self, roots: Vec<String>) -> Result<(), String> {
        let signaling = self.signaling.read().await;
        if let Some(ref sig) = *signaling {
            sig.send_hello(roots)
                .await
                .map_err(|e| format!("Failed to send hello: {:?}", e))
        } else {
            Err("WebRTC not initialized".to_string())
        }
    }

    /// Get connected peer statistics
    pub async fn get_peer_stats(&self) -> Vec<PeerStats> {
        let signaling = self.signaling.read().await;
        if let Some(ref sig) = *signaling {
            // Get peer IDs and check each one
            let peer_ids = sig.peer_ids().await;
            let mut stats = Vec::new();

            for peer_id in peer_ids {
                // Extract pubkey from peer_id (format: "pubkey:uuid")
                let pubkey = peer_id.split(':').next().unwrap_or("").to_string();

                // Check if it's in follows
                let pool = if self.follows.read().await.contains(&pubkey) {
                    "follows"
                } else {
                    "other"
                };

                // Check if channel is open
                let connected = sig.get_channel(&peer_id).await.map(|c| c.is_open()).unwrap_or(false);

                stats.push(PeerStats {
                    peer_id,
                    connected,
                    pool: pool.to_string(),
                });
            }

            stats
        } else {
            Vec::new()
        }
    }

    /// Get count of connected peers
    pub async fn peer_count(&self) -> usize {
        let signaling = self.signaling.read().await;
        if let Some(ref sig) = *signaling {
            sig.peer_count().await
        } else {
            0
        }
    }

    /// Update pool settings
    pub async fn set_pools(
        &self,
        follows_max: usize,
        follows_satisfied: usize,
        other_max: usize,
        other_satisfied: usize,
    ) {
        let mut pools = self.pools.write().await;
        pools.follows.max_connections = follows_max;
        pools.follows.satisfied_connections = follows_satisfied;
        pools.other.max_connections = other_max;
        pools.other.satisfied_connections = other_satisfied;

        // Note: SignalingManager doesn't have update_pools method
        // Pool settings are used at construction time
        // For dynamic updates, would need to add that to SignalingManager
    }

    /// Update the follows set for peer classification
    pub async fn update_follows(&self, pubkeys: Vec<String>) {
        let mut follows = self.follows.write().await;
        follows.clear();
        follows.extend(pubkeys);
        debug!("Updated follows set with {} pubkeys", follows.len());
    }

    /// Add a pubkey to the follows set
    pub async fn add_follow(&self, pubkey: String) {
        self.follows.write().await.insert(pubkey);
    }

    /// Remove a pubkey from the follows set
    pub async fn remove_follow(&self, pubkey: &str) {
        self.follows.write().await.remove(pubkey);
    }

    /// Shutdown WebRTC
    pub async fn shutdown(&self) {
        *self.running.write().await = false;

        if let Some(transport) = self.transport.write().await.take() {
            transport.disconnect().await;
        }

        self.signaling.write().await.take();
        self.classifier_tx.write().await.take();

        info!("WebRTC shut down");
    }
}

impl Default for WebRTCManager {
    fn default() -> Self {
        Self::new()
    }
}
