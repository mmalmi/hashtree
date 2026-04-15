//! Remote content fetching with WebRTC and Blossom fallback
//!
//! Provides shared logic for fetching content from:
//! 1. Local storage (first)
//! 2. WebRTC peers (second)
//! 3. Blossom HTTP servers (fallback)

use anyhow::Result;
use hashtree_blossom::BlossomClient;
use hashtree_config::detect_local_daemon_url;
use hashtree_core::{to_hex, Cid, HashTree, HashTreeConfig, Link};
use nostr::Keys;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

use crate::config::Config as CliConfig;
use crate::storage::HashtreeStore;
use crate::webrtc::WebRTCState;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

fn child_cid(parent: &Cid, link: &Link) -> Cid {
    let inherits_parent_key = link
        .name
        .as_deref()
        .map(|name| {
            name.starts_with("_chunk_")
                || (name.starts_with('_') && name.chars().count() == 2 && link.link_type.is_tree())
        })
        .unwrap_or(false);

    Cid {
        hash: link.hash,
        key: link.key.or(if inherits_parent_key {
            parent.key
        } else {
            None
        }),
    }
}

#[derive(Debug, Default)]
pub struct FetchProgress {
    chunks_fetched: AtomicUsize,
    bytes_fetched: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FetchProgressSnapshot {
    pub chunks_fetched: usize,
    pub bytes_fetched: u64,
}

impl FetchProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> FetchProgressSnapshot {
        FetchProgressSnapshot {
            chunks_fetched: self.chunks_fetched.load(Ordering::Relaxed),
            bytes_fetched: self.bytes_fetched.load(Ordering::Relaxed),
        }
    }

    fn record_chunk(&self, byte_len: usize) {
        self.chunks_fetched.fetch_add(1, Ordering::Relaxed);
        self.bytes_fetched
            .fetch_add(byte_len as u64, Ordering::Relaxed);
    }
}

/// Configuration for remote fetching
#[derive(Clone)]
pub struct FetchConfig {
    /// Timeout for WebRTC requests
    pub webrtc_timeout: Duration,
    /// Timeout for Blossom requests
    pub blossom_timeout: Duration,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            webrtc_timeout: Duration::from_millis(2000),
            blossom_timeout: Duration::from_millis(10000),
        }
    }
}

/// Fetcher for remote content
pub struct Fetcher {
    config: FetchConfig,
    blossom: BlossomClient,
}

impl Fetcher {
    /// Create a new fetcher with the given config
    /// BlossomClient auto-loads servers from ~/.hashtree/config.toml
    pub fn new(config: FetchConfig) -> Self {
        // Generate ephemeral keys for downloads (no signing needed)
        let keys = Keys::generate();
        let blossom = BlossomClient::new(keys).with_timeout(config.blossom_timeout);
        let blossom = with_local_daemon_read(blossom);

        Self { config, blossom }
    }

    /// Create a new fetcher with specific keys (for authenticated uploads)
    pub fn with_keys(config: FetchConfig, keys: Keys) -> Self {
        let blossom = BlossomClient::new(keys).with_timeout(config.blossom_timeout);
        let blossom = with_local_daemon_read(blossom);

        Self { config, blossom }
    }

    /// Get the underlying BlossomClient
    pub fn blossom(&self) -> &BlossomClient {
        &self.blossom
    }

    /// Fetch a single chunk by hash, trying WebRTC first then Blossom
    pub async fn fetch_chunk(
        &self,
        webrtc_state: Option<&Arc<WebRTCState>>,
        hash_hex: &str,
    ) -> Result<Vec<u8>> {
        let short_hash = if hash_hex.len() >= 12 {
            &hash_hex[..12]
        } else {
            hash_hex
        };

        // Try WebRTC first
        if let Some(state) = webrtc_state {
            debug!("Trying WebRTC for {}", short_hash);
            let webrtc_result = tokio::time::timeout(
                self.config.webrtc_timeout,
                state.request_from_peers(hash_hex),
            )
            .await;

            if let Ok(Some(data)) = webrtc_result {
                debug!("Got {} from WebRTC ({} bytes)", short_hash, data.len());
                return Ok(data);
            }
        }

        // Fallback to Blossom
        debug!("Trying Blossom for {}", short_hash);
        match self.blossom.download(hash_hex).await {
            Ok(data) => {
                debug!("Got {} from Blossom ({} bytes)", short_hash, data.len());
                Ok(data)
            }
            Err(e) => {
                debug!("Blossom download failed for {}: {}", short_hash, e);
                Err(anyhow::anyhow!(
                    "Failed to fetch {} from any source: {}",
                    short_hash,
                    e
                ))
            }
        }
    }

    /// Fetch a chunk, checking local storage first
    pub async fn fetch_chunk_with_store(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        // Check local storage first
        if let Some(data) = store.get_chunk(hash)? {
            return Ok(data);
        }

        // Fetch remotely and store
        let hash_hex = to_hex(hash);
        let data = self.fetch_chunk(webrtc_state, &hash_hex).await?;
        store.put_cached_blob(&data)?;
        Ok(data)
    }

    /// Fetch an entire tree (all chunks recursively) - sequential version
    /// Returns (chunks_fetched, bytes_fetched)
    pub async fn fetch_tree(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        root_hash: &[u8; 32],
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree(store, webrtc_state, &Cid::public(*root_hash))
            .await
    }

    /// Fetch an entire tree from a CID, preserving decryption keys for encrypted trees.
    pub async fn fetch_cid_tree(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        root_cid: &Cid,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_with_progress(store, webrtc_state, root_cid, None)
            .await
    }

    /// Fetch an entire tree from a CID, preserving decryption keys, with optional progress updates.
    pub async fn fetch_cid_tree_with_progress(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        root_cid: &Cid,
        progress: Option<&FetchProgress>,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_parallel_with_progress(store, webrtc_state, root_cid, 1, progress)
            .await
    }

    /// Fetch an entire tree with parallel downloads
    /// Uses work-stealing: always keeps `concurrency` requests in flight
    /// Returns (chunks_fetched, bytes_fetched)
    pub async fn fetch_tree_parallel(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        root_hash: &[u8; 32],
        concurrency: usize,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_parallel(store, webrtc_state, &Cid::public(*root_hash), concurrency)
            .await
    }

    /// Fetch an entire tree with parallel downloads, preserving decryption keys.
    pub async fn fetch_cid_tree_parallel(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        root_cid: &Cid,
        concurrency: usize,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_parallel_with_progress(store, webrtc_state, root_cid, concurrency, None)
            .await
    }

    /// Fetch an entire tree with parallel downloads, preserving decryption keys, with optional progress updates.
    pub async fn fetch_cid_tree_parallel_with_progress(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        root_cid: &Cid,
        concurrency: usize,
        progress: Option<&FetchProgress>,
    ) -> Result<(usize, u64)> {
        use futures::stream::{FuturesUnordered, StreamExt};

        let chunks_fetched = Arc::new(AtomicUsize::new(0));
        let bytes_fetched = Arc::new(AtomicU64::new(0));
        let mut queued: HashSet<[u8; 32]> = HashSet::new();
        let mut pending: VecDeque<Cid> = VecDeque::new();

        pending.push_back(root_cid.clone());
        queued.insert(root_cid.hash);

        let mut active = FuturesUnordered::new();
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        loop {
            // Fill up to concurrency limit from pending queue
            while active.len() < concurrency {
                if let Some(cid) = pending.pop_front() {
                    if store.blob_exists(&cid.hash).unwrap_or(false) {
                        if let Some(node) = tree.get_node(&cid).await? {
                            for link in node.links {
                                let child = child_cid(&cid, &link);
                                if queued.insert(child.hash) {
                                    pending.push_back(child);
                                }
                            }
                        }
                        continue;
                    }

                    let hash_hex = to_hex(&cid.hash);
                    let blossom = self.blossom.clone();
                    let webrtc = webrtc_state.map(Arc::clone);
                    let timeout = self.config.webrtc_timeout;

                    let fut = async move {
                        // Try WebRTC first
                        if let Some(state) = &webrtc {
                            if let Ok(Some(data)) =
                                tokio::time::timeout(timeout, state.request_from_peers(&hash_hex))
                                    .await
                            {
                                return (cid, Ok(data));
                            }
                        }
                        // Fallback to Blossom
                        let data = blossom.download(&hash_hex).await;
                        (cid, data)
                    };
                    active.push(fut);
                } else {
                    break;
                }
            }

            // If nothing active, we're done
            if active.is_empty() {
                break;
            }

            // Wait for any download to complete
            if let Some((cid, result)) = active.next().await {
                match result {
                    Ok(data) => {
                        // Store it
                        store.put_cached_blob(&data)?;
                        if let Some(progress) = progress {
                            progress.record_chunk(data.len());
                        }
                        chunks_fetched.fetch_add(1, Ordering::Relaxed);
                        bytes_fetched.fetch_add(data.len() as u64, Ordering::Relaxed);

                        if let Some(node) = tree.get_node(&cid).await? {
                            for link in node.links {
                                let child = child_cid(&cid, &link);
                                if queued.insert(child.hash) {
                                    pending.push_back(child);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!("Failed to fetch {}: {}", to_hex(&cid.hash), e);
                        // Continue with other chunks - don't fail the whole tree
                    }
                }
            }
        }

        Ok((
            chunks_fetched.load(Ordering::Relaxed),
            bytes_fetched.load(Ordering::Relaxed),
        ))
    }

    /// Fetch a file by hash, fetching all chunks if needed
    /// Returns the complete file content
    pub async fn fetch_file(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        hash: &[u8; 32],
    ) -> Result<Option<Vec<u8>>> {
        // First, try to get from local storage
        if let Some(content) = store.get_file(hash)? {
            return Ok(Some(content));
        }

        // Fetch the tree
        self.fetch_tree(store, webrtc_state, hash).await?;

        // Now try to read the file
        store.get_file(hash)
    }

    /// Fetch a directory listing, fetching chunks if needed
    pub async fn fetch_directory(
        &self,
        store: &HashtreeStore,
        webrtc_state: Option<&Arc<WebRTCState>>,
        hash: &[u8; 32],
    ) -> Result<Option<crate::storage::DirectoryListing>> {
        // First, try to get from local storage
        if let Ok(Some(listing)) = store.get_directory_listing(hash) {
            return Ok(Some(listing));
        }

        // Fetch the tree
        self.fetch_tree(store, webrtc_state, hash).await?;

        // Now try to get the directory listing
        store.get_directory_listing(hash)
    }

    /// Upload data to Blossom servers
    pub async fn upload(&self, data: &[u8]) -> Result<String> {
        self.blossom
            .upload(data)
            .await
            .map_err(|e| anyhow::anyhow!("Blossom upload failed: {}", e))
    }

    /// Upload data if it doesn't already exist
    pub async fn upload_if_missing(&self, data: &[u8]) -> Result<(String, bool)> {
        self.blossom
            .upload_if_missing(data)
            .await
            .map_err(|e| anyhow::anyhow!("Blossom upload failed: {}", e))
    }
}

fn with_local_daemon_read(blossom: BlossomClient) -> BlossomClient {
    let bind_address = CliConfig::load().ok().map(|cfg| cfg.server.bind_address);
    let local_url = detect_local_daemon_url(bind_address.as_deref());
    let Some(local_url) = local_url else {
        return blossom;
    };

    let mut servers = blossom.read_servers().to_vec();
    if servers.iter().any(|server| server == &local_url) {
        return blossom;
    }
    servers.insert(0, local_url);
    blossom.with_read_servers(servers)
}
