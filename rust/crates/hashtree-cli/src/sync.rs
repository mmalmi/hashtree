//! Background sync service for auto-pulling trees from Nostr
//!
//! Subscribes to:
//! 1. Own trees (all visibility levels) - highest priority
//! 2. Followed users' public trees - lower priority
//!
//! Blob reads use configured Blossom HTTP servers.

use anyhow::Result;
use git_remote_htree::nostr_client::{hashtree_root_kinds, is_hashtree_root_kind, load_keys};
use hashtree_core::{from_hex, to_hex, Cid};
use nostr_sdk::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::fetch::{FetchConfig, Fetcher};
use crate::storage::{HashtreeStore, PRIORITY_FOLLOWED, PRIORITY_OWN};

/// Sync priority levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncPriority {
    /// Explicitly pinned mutable refs - highest priority
    Pinned = 0,
    /// Explicitly tracked authors - mirrors all readable trees for that author
    TrackedAuthor = 1,
    /// Own trees - high priority
    Own = 2,
    /// Followed users' trees - lower priority
    Followed = 3,
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

fn build_exact_tree_filter(key: &str) -> Result<Filter> {
    let (npub, tree_name) = key
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("Invalid pinned ref key: {}", key))?;
    let author = PublicKey::from_bech32(npub)
        .map_err(|_| anyhow::anyhow!("Invalid npub in pinned ref key: {}", key))?;

    Ok(Filter::new()
        .kinds(hashtree_root_kinds())
        .author(author)
        .custom_tag(
            SingleLetterTag::lowercase(Alphabet::D),
            tree_name.to_string(),
        )
        .custom_tag(SingleLetterTag::lowercase(Alphabet::L), "hashtree"))
}

fn build_author_tree_filter(author: PublicKey) -> Filter {
    Filter::new()
        .kinds(hashtree_root_kinds())
        .author(author)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::L), "hashtree")
}

fn load_author_signing_keys() -> HashMap<String, Keys> {
    load_keys()
        .into_iter()
        .filter_map(|stored| {
            let secret_hex = stored.secret_hex?;
            let secret_bytes = hex::decode(&secret_hex).ok()?;
            let secret = SecretKey::from_slice(&secret_bytes).ok()?;
            Some((stored.pubkey_hex, Keys::new(secret)))
        })
        .collect()
}

fn cid_from_tree_event(event: &Event, author_keys: Option<&Keys>) -> Option<Cid> {
    let mut hash_hex: Option<String> = None;
    let mut key_hex: Option<String> = None;
    let mut encrypted_key: Option<String> = None;
    let mut self_encrypted_key: Option<String> = None;

    for tag in event.tags.iter() {
        let tag_vec = tag.as_slice();
        if tag_vec.len() < 2 {
            continue;
        }

        match tag_vec[0].as_str() {
            "hash" => hash_hex = Some(tag_vec[1].clone()),
            "key" => key_hex = Some(tag_vec[1].clone()),
            "encryptedKey" => encrypted_key = Some(tag_vec[1].clone()),
            "selfEncryptedKey" => self_encrypted_key = Some(tag_vec[1].clone()),
            _ => {}
        }
    }

    let hash = from_hex(&hash_hex?).ok()?;

    if let Some(key_hex) = key_hex {
        let bytes = hex::decode(&key_hex).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Some(Cid {
            hash,
            key: Some(key),
        });
    }

    if let Some(ciphertext) = self_encrypted_key {
        let keys = author_keys?;
        if keys.public_key() != event.pubkey {
            return None;
        }
        let key_hex = nip44::decrypt(keys.secret_key(), &event.pubkey, &ciphertext).ok()?;
        let bytes = hex::decode(&key_hex).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Some(Cid {
            hash,
            key: Some(key),
        });
    }

    if encrypted_key.is_some() {
        return None;
    }

    Some(Cid { hash, key: None })
}

fn classify_sync_event(
    key: &str,
    author_hex: &str,
    my_pubkey: &PublicKey,
    pinned_refs: &HashSet<String>,
    tracked_authors: &HashSet<String>,
    followed_authors: &HashSet<String>,
) -> Option<SyncPriority> {
    if pinned_refs.contains(key) {
        return Some(SyncPriority::Pinned);
    }

    if tracked_authors.contains(author_hex) {
        return Some(SyncPriority::TrackedAuthor);
    }

    if author_hex == my_pubkey.to_hex() {
        return Some(SyncPriority::Own);
    }

    if followed_authors.contains(author_hex) {
        return Some(SyncPriority::Followed);
    }

    None
}

fn apply_synced_tree_update(store: &HashtreeStore, task: &SyncTask) -> Result<()> {
    let (owner, name) = task
        .key
        .split_once('/')
        .map(|(o, n)| (o.to_string(), Some(n)))
        .unwrap_or((task.key.clone(), None));

    let storage_priority = match task.priority {
        SyncPriority::Pinned | SyncPriority::TrackedAuthor | SyncPriority::Own => PRIORITY_OWN,
        SyncPriority::Followed => PRIORITY_FOLLOWED,
    };

    if matches!(
        task.priority,
        SyncPriority::Pinned | SyncPriority::TrackedAuthor
    ) {
        store.pin(&task.cid.hash)?;
    }

    store.index_tree(
        &task.cid.hash,
        &owner,
        name,
        storage_priority,
        Some(&task.key),
    )?;

    store.evict_if_needed()?;
    Ok(())
}

/// Background sync service
pub struct BackgroundSync {
    config: SyncConfig,
    store: Arc<HashtreeStore>,
    /// Nostr client for subscriptions
    client: Client,
    /// Our public key
    my_pubkey: PublicKey,
    /// Subscribed trees
    subscriptions: Arc<RwLock<HashMap<String, TreeSubscription>>>,
    /// Followed authors that are allowed to generate sync tasks
    followed_authors: Arc<RwLock<HashSet<String>>>,
    /// Currently pinned mutable refs that should keep following updates
    pinned_refs: Arc<RwLock<HashSet<String>>>,
    /// Authors whose readable trees should be mirrored continuously
    tracked_authors: Arc<RwLock<HashSet<String>>>,
    /// Exact pinned refs already subscribed at the relay layer
    subscribed_pinned_refs: Arc<RwLock<HashSet<String>>>,
    /// Tracked authors already subscribed at the relay layer
    subscribed_tracked_authors: Arc<RwLock<HashSet<String>>>,
    /// Local signing keys available for decrypting owner-private roots
    author_signing_keys: Arc<RwLock<HashMap<String, Keys>>>,
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
    pub async fn new(config: SyncConfig, store: Arc<HashtreeStore>, keys: Keys) -> Result<Self> {
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
            blossom_timeout: Duration::from_millis(config.blossom_timeout_ms),
        };
        let fetcher = Arc::new(Fetcher::new(fetch_config));

        Ok(Self {
            config,
            store,
            client,
            my_pubkey,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            followed_authors: Arc::new(RwLock::new(HashSet::new())),
            pinned_refs: Arc::new(RwLock::new(HashSet::new())),
            tracked_authors: Arc::new(RwLock::new(HashSet::new())),
            subscribed_pinned_refs: Arc::new(RwLock::new(HashSet::new())),
            subscribed_tracked_authors: Arc::new(RwLock::new(HashSet::new())),
            author_signing_keys: Arc::new(RwLock::new(load_author_signing_keys())),
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

        self.refresh_author_signing_keys().await;
        self.refresh_pinned_ref_subscriptions().await?;
        self.refresh_tracked_author_subscriptions().await?;

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
                            let fetcher_clone = fetcher.clone();

                            tokio::spawn(async move {
                                let result = fetcher_clone.fetch_cid_tree(
                                    &store_clone,
                                    &task.cid,
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
                                        } else {
                                            tracing::debug!(
                                                "Tree {} already present locally; applying ref update",
                                                &hash_hex[..12]
                                            );
                                        }

                                        match store_clone.blob_exists(&task.cid.hash) {
                                            Ok(true) => {}
                                            Ok(false) => {
                                                warn!(
                                                    "Skipping ref update for {} because root {} is still missing locally",
                                                    task.key,
                                                    &hash_hex[..12]
                                                );
                                                syncing_clone.write().await.remove(&hash_hex);
                                                return;
                                            }
                                            Err(err) => {
                                                warn!(
                                                    "Failed to verify synced root {} before indexing {}: {}",
                                                    &hash_hex[..12],
                                                    task.key,
                                                    err
                                                );
                                                syncing_clone.write().await.remove(&hash_hex);
                                                return;
                                            }
                                        }

                                        if let Err(e) = apply_synced_tree_update(&store_clone, &task) {
                                            warn!("Failed to apply synced tree {}: {}", &hash_hex[..12], e);
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
        let mut pinned_refresh = tokio::time::interval(Duration::from_secs(5));
        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("Background sync shutting down");
                        break;
                    }
                }
                _ = pinned_refresh.tick() => {
                    self.refresh_author_signing_keys().await;
                    if let Err(err) = self.refresh_pinned_ref_subscriptions().await {
                        warn!("Failed to refresh pinned ref subscriptions: {}", err);
                    }
                    if let Err(err) = self.refresh_tracked_author_subscriptions().await {
                        warn!("Failed to refresh tracked author subscriptions: {}", err);
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

    async fn refresh_pinned_ref_subscriptions(&self) -> Result<()> {
        let current_refs: HashSet<String> = self.store.list_pinned_refs()?.into_iter().collect();
        {
            let mut pinned_refs = self.pinned_refs.write().await;
            *pinned_refs = current_refs.clone();
        }

        {
            let mut subscriptions = self.subscriptions.write().await;
            subscriptions.retain(|key, sub| {
                sub.priority != SyncPriority::Pinned || current_refs.contains(key)
            });
        }

        let new_refs: Vec<String> = {
            let subscribed = self.subscribed_pinned_refs.read().await;
            current_refs
                .iter()
                .filter(|key| !subscribed.contains(*key))
                .cloned()
                .collect()
        };

        for key in new_refs {
            let filter = match build_exact_tree_filter(&key) {
                Ok(filter) => filter,
                Err(err) => {
                    warn!("Ignoring invalid pinned ref {}: {}", key, err);
                    continue;
                }
            };

            match self.client.subscribe(filter, None).await {
                Ok(_) => {
                    info!("Subscribed to pinned ref {}", key);
                    self.subscribed_pinned_refs.write().await.insert(key);
                }
                Err(err) => {
                    warn!(
                        "Failed to subscribe to pinned ref (will retry on refresh): {}",
                        err
                    );
                }
            }
        }

        Ok(())
    }

    async fn refresh_author_signing_keys(&self) {
        let mut author_signing_keys = self.author_signing_keys.write().await;
        *author_signing_keys = load_author_signing_keys();
    }

    async fn refresh_tracked_author_subscriptions(&self) -> Result<()> {
        let tracked_npubs = self.store.list_tracked_authors()?;
        let parsed_authors: Vec<(String, PublicKey, String)> = tracked_npubs
            .into_iter()
            .filter_map(|npub| match PublicKey::from_bech32(&npub) {
                Ok(pubkey) => Some((npub, pubkey, pubkey.to_hex())),
                Err(err) => {
                    warn!("Ignoring invalid tracked author {}: {}", npub, err);
                    None
                }
            })
            .collect();
        let current_authors: HashSet<String> = parsed_authors
            .iter()
            .map(|(_, _, author_hex)| author_hex.clone())
            .collect();

        {
            let mut tracked_authors = self.tracked_authors.write().await;
            *tracked_authors = current_authors.clone();
        }

        {
            let mut subscriptions = self.subscriptions.write().await;
            subscriptions.retain(|key, sub| {
                if sub.priority != SyncPriority::TrackedAuthor {
                    return true;
                }

                let Some((npub, _)) = key.split_once('/') else {
                    return false;
                };
                let Ok(author) = PublicKey::from_bech32(npub) else {
                    return false;
                };
                current_authors.contains(&author.to_hex())
            });
        }

        let new_authors: Vec<(String, PublicKey)> = {
            let subscribed = self.subscribed_tracked_authors.read().await;
            parsed_authors
                .iter()
                .filter(|(_, _, author_hex)| !subscribed.contains(author_hex))
                .map(|(_, pubkey, author_hex)| (author_hex.clone(), *pubkey))
                .collect()
        };

        for (author_hex, author) in new_authors {
            match self
                .client
                .subscribe(build_author_tree_filter(author), None)
                .await
            {
                Ok(_) => {
                    info!(
                        "Subscribed to tracked author {}",
                        author.to_bech32().unwrap_or(author_hex.clone())
                    );
                    self.subscribed_tracked_authors
                        .write()
                        .await
                        .insert(author_hex);
                }
                Err(err) => {
                    warn!(
                        "Failed to subscribe to tracked author (will retry on refresh): {}",
                        err
                    );
                }
            }
        }

        Ok(())
    }

    /// Subscribe to own trees from our pubkey.
    async fn subscribe_own_trees(&self) -> Result<()> {
        let filter = build_author_tree_filter(self.my_pubkey);

        match self.client.subscribe(filter, None).await {
            Ok(_) => {
                info!(
                    "Subscribed to own trees for {}",
                    self.my_pubkey.to_bech32().unwrap_or_default()
                );
            }
            Err(e) => {
                warn!(
                    "Failed to subscribe to own trees (will retry on reconnect): {}",
                    e
                );
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
            self.followed_authors.write().await.clear();
            info!("No contacts to subscribe to");
            return Ok(());
        }

        {
            let mut followed_authors = self.followed_authors.write().await;
            *followed_authors = contacts.iter().cloned().collect();
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
            .kinds(hashtree_root_kinds())
            .authors(pubkeys.clone())
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), "hashtree");

        match self.client.subscribe(filter, None).await {
            Ok(_) => {
                info!("Subscribed to {} followed users' trees", pubkeys.len());
            }
            Err(e) => {
                warn!(
                    "Failed to subscribe to followed trees (will retry on reconnect): {}",
                    e
                );
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

        if !has_hashtree_tag || !is_hashtree_root_kind(event.kind) {
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

        // Build key
        let npub = event
            .pubkey
            .to_bech32()
            .unwrap_or_else(|_| event.pubkey.to_hex());
        let key = format!("{}/{}", npub, tree_name);

        let author_hex = event.pubkey.to_hex();
        let pinned_refs = self.pinned_refs.read().await.clone();
        let tracked_authors = self.tracked_authors.read().await.clone();
        let followed_authors = self.followed_authors.read().await.clone();

        // Determine priority and ignore stale events from refs we no longer care about.
        let Some(priority) = classify_sync_event(
            &key,
            &author_hex,
            &self.my_pubkey,
            &pinned_refs,
            &tracked_authors,
            &followed_authors,
        ) else {
            return;
        };

        let author_keys = self
            .author_signing_keys
            .read()
            .await
            .get(&author_hex)
            .cloned();
        let Some(cid) = cid_from_tree_event(event, author_keys.as_ref()) else {
            return;
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
            info!(
                "New tree update: {} -> {}",
                key,
                to_hex(&cid.hash)[..12].to_string()
            );

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

#[cfg(test)]
mod tests {
    use super::*;
    use git_remote_htree::nostr_client::KIND_HASHTREE_ROOT;
    use nostr_sdk::Keys;
    use std::fs;
    use tempfile::TempDir;

    fn upload_repo_root(
        store: &HashtreeStore,
        base: &std::path::Path,
        name: &str,
        body: &str,
    ) -> Cid {
        let dir = base.join(name);
        fs::create_dir_all(&dir).expect("create repo dir");
        fs::write(dir.join("README.md"), body).expect("write repo file");
        let cid = store
            .upload_dir_with_options(&dir, true)
            .expect("upload repo directory");
        let cid = Cid::parse(&cid).expect("parse repo cid");
        store.unpin(&cid.hash).expect("clear upload auto-pin");
        cid
    }

    #[test]
    fn classify_sync_event_ignores_removed_pinned_refs() {
        let keys = Keys::generate();
        let author = Keys::generate().public_key();
        let key = format!("{}/repo", author.to_bech32().expect("author npub"));

        let priority = classify_sync_event(
            &key,
            &author.to_hex(),
            &keys.public_key(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
        );

        assert_eq!(priority, None);
    }

    #[test]
    fn classify_sync_event_prioritizes_tracked_authors() {
        let keys = Keys::generate();
        let author = Keys::generate().public_key();
        let key = format!("{}/repo", author.to_bech32().expect("author npub"));

        let mut tracked_authors = HashSet::new();
        tracked_authors.insert(author.to_hex());

        let priority = classify_sync_event(
            &key,
            &author.to_hex(),
            &keys.public_key(),
            &HashSet::new(),
            &tracked_authors,
            &HashSet::new(),
        );

        assert_eq!(priority, Some(SyncPriority::TrackedAuthor));
    }

    #[test]
    fn tracked_author_private_event_uses_matching_local_key() {
        let author = Keys::generate();
        let root_hash = [0x11; 32];
        let root_key = [0x22; 32];
        let ciphertext = nip44::encrypt(
            author.secret_key(),
            &author.public_key(),
            hex::encode(root_key),
            nip44::Version::V2,
        )
        .expect("encrypt private root key");
        let event = EventBuilder::new(Kind::Custom(KIND_HASHTREE_ROOT), "")
            .tags(vec![
                Tag::identifier("backup".to_string()),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    vec!["hashtree"],
                ),
                Tag::custom(TagKind::Custom("hash".into()), vec![hex::encode(root_hash)]),
                Tag::custom(TagKind::Custom("selfEncryptedKey".into()), vec![ciphertext]),
            ])
            .sign_with_keys(&author)
            .expect("sign private root event");

        let cid = cid_from_tree_event(&event, Some(&author)).expect("decrypt tracked private cid");

        assert_eq!(cid.hash, root_hash);
        assert_eq!(cid.key, Some(root_key));
    }

    #[test]
    fn pinned_sync_update_replaces_old_root_pin() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::new(temp_dir.path().join("store")).expect("store");
        let first_cid = upload_repo_root(&store, temp_dir.path(), "repo-v1", "version one\n");
        let second_cid = upload_repo_root(&store, temp_dir.path(), "repo-v2", "version two\n");
        let repo_key = format!(
            "{}/repo",
            Keys::generate()
                .public_key()
                .to_bech32()
                .expect("repo owner npub")
        );

        let first_task = SyncTask {
            key: repo_key.clone(),
            cid: first_cid.clone(),
            priority: SyncPriority::Pinned,
            queued_at: Instant::now(),
        };
        apply_synced_tree_update(&store, &first_task).expect("apply first sync update");

        assert!(store.is_pinned(&first_cid.hash).expect("first root pinned"));
        assert_eq!(
            store.get_tree_ref(&repo_key).expect("first tree ref"),
            Some(first_cid.hash)
        );

        let second_task = SyncTask {
            key: repo_key.clone(),
            cid: second_cid.clone(),
            priority: SyncPriority::Pinned,
            queued_at: Instant::now(),
        };
        apply_synced_tree_update(&store, &second_task).expect("apply second sync update");

        assert!(
            !store
                .is_pinned(&first_cid.hash)
                .expect("first root pin status"),
            "updating a pinned ref should unpin the superseded root"
        );
        assert!(store
            .is_pinned(&second_cid.hash)
            .expect("second root pinned"));
        assert_eq!(
            store.get_tree_ref(&repo_key).expect("updated tree ref"),
            Some(second_cid.hash)
        );
        assert!(
            store
                .get_tree_meta(&first_cid.hash)
                .expect("first meta lookup")
                .is_none(),
            "superseded pinned root should be unindexed after update"
        );
    }

    #[test]
    fn tracked_author_sync_update_replaces_old_root_pin() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::new(temp_dir.path().join("store")).expect("store");
        let first_cid = upload_repo_root(&store, temp_dir.path(), "repo-v1", "version one\n");
        let second_cid = upload_repo_root(&store, temp_dir.path(), "repo-v2", "version two\n");
        let repo_key = format!(
            "{}/repo",
            Keys::generate()
                .public_key()
                .to_bech32()
                .expect("repo owner npub")
        );

        let first_task = SyncTask {
            key: repo_key.clone(),
            cid: first_cid.clone(),
            priority: SyncPriority::TrackedAuthor,
            queued_at: Instant::now(),
        };
        apply_synced_tree_update(&store, &first_task).expect("apply first tracked sync update");

        assert!(store.is_pinned(&first_cid.hash).expect("first root pinned"));
        assert_eq!(
            store.get_tree_ref(&repo_key).expect("first tree ref"),
            Some(first_cid.hash)
        );

        let second_task = SyncTask {
            key: repo_key.clone(),
            cid: second_cid.clone(),
            priority: SyncPriority::TrackedAuthor,
            queued_at: Instant::now(),
        };
        apply_synced_tree_update(&store, &second_task).expect("apply second tracked sync update");

        assert!(
            !store
                .is_pinned(&first_cid.hash)
                .expect("first root pin status"),
            "updating a tracked author ref should unpin the superseded root"
        );
        assert!(store
            .is_pinned(&second_cid.hash)
            .expect("second root pinned"));
        assert_eq!(
            store.get_tree_ref(&repo_key).expect("updated tree ref"),
            Some(second_cid.hash)
        );
    }
}
