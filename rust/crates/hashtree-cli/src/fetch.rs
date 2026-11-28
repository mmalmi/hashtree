//! Remote content fetching through configured Blossom read servers.
//!
//! Provides shared logic for fetching content from:
//! 1. Local storage, then the configured S3 archive
//! 2. Blossom HTTP servers (using `POST /blob/batch` when supported, with hedged parallel GET fallback)

use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use hashtree_blossom::{BlossomClient, BlossomError};
use hashtree_config::detect_local_daemon_url;
use hashtree_core::{
    decode_tree_node_by_cid, encrypted_size, encrypted_size_chk, to_hex, Cid, EncryptionMode, Link,
    LinkType, ObjectKey,
};
use nostr::Keys;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tracing::debug;

use crate::config::Config as CliConfig;
use crate::storage::HashtreeStore;

const DEFAULT_FETCH_CONCURRENCY: usize = 16;
const FETCH_BATCH_MAX_HASHES: usize = 64;
const FETCH_DEFAULT_ITEM_ESTIMATE: u64 = 64 * 1024;
const FETCH_BATCH_MAX_BYTES: u64 = 8 * 1024 * 1024;
const FETCH_BATCH_SINGLE_ITEM_MAX_BYTES: u64 = 512 * 1024;
const FETCH_MAX_CONCURRENT_BATCHES: usize = 4;
const FETCH_WRITE_BATCH_ITEMS: usize = 128;
const FETCH_WRITE_BATCH_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn child_cid(parent: &Cid, link: &Link) -> Cid {
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

fn is_literal_file_chunk(cid: &Cid, file_chunk_size: Option<u64>, stored_len: u64) -> bool {
    let overhead = match cid.key.map(|key| key.mode()) {
        None => 0,
        Some(EncryptionMode::LegacyChk | EncryptionMode::ChkV1) => encrypted_size_chk(0) as u64,
        Some(EncryptionMode::RndV1) => encrypted_size(0) as u64,
    };
    file_chunk_size.is_some_and(|size| size.checked_add(overhead) == Some(stored_len))
}

#[derive(Clone)]
struct FetchItem {
    cid: Cid,
    // Only a Blob link inside a File can identify literal chunks by length.
    file_chunk_size: Option<u64>,
    estimated_bytes: u64,
}

type Visited = HashSet<([u8; 32], Option<ObjectKey>, Option<u64>)>;

enum FetchOutcome {
    Single(FetchItem, Result<Vec<u8>, BlossomError>),
    Batch(
        Vec<FetchItem>,
        Result<Option<Vec<([u8; 32], Vec<u8>)>>, BlossomError>,
    ),
}

type FetchFuture = std::pin::Pin<Box<dyn std::future::Future<Output = FetchOutcome> + Send>>;

#[derive(Default)]
struct FetchWriteBuffer {
    entries: Vec<([u8; 32], Vec<u8>)>,
    index: HashMap<[u8; 32], usize>,
    bytes: usize,
}

impl FetchWriteBuffer {
    fn get(&self, hash: &[u8; 32]) -> Option<&[u8]> {
        self.index
            .get(hash)
            .map(|&idx| self.entries[idx].1.as_slice())
    }

    fn insert(&mut self, hash: [u8; 32], data: Vec<u8>) {
        if self.index.contains_key(&hash) {
            return;
        }
        self.bytes += data.len();
        self.index.insert(hash, self.entries.len());
        self.entries.push((hash, data));
    }

    fn flush(&mut self, store: &HashtreeStore) -> Result<()> {
        if self.entries.is_empty() {
            return Ok(());
        }
        store.put_cached_blobs(&self.entries)?;
        self.entries.clear();
        self.index.clear();
        self.bytes = 0;
        Ok(())
    }

    fn flush_if_needed(&mut self, store: &HashtreeStore) -> Result<()> {
        if self.entries.len() >= FETCH_WRITE_BATCH_ITEMS || self.bytes >= FETCH_WRITE_BATCH_BYTES {
            self.flush(store)?;
        }
        Ok(())
    }
}

fn enqueue_children(
    item: &FetchItem,
    data: Cow<'_, [u8]>,
    visited: &mut Visited,
    pending: &mut VecDeque<FetchItem>,
) -> Result<()> {
    if is_literal_file_chunk(&item.cid, item.file_chunk_size, data.len() as u64) {
        return Ok(());
    }
    if let Some(node) = decode_tree_node_by_cid(&item.cid, data.into_owned())
        .map_err(|err| anyhow::anyhow!("Failed to decode {}: {err}", to_hex(&item.cid.hash)))?
    {
        if item.file_chunk_size.is_some() && node.node_type != LinkType::File {
            // A legacy Blob child can extend a file only with another File node.
            return Ok(());
        }
        for link in node.links {
            let cid = child_cid(&item.cid, &link);
            let blob_size = (link.link_type == LinkType::Blob).then_some(link.size);
            let file_chunk_size = if node.node_type == LinkType::File {
                blob_size
            } else {
                None
            };
            if visited.insert((cid.hash, cid.key, file_chunk_size)) {
                pending.push_back(FetchItem {
                    cid,
                    file_chunk_size,
                    estimated_bytes: blob_size.unwrap_or(FETCH_DEFAULT_ITEM_ESTIMATE),
                });
            }
        }
    }
    Ok(())
}

fn spawn_single_fetch(
    active: &mut FuturesUnordered<FetchFuture>,
    blossom: &BlossomClient,
    item: FetchItem,
) {
    let blossom = blossom.clone();
    active.push(Box::pin(async move {
        let hash_hex = to_hex(&item.cid.hash);
        let res = blossom.download(&hash_hex).await;
        FetchOutcome::Single(item, res)
    }));
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
    /// Timeout for Blossom requests
    pub blossom_timeout: Duration,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            blossom_timeout: Duration::from_millis(10000),
        }
    }
}

/// Fetcher for remote content
pub struct Fetcher {
    blossom: BlossomClient,
}

impl Fetcher {
    /// Create a new fetcher with the given config.
    /// BlossomClient auto-loads servers from ~/.hashtree/config.toml.
    pub fn new(config: FetchConfig) -> Self {
        let keys = Keys::generate();
        let blossom = BlossomClient::new(keys).with_timeout(config.blossom_timeout);
        Self {
            blossom: with_local_daemon_read(blossom),
        }
    }

    /// Create a new fetcher with specific keys (for authenticated uploads).
    pub fn with_keys(config: FetchConfig, keys: Keys) -> Self {
        let blossom = BlossomClient::new(keys).with_timeout(config.blossom_timeout);
        Self {
            blossom: with_local_daemon_read(blossom),
        }
    }

    /// Create a fetcher with an explicit BlossomClient.
    pub fn with_client(blossom: BlossomClient) -> Self {
        Self { blossom }
    }

    /// Get the underlying BlossomClient
    pub fn blossom(&self) -> &BlossomClient {
        &self.blossom
    }

    /// Fetch a single chunk by hash.
    pub async fn fetch_chunk(&self, hash_hex: &str) -> Result<Vec<u8>> {
        let short_hash = &hash_hex[..12.min(hash_hex.len())];
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

    /// Fetch a chunk, checking local storage first.
    pub async fn fetch_chunk_with_store(
        &self,
        store: &HashtreeStore,
        hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        if let Some(data) = store.get_chunk(hash)? {
            return Ok(data);
        }

        let hash_hex = to_hex(hash);
        let data = self.fetch_chunk(&hash_hex).await?;
        store.put_cached_blob(&data)?;
        Ok(data)
    }

    /// Fetch an entire tree (all chunks recursively).
    /// Returns (chunks_fetched, bytes_fetched).
    pub async fn fetch_tree(
        &self,
        store: &HashtreeStore,
        root_hash: &[u8; 32],
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree(store, &Cid::public(*root_hash)).await
    }

    /// Fetch an entire tree from a CID, preserving decryption keys for encrypted trees.
    pub async fn fetch_cid_tree(
        &self,
        store: &HashtreeStore,
        root_cid: &Cid,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_with_progress(store, root_cid, None)
            .await
    }

    /// Fetch an entire tree from a CID, preserving decryption keys, with optional progress updates.
    pub async fn fetch_cid_tree_with_progress(
        &self,
        store: &HashtreeStore,
        root_cid: &Cid,
        progress: Option<&FetchProgress>,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_parallel_with_progress(
            store,
            root_cid,
            DEFAULT_FETCH_CONCURRENCY,
            progress,
        )
        .await
    }

    /// Fetch an entire tree with parallel downloads.
    /// Returns (chunks_fetched, bytes_fetched).
    pub async fn fetch_tree_parallel(
        &self,
        store: &HashtreeStore,
        root_hash: &[u8; 32],
        concurrency: usize,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_parallel(store, &Cid::public(*root_hash), concurrency)
            .await
    }

    /// Fetch an entire tree with parallel downloads, preserving decryption keys.
    pub async fn fetch_cid_tree_parallel(
        &self,
        store: &HashtreeStore,
        root_cid: &Cid,
        concurrency: usize,
    ) -> Result<(usize, u64)> {
        self.fetch_cid_tree_parallel_with_progress(store, root_cid, concurrency, None)
            .await
    }

    /// Fetch an entire tree with parallel & batched downloads, preserving decryption keys.
    ///
    /// Batches local cache writes into fewer LMDB transactions, decodes tree
    /// nodes in memory without rereading leaf blobs, and fails explicitly if
    /// any required chunk cannot be fetched.
    pub async fn fetch_cid_tree_parallel_with_progress(
        &self,
        store: &HashtreeStore,
        root_cid: &Cid,
        concurrency: usize,
        progress: Option<&FetchProgress>,
    ) -> Result<(usize, u64)> {
        let concurrency = concurrency.max(1);
        let mut chunks_fetched = 0usize;
        let mut bytes_fetched = 0u64;

        let mut visited = Visited::new();
        let mut pending: VecDeque<FetchItem> = VecDeque::new();
        let mut remote_queue: VecDeque<FetchItem> = VecDeque::new();
        let mut single_only: VecDeque<FetchItem> = VecDeque::new();
        let mut in_flight: HashSet<[u8; 32]> = HashSet::new();
        let mut waiting_same_hash: HashMap<[u8; 32], Vec<FetchItem>> = HashMap::new();
        let mut writes = FetchWriteBuffer::default();
        let mut failed: Vec<(String, String)> = Vec::new();

        let mut batch_supported = concurrency > 1 && !self.blossom.read_servers().is_empty();
        let mut active_batches = 0usize;
        let mut active: FuturesUnordered<FetchFuture> = FuturesUnordered::new();

        visited.insert((root_cid.hash, root_cid.key, None));
        pending.push_back(FetchItem {
            cid: root_cid.clone(),
            file_chunk_size: None,
            estimated_bytes: FETCH_DEFAULT_ITEM_ESTIMATE,
        });

        loop {
            // 1. Resolve locally present or already-buffered items first so
            //    their children become available for batching right away.
            while let Some(item) = pending.pop_front() {
                if let Some(data) = writes.get(&item.cid.hash) {
                    enqueue_children(&item, Cow::Borrowed(data), &mut visited, &mut pending)?;
                    continue;
                }
                // The routed read checks local member bytes and falls back to the
                // configured archive. A catalog size alone cannot establish either.
                if let Some(data) = store.router().get_sync(&item.cid.hash)? {
                    // A warm walk verifies literal chunks without scheduling an
                    // access-time write for each chunk of an already-local file.
                    if !is_literal_file_chunk(&item.cid, item.file_chunk_size, data.len() as u64) {
                        store.record_blob_accesses([item.cid.hash]);
                    }
                    enqueue_children(&item, Cow::Owned(data), &mut visited, &mut pending)?;
                    continue;
                }
                if in_flight.contains(&item.cid.hash) {
                    waiting_same_hash
                        .entry(item.cid.hash)
                        .or_default()
                        .push(item);
                    continue;
                }
                in_flight.insert(item.cid.hash);
                remote_queue.push_back(item);
            }

            // 2. Dispatch missing items as /blob/batch or hedged single GETs.
            while active.len() < concurrency {
                if let Some(item) = single_only.pop_front() {
                    spawn_single_fetch(&mut active, &self.blossom, item);
                    continue;
                }

                if batch_supported
                    && active_batches < FETCH_MAX_CONCURRENT_BATCHES
                    && remote_queue.len() >= 2
                {
                    let mut batch = Vec::new();
                    let mut est_bytes = 0u64;
                    while batch.len() < FETCH_BATCH_MAX_HASHES {
                        let Some(next) = remote_queue.front() else {
                            break;
                        };
                        let item_est = next.estimated_bytes;
                        if item_est > FETCH_BATCH_SINGLE_ITEM_MAX_BYTES
                            || (!batch.is_empty() && est_bytes + item_est > FETCH_BATCH_MAX_BYTES)
                        {
                            break;
                        }
                        est_bytes += item_est;
                        batch.push(remote_queue.pop_front().unwrap());
                    }
                    if batch.len() >= 2 {
                        let hashes: Vec<String> =
                            batch.iter().map(|item| to_hex(&item.cid.hash)).collect();
                        let blossom = self.blossom.clone();
                        active_batches += 1;
                        active.push(Box::pin(async move {
                            let res = blossom.download_batch(&hashes).await;
                            FetchOutcome::Batch(batch, res)
                        }));
                        continue;
                    } else if let Some(item) = batch.pop() {
                        remote_queue.push_front(item);
                    }
                }

                let Some(item) = remote_queue.pop_front() else {
                    break;
                };
                spawn_single_fetch(&mut active, &self.blossom, item);
            }

            // 3. Process the next completed network fetch.
            let Some(outcome) = active.next().await else {
                break;
            };
            let mut delivered = Vec::new();
            match outcome {
                FetchOutcome::Single(item, Ok(data)) => delivered.push((item, data)),
                FetchOutcome::Single(item, Err(err)) => {
                    in_flight.remove(&item.cid.hash);
                    waiting_same_hash.remove(&item.cid.hash);
                    let hash_hex = to_hex(&item.cid.hash);
                    debug!("Failed to fetch {}: {}", hash_hex, err);
                    failed.push((hash_hex, err.to_string()));
                }
                FetchOutcome::Batch(items, Ok(Some(returned))) => {
                    active_batches = active_batches.saturating_sub(1);
                    let mut returned_map: HashMap<[u8; 32], Vec<u8>> =
                        returned.into_iter().collect();
                    for item in items {
                        if let Some(data) = returned_map.remove(&item.cid.hash) {
                            delivered.push((item, data));
                        } else {
                            // Omitted from batch response (truncated or only on another server).
                            single_only.push_back(item);
                        }
                    }
                }
                FetchOutcome::Batch(items, Ok(None)) => {
                    active_batches = active_batches.saturating_sub(1);
                    batch_supported = false;
                    single_only.extend(items);
                }
                FetchOutcome::Batch(items, Err(err)) => {
                    active_batches = active_batches.saturating_sub(1);
                    debug!("Blossom batch fetch failed, falling back to single GETs: {err}");
                    single_only.extend(items);
                }
            }

            for (item, data) in delivered {
                in_flight.remove(&item.cid.hash);
                if let Some(progress) = progress {
                    progress.record_chunk(data.len());
                }
                chunks_fetched += 1;
                bytes_fetched += data.len() as u64;

                let mut decode_res =
                    enqueue_children(&item, Cow::Borrowed(&data), &mut visited, &mut pending);
                if let Some(extras) = waiting_same_hash.remove(&item.cid.hash) {
                    for extra in extras {
                        if decode_res.is_ok() {
                            decode_res = enqueue_children(
                                &extra,
                                Cow::Borrowed(&data),
                                &mut visited,
                                &mut pending,
                            );
                        }
                    }
                }
                writes.insert(item.cid.hash, data);
                if let Err(err) = decode_res {
                    let _ = writes.flush(store);
                    return Err(err);
                }
            }

            writes.flush_if_needed(store)?;
        }

        writes.flush(store)?;

        if !failed.is_empty() {
            let (first_hash, first_err) = &failed[0];
            let short = &first_hash[..12.min(first_hash.len())];
            anyhow::bail!(
                "Failed to fetch {} chunk(s) (first: {}: {})",
                failed.len(),
                short,
                first_err
            );
        }

        Ok((chunks_fetched, bytes_fetched))
    }

    /// Fetch a file by hash, fetching all chunks if needed.
    /// Returns the complete file content.
    pub async fn fetch_file(
        &self,
        store: &HashtreeStore,
        hash: &[u8; 32],
    ) -> Result<Option<Vec<u8>>> {
        if let Some(content) = store.get_file(hash)? {
            return Ok(Some(content));
        }
        self.fetch_tree(store, hash).await?;
        store.get_file(hash)
    }

    /// Fetch a directory listing, fetching chunks if needed.
    pub async fn fetch_directory(
        &self,
        store: &HashtreeStore,
        hash: &[u8; 32],
    ) -> Result<Option<crate::storage::DirectoryListing>> {
        if let Ok(Some(listing)) = store.get_directory_listing(hash) {
            return Ok(Some(listing));
        }
        self.fetch_tree(store, hash).await?;
        store.get_directory_listing(hash)
    }

    /// Upload data to Blossom servers.
    pub async fn upload(&self, data: &[u8]) -> Result<String> {
        self.blossom
            .upload(data)
            .await
            .map_err(|e| anyhow::anyhow!("Blossom upload failed: {}", e))
    }

    /// Upload data if it doesn't already exist.
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

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::{DirEntry, HashTree, HashTreeConfig};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn fetch_cid_tree_fails_loudly_on_missing_child_chunk() {
        let src_dir = tempdir().unwrap();
        let src = HashtreeStore::with_options(src_dir.path(), None, 32 * 1024 * 1024).unwrap();
        let tree = HashTree::new(HashTreeConfig::new(src.store_arc()));
        let (a_cid, a_size) = tree.put(b"present-file").await.unwrap();
        let (b_cid, b_size) = tree.put(b"missing-file").await.unwrap();
        let root = tree
            .put_directory(vec![
                DirEntry::from_cid("a.txt", &a_cid).with_size(a_size),
                DirEntry::from_cid("b.txt", &b_cid).with_size(b_size),
            ])
            .await
            .unwrap();

        let blobs = Arc::new(HashMap::from([
            (
                to_hex(&root.hash),
                src.get_blob(&root.hash).unwrap().unwrap(),
            ),
            (
                to_hex(&a_cid.hash),
                src.get_blob(&a_cid.hash).unwrap().unwrap(),
            ),
        ]));
        let serve_blobs = Arc::clone(&blobs);
        let app = axum::Router::new().route(
            "/:name",
            axum::routing::get(
                move |axum::extract::Path(name): axum::extract::Path<String>| {
                    let blobs = Arc::clone(&serve_blobs);
                    async move {
                        let key = name.trim_end_matches(".bin");
                        match blobs.get(key) {
                            Some(bytes) => (axum::http::StatusCode::OK, bytes.clone()),
                            None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dst_dir = tempdir().unwrap();
        let dst = HashtreeStore::with_options(dst_dir.path(), None, 32 * 1024 * 1024).unwrap();
        let client = BlossomClient::new_empty(Keys::generate())
            .with_read_servers(vec![format!("http://{addr}")]);
        let fetcher = Fetcher::with_client(client);

        let err = fetcher
            .fetch_cid_tree(&dst, &root)
            .await
            .expect_err("missing child chunk must fail instead of returning Ok");
        assert!(err.to_string().contains("Failed to fetch 1 chunk(s)"));
        assert!(
            dst.blob_exists(&a_cid.hash).unwrap(),
            "valid fetched chunks should be preserved in cache for retries"
        );

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn fetch_cid_tree_uses_blob_batch_and_recovers_encrypted_directory() {
        let src_dir = tempdir().unwrap();
        let src = HashtreeStore::with_options(src_dir.path(), None, 32 * 1024 * 1024).unwrap();
        let tree = HashTree::new(HashTreeConfig::new(src.store_arc()));
        let mut entries = Vec::new();
        let mut blobs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        for i in 0..8u8 {
            let payload = vec![i.wrapping_mul(17); 2048];
            let (cid, size) = tree.put(&payload).await.unwrap();
            blobs.insert(cid.hash, src.get_blob(&cid.hash).unwrap().unwrap());
            entries.push(DirEntry::from_cid(format!("f{i}.bin"), &cid).with_size(size));
        }
        let root = tree.put_directory(entries).await.unwrap();
        blobs.insert(root.hash, src.get_blob(&root.hash).unwrap().unwrap());

        let blobs = Arc::new(blobs);
        let batch_hits = Arc::new(AtomicUsize::new(0));
        let blobs_get = Arc::clone(&blobs);
        let blobs_batch = Arc::clone(&blobs);
        let batch_hits_route = Arc::clone(&batch_hits);

        let app = axum::Router::new()
            .route(
                "/blob/batch",
                axum::routing::post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let blobs = Arc::clone(&blobs_batch);
                    let batch_hits = Arc::clone(&batch_hits_route);
                    async move {
                        batch_hits.fetch_add(1, Ordering::SeqCst);
                        let hashes = req["hashes"].as_array().cloned().unwrap_or_default();
                        let mut found = Vec::new();
                        for h in hashes {
                            if let Some(hex_str) = h.as_str() {
                                if let Ok(raw) = hashtree_core::from_hex(hex_str) {
                                    if let Some(data) = blobs.get(&raw) {
                                        found.push((raw, data.clone()));
                                    }
                                }
                            }
                        }
                        let mut body = Vec::new();
                        body.extend_from_slice(hashtree_blossom::BLOB_BATCH_DOWNLOAD_MAGIC);
                        body.extend_from_slice(&(found.len() as u32).to_be_bytes());
                        for (h, d) in found {
                            body.extend_from_slice(&h);
                            body.extend_from_slice(&(d.len() as u64).to_be_bytes());
                            body.extend_from_slice(&d);
                        }
                        (axum::http::StatusCode::OK, body)
                    }
                }),
            )
            .route(
                "/:name",
                axum::routing::get(
                    move |axum::extract::Path(name): axum::extract::Path<String>| {
                        let blobs = Arc::clone(&blobs_get);
                        async move {
                            let key = name.trim_end_matches(".bin");
                            match hashtree_core::from_hex(key)
                                .ok()
                                .and_then(|h| blobs.get(&h))
                            {
                                Some(bytes) => (axum::http::StatusCode::OK, bytes.clone()),
                                None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                            }
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dst_dir = tempdir().unwrap();
        let dst = HashtreeStore::with_options(dst_dir.path(), None, 32 * 1024 * 1024).unwrap();
        let client = BlossomClient::new_empty(Keys::generate())
            .with_read_servers(vec![format!("http://{addr}")]);
        let fetcher = Fetcher::with_client(client);

        let (chunks, _) = fetcher.fetch_cid_tree(&dst, &root).await.expect("fetch ok");
        assert_eq!(chunks, 9);
        assert!(
            batch_hits.load(Ordering::SeqCst) >= 1,
            "expected /blob/batch usage"
        );

        let dst_tree = HashTree::new(HashTreeConfig::new(dst.store_arc()));
        for i in 0..8u8 {
            let child = dst_tree
                .resolve_path(&root, &format!("f{i}.bin"))
                .await
                .unwrap()
                .unwrap();
            let got = dst_tree.get(&child, None).await.unwrap().unwrap();
            assert_eq!(got, vec![i.wrapping_mul(17); 2048]);
        }

        server.abort();
        let _ = server.await;
    }
}

#[cfg(test)]
mod tree_tests;
