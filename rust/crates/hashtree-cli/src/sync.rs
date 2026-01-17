//! Background sync service for auto-pulling trees from Nostr
//!
//! Subscribes to:
//! 1. Own trees (all visibility levels) - highest priority
//! 2. Followed users' public trees - lower priority
//!
//! Uses WebRTC peers first, falls back to Blossom HTTP servers

use anyhow::Result;
use hashtree_core::{from_hex, to_hex, Cid};
use nostr_sdk::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::fetch::{FetchConfig, Fetcher};
use crate::storage::{HashtreeStore, PRIORITY_OWN, PRIORITY_FOLLOWED};
use crate::webrtc::WebRTCState;

/// Sync priority levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncPriority {
    /// Own trees - highest priority
    Own = 0,
    /// Followed users' trees - lower priority
    Followed = 1,
}

/// A tree to sync
#[derive(Debug, Clone)]
pub struct SyncTask {
    /// Nostr key (npub.../treename)
    pub key: String,
    /// Content identifier
    pub cid: Cid,
    /// Priority level
    pub priority: SyncPriority,
    /// When this task was queued
    pub queued_at: Instant,
}

/// Configuration for background sync
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Enable syncing own trees
    pub sync_own: bool,
    /// Enable syncing followed users' public trees
    pub sync_followed: bool,
    /// Nostr relays for subscriptions
    pub relays: Vec<String>,
    /// Max concurrent sync tasks
    pub max_concurrent: usize,
    /// Timeout for WebRTC requests (ms)
    pub webrtc_timeout_ms: u64,
    /// Timeout for Blossom requests (ms)
    pub blossom_timeout_ms: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            sync_own: true,
            sync_followed: true,
            relays: hashtree_config::DEFAULT_RELAYS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_concurrent: 3,
            webrtc_timeout_ms: 2000,
            blossom_timeout_ms: 10000,
        }
    }
}

impl SyncConfig {
    /// Create from hashtree_config (respects user's config.toml)
    pub fn from_config(config: &hashtree_config::Config) -> Self {
        Self {
            sync_own: true,
            sync_followed: true,
            relays: config.nostr.relays.clone(),
            max_concurrent: 3,
            webrtc_timeout_ms: 2000,
            blossom_timeout_ms: 10000,
        }
    }
}

/// State for a subscribed tree
#[allow(dead_code)]
struct TreeSubscription {
    key: String,
    current_cid: Option<Cid>,
    priority: SyncPriority,
    last_synced: Option<Instant>,
}

/// Background sync service
pub struct BackgroundSync {
    config: SyncConfig,
    store: Arc<HashtreeStore>,
    webrtc_state: Option<Arc<WebRTCState>>,
    /// Nostr client for subscriptions
    client: Client,
    /// Our public key
    my_pubkey: PublicKey,
    /// Subscribed trees
    subscriptions: Arc<RwLock<HashMap<String, TreeSubscription>>>,
    /// Sync queue
    queue: Arc<RwLock<VecDeque<SyncTask>>>,
    /// Currently syncing hashes
    syncing: Arc<RwLock<HashSet<String>>>,
    /// Shutdown signal
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    /// Fetcher for remote content
    fetcher: Arc<Fetcher>,
}

impl BackgroundSync {
    /// Create a new background sync service
    pub async fn new(
        config: SyncConfig,
        store: Arc<HashtreeStore>,
        keys: Keys,
        webrtc_state: Option<Arc<WebRTCState>>,
    ) -> Result<Self> {
        let my_pubkey = keys.public_key();
        let client = Client::new(keys);

        // Add relays
        for relay in &config.relays {
            if let Err(e) = client.add_relay(relay).await {
                warn!("Failed to add relay {}: {}", relay, e);
            }
        }

        // Connect to relays
        client.connect().await;

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Create fetcher with config
        // BlossomClient auto-loads servers from ~/.hashtree/config.toml
        let fetch_config = FetchConfig {
            webrtc_timeout: Duration::from_millis(config.webrtc_timeout_ms),
            blossom_timeout: Duration::from_millis(config.blossom_timeout_ms),
        };
        let fetcher = Arc::new(Fetcher::new(fetch_config));

        Ok(Self {
            config,
            store,
            webrtc_state,
            client,
            my_pubkey,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            queue: Arc::new(RwLock::new(VecDeque::new())),
            syncing: Arc::new(RwLock::new(HashSet::new())),
            shutdown_tx,
            shutdown_rx,
            fetcher,
        })
    }

    /// Start the background sync service
    pub async fn run(&self, contacts_file: PathBuf) -> Result<()> {
        info!("Starting background sync service");

        // Wait for relays to connect before subscribing
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Subscribe to own trees
        if self.config.sync_own {
            self.subscribe_own_trees().await?;
        }

        // Subscribe to followed users' trees
        if self.config.sync_followed {
            self.subscribe_followed_trees(&contacts_file).await?;
        }

        // Start sync worker
        let queue = self.queue.clone();
        let syncing = self.syncing.clone();
        let store = self.store.clone();
        let webrtc_state = self.webrtc_state.clone();
        let fetcher = self.fetcher.clone();
        let max_concurrent = self.config.max_concurrent;
        let mut shutdown_rx = self.shutdown_rx.clone();

        // Spawn sync worker task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            info!("Sync worker shutting down");
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        // Check if we can start more sync tasks
                        let current_syncing = syncing.read().await.len();
                        if current_syncing >= max_concurrent {
                            continue;
                        }

                        // Get next task from queue
                        let task = {
                            let mut q = queue.write().await;
                            q.pop_front()
                        };

                        if let Some(task) = task {
                            let hash_hex = to_hex(&task.cid.hash);

                            // Check if already syncing
                            {
                                let mut s = syncing.write().await;
                                if s.contains(&hash_hex) {
                                    continue;
                                }
                                s.insert(hash_hex.clone());
                            }

                            // Spawn sync task
                            let syncing_clone = syncing.clone();
                            let store_clone = store.clone();
                            let webrtc_clone = webrtc_state.clone();
                            let fetcher_clone = fetcher.clone();

                            tokio::spawn(async move {
                                let result = fetcher_clone.fetch_tree(
                                    &store_clone,
                                    webrtc_clone.as_ref(),
                                    &task.cid.hash,
                                ).await;

                                match result {
                                    Ok((chunks_fetched, bytes_fetched)) => {
                                        if chunks_fetched > 0 {
                                            info!(
                                                "Synced tree {} ({} chunks, {} bytes)",
                                                &hash_hex[..12],
                                                chunks_fetched,
                                                bytes_fetched
                                            );

                                            // Index the tree for eviction tracking
                                            // Extract owner from key (format: "npub.../treename" or "pubkey/treename")
                                            let (owner, name) = task.key.split_once('/')
                                                .map(|(o, n)| (o.to_string(), Some(n)))
                                                .unwrap_or((task.key.clone(), None));

                                            // Map SyncPriority to storage priority
                                            let storage_priority = match task.priority {
                                                SyncPriority::Own => PRIORITY_OWN,
                                                SyncPriority::Followed => PRIORITY_FOLLOWED,
                                            };

                                            if let Err(e) = store_clone.index_tree(
                                                &task.cid.hash,
                                                &owner,
                                                name,
                                                storage_priority,
                                                Some(&task.key), // ref_key for replacing old versions
                                            ) {
                                                warn!("Failed to index tree {}: {}", &hash_hex[..12], e);
                                            }

                                            // Check if eviction is needed
                                            if let Err(e) = store_clone.evict_if_needed() {
                                                warn!("Eviction check failed: {}", e);
                                            }
                                        } else {
                                            tracing::debug!("Tree {} already synced", &hash_hex[..12]);
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to sync tree {}: {}", &hash_hex[..12], e);
                                    }
                                }

                                // Remove from syncing set
                                syncing_clone.write().await.remove(&hash_hex);
                            });
                        }
                    }
                }
            }
        });

        // Handle Nostr notifications for tree updates
        let mut notifications = self.client.notifications();
        let subscriptions = self.subscriptions.clone();
        let queue = self.queue.clone();
        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("Background sync shutting down");
                        break;
                    }
                }
                notification = notifications.recv() => {
                    match notification {
                        Ok(RelayPoolNotification::Event { event, .. }) => {
                            self.handle_tree_event(&event, &subscriptions, &queue).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            error!("Notification error: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Subscribe to own trees (kind 30078 events from our pubkey)
    async fn subscribe_own_trees(&self) -> Result<()> {
        let filter = Filter::new()
            .kind(Kind::Custom(30078))
            .author(self.my_pubkey)
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), vec!["hashtree"]);

        match self.client.subscribe(vec![filter], None).await {
            Ok(_) => {
                info!(
                    "Subscribed to own trees for {}",
                    self.my_pubkey.to_bech32().unwrap_or_default()
                );
            }
            Err(e) => {
                warn!("Failed to subscribe to own trees (will retry on reconnect): {}", e);
            }
        }

        Ok(())
    }

    /// Subscribe to followed users' trees
    async fn subscribe_followed_trees(&self, contacts_file: &PathBuf) -> Result<()> {
        // Load contacts from file
        let contacts: Vec<String> = if contacts_file.exists() {
            let data = std::fs::read_to_string(contacts_file)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Vec::new()
        };

        if contacts.is_empty() {
            info!("No contacts to subscribe to");
            return Ok(());
        }

        // Convert hex pubkeys to PublicKey
        let pubkeys: Vec<PublicKey> = contacts
            .iter()
            .filter_map(|hex| PublicKey::from_hex(hex).ok())
            .collect();

        if pubkeys.is_empty() {
            return Ok(());
        }

        // Subscribe to all followed users' hashtree events
        let filter = Filter::new()
            .kind(Kind::Custom(30078))
            .authors(pubkeys.clone())
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), vec!["hashtree"]);

        match self.client.subscribe(vec![filter], None).await {
            Ok(_) => {
                info!("Subscribed to {} followed users' trees", pubkeys.len());
            }
            Err(e) => {
                warn!("Failed to subscribe to followed trees (will retry on reconnect): {}", e);
            }
        }

        Ok(())
    }

    /// Handle incoming tree event
    async fn handle_tree_event(
        &self,
        event: &Event,
        subscriptions: &Arc<RwLock<HashMap<String, TreeSubscription>>>,
        queue: &Arc<RwLock<VecDeque<SyncTask>>>,
    ) {
        // Check if it's a hashtree event
        let has_hashtree_tag = event.tags.iter().any(|tag| {
            let v = tag.as_slice();
            v.len() >= 2 && v[0] == "l" && v[1] == "hashtree"
        });

        if !has_hashtree_tag || event.kind != Kind::Custom(30078) {
            return;
        }

        // Extract d-tag (tree name)
        let d_tag = event.tags.iter().find_map(|tag| {
            if let Some(TagStandard::Identifier(id)) = tag.as_standardized() {
                Some(id.clone())
            } else {
                None
            }
        });

        let tree_name = match d_tag {
            Some(name) => name,
            None => return,
        };

        // Extract hash and key from tags
        let mut hash_hex: Option<String> = None;
        let mut key_hex: Option<String> = None;

        for tag in event.tags.iter() {
            let tag_vec = tag.as_slice();
            if tag_vec.len() >= 2 {
                match tag_vec[0].as_str() {
                    "hash" => hash_hex = Some(tag_vec[1].clone()),
                    "key" => key_hex = Some(tag_vec[1].clone()),
                    _ => {}
                }
            }
        }

        let hash = match hash_hex.and_then(|h| from_hex(&h).ok()) {
            Some(h) => h,
            None => return,
        };

        let key = key_hex.and_then(|k| {
            let bytes = hex::decode(&k).ok()?;
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        });

        let cid = Cid { hash, key };

        // Build key
        let npub = event.pubkey.to_bech32().unwrap_or_else(|_| event.pubkey.to_hex());
        let key = format!("{}/{}", npub, tree_name);

        // Determine priority
        let priority = if event.pubkey == self.my_pubkey {
            SyncPriority::Own
        } else {
            SyncPriority::Followed
        };

        // Check if we need to sync
        let should_sync = {
            let mut subs = subscriptions.write().await;
            let sub = subs.entry(key.clone()).or_insert(TreeSubscription {
                key: key.clone(),
                current_cid: None,
                priority,
                last_synced: None,
            });

            // Check if CID changed
            let changed = sub.current_cid.as_ref().map(|c| c.hash) != Some(cid.hash);
            if changed {
                sub.current_cid = Some(cid.clone());
                true
            } else {
                false
            }
        };

        if should_sync {
            info!("New tree update: {} -> {}", key, to_hex(&cid.hash)[..12].to_string());

            // Add to sync queue
            let task = SyncTask {
                key,
                cid,
                priority,
                queued_at: Instant::now(),
            };

            let mut q = queue.write().await;

            // Insert based on priority (own trees first)
            let insert_pos = q
                .iter()
                .position(|t| t.priority > task.priority)
                .unwrap_or(q.len());
            q.insert(insert_pos, task);
        }
    }

    /// Signal shutdown
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Queue a manual sync for a specific tree
    pub async fn queue_sync(&self, key: &str, cid: Cid, priority: SyncPriority) {
        let task = SyncTask {
            key: key.to_string(),
            cid,
            priority,
            queued_at: Instant::now(),
        };

        let mut q = self.queue.write().await;
        let insert_pos = q
            .iter()
            .position(|t| t.priority > task.priority)
            .unwrap_or(q.len());
        q.insert(insert_pos, task);
    }

    /// Get current sync status
    pub async fn status(&self) -> SyncStatus {
        let subscriptions = self.subscriptions.read().await;
        let queue = self.queue.read().await;
        let syncing = self.syncing.read().await;

        SyncStatus {
            subscribed_trees: subscriptions.len(),
            queued_tasks: queue.len(),
            active_syncs: syncing.len(),
        }
    }
}

/// Overall sync status
#[derive(Debug, Clone)]
pub struct SyncStatus {
    pub subscribed_trees: usize,
    pub queued_tasks: usize,
    pub active_syncs: usize,
}
