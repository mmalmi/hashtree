use anyhow::{Context, Result};
use async_trait::async_trait;
use heed::{Database, EnvOpenOptions};
use heed::types::*;
use hashtree_fs::FsBlobStore;
#[cfg(feature = "lmdb")]
use hashtree_lmdb::LmdbBlobStore;
use hashtree_core::{
    HashTree, HashTreeConfig, Cid,
    sha256, to_hex, from_hex, TreeNode, DirEntry as HashTreeDirEntry,
    types::Hash,
};
use hashtree_core::store::{Store, StoreError};
use hashtree_config::StorageBackend;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use futures::executor::block_on as sync_block_on;

/// Priority levels for tree eviction
pub const PRIORITY_OTHER: u8 = 64;
pub const PRIORITY_FOLLOWED: u8 = 128;
pub const PRIORITY_OWN: u8 = 255;

/// Metadata for a synced tree (for eviction tracking)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeMeta {
    /// Pubkey of tree owner
    pub owner: String,
    /// Tree name if known (from nostr key like "npub.../name")
    pub name: Option<String>,
    /// Unix timestamp when this tree was synced
    pub synced_at: u64,
    /// Total size of all blobs in this tree
    pub total_size: u64,
    /// Eviction priority: 255=own/pinned, 128=followed, 64=other
    pub priority: u8,
}

/// Cached root info from Nostr events (replaces nostrdb caching)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRoot {
    /// Root hash (hex)
    pub hash: String,
    /// Optional decryption key (hex)
    pub key: Option<String>,
    /// Unix timestamp when this was cached (from event created_at)
    pub updated_at: u64,
    /// Visibility: "public" or "private"
    pub visibility: String,
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct LocalStoreStats {
    pub count: usize,
    pub total_bytes: u64,
}

/// Local blob store - wraps either FsBlobStore or LmdbBlobStore
pub enum LocalStore {
    Fs(FsBlobStore),
    #[cfg(feature = "lmdb")]
    Lmdb(LmdbBlobStore),
}

impl LocalStore {
    /// Create a new local store based on config
    pub fn new<P: AsRef<Path>>(path: P, backend: &StorageBackend) -> Result<Self, StoreError> {
        match backend {
            StorageBackend::Fs => {
                Ok(LocalStore::Fs(FsBlobStore::new(path)?))
            }
            #[cfg(feature = "lmdb")]
            StorageBackend::Lmdb => {
                Ok(LocalStore::Lmdb(LmdbBlobStore::new(path)?))
            }
            #[cfg(not(feature = "lmdb"))]
            StorageBackend::Lmdb => {
                tracing::warn!("LMDB backend requested but lmdb feature not enabled, using filesystem storage");
                Ok(LocalStore::Fs(FsBlobStore::new(path)?))
            }
        }
    }

    /// Sync put operation
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.put_sync(hash, data),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.put_sync(hash, data),
        }
    }

    /// Sync get operation
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get_sync(hash),
        }
    }

    /// Check if hash exists
    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => Ok(store.exists(hash)),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.exists(hash),
        }
    }

    /// Sync delete operation
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete_sync(hash),
        }
    }

    /// Get storage statistics
    pub fn stats(&self) -> Result<LocalStoreStats, StoreError> {
        match self {
            LocalStore::Fs(store) => {
                let stats = store.stats()?;
                Ok(LocalStoreStats {
                    count: stats.count,
                    total_bytes: stats.total_bytes,
                })
            }
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => {
                let stats = store.stats()?;
                Ok(LocalStoreStats {
                    count: stats.count,
                    total_bytes: stats.total_bytes,
                })
            }
        }
    }

    /// List all hashes in the store
    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.list(),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.list(),
        }
    }
}

#[async_trait]
impl Store for LocalStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.delete_sync(hash)
    }
}

#[cfg(feature = "s3")]
use tokio::sync::mpsc;

use crate::config::S3Config;

/// Message for background S3 sync
#[cfg(feature = "s3")]
enum S3SyncMessage {
    Upload { hash: Hash, data: Vec<u8> },
    Delete { hash: Hash },
}

/// Storage router - local store primary with optional S3 backup
///
/// Write path: local first (fast), then queue S3 upload (non-blocking)
/// Read path: local first, fall back to S3 if miss
pub struct StorageRouter {
    /// Primary local store (always used)
    local: Arc<LocalStore>,
    /// Optional S3 client for backup
    #[cfg(feature = "s3")]
    s3_client: Option<aws_sdk_s3::Client>,
    #[cfg(feature = "s3")]
    s3_bucket: Option<String>,
    #[cfg(feature = "s3")]
    s3_prefix: String,
    /// Channel to send uploads to background task
    #[cfg(feature = "s3")]
    sync_tx: Option<mpsc::UnboundedSender<S3SyncMessage>>,
}

impl StorageRouter {
    /// Create router with local storage only
    pub fn new(local: Arc<LocalStore>) -> Self {
        Self {
            local,
            #[cfg(feature = "s3")]
            s3_client: None,
            #[cfg(feature = "s3")]
            s3_bucket: None,
            #[cfg(feature = "s3")]
            s3_prefix: String::new(),
            #[cfg(feature = "s3")]
            sync_tx: None,
        }
    }

    /// Create router with local storage + S3 backup
    #[cfg(feature = "s3")]
    pub async fn with_s3(local: Arc<LocalStore>, config: &S3Config) -> Result<Self, anyhow::Error> {
        use aws_sdk_s3::Client as S3Client;

        // Build AWS config
        let mut aws_config_loader = aws_config::from_env();
        aws_config_loader = aws_config_loader.region(aws_sdk_s3::config::Region::new(config.region.clone()));
        let aws_config = aws_config_loader.load().await;

        // Build S3 client with custom endpoint
        let mut s3_config_builder = aws_sdk_s3::config::Builder::from(&aws_config);
        s3_config_builder = s3_config_builder
            .endpoint_url(&config.endpoint)
            .force_path_style(true);

        let s3_client = S3Client::from_conf(s3_config_builder.build());
        let bucket = config.bucket.clone();
        let prefix = config.prefix.clone().unwrap_or_default();

        // Create background sync channel
        let (sync_tx, mut sync_rx) = mpsc::unbounded_channel::<S3SyncMessage>();

        // Spawn background sync task with bounded concurrent uploads
        let sync_client = s3_client.clone();
        let sync_bucket = bucket.clone();
        let sync_prefix = prefix.clone();

        tokio::spawn(async move {
            use aws_sdk_s3::primitives::ByteStream;

            tracing::info!("S3 background sync task started");

            // Limit concurrent uploads to prevent overwhelming the runtime
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
            let client = std::sync::Arc::new(sync_client);
            let bucket = std::sync::Arc::new(sync_bucket);
            let prefix = std::sync::Arc::new(sync_prefix);

            while let Some(msg) = sync_rx.recv().await {
                let client = client.clone();
                let bucket = bucket.clone();
                let prefix = prefix.clone();
                let semaphore = semaphore.clone();

                // Spawn each upload with semaphore-bounded concurrency
                tokio::spawn(async move {
                    // Acquire permit before uploading
                    let _permit = semaphore.acquire().await;

                    match msg {
                        S3SyncMessage::Upload { hash, data } => {
                            let key = format!("{}{}.bin", prefix, to_hex(&hash));
                            tracing::debug!("S3 uploading {} ({} bytes)", &key, data.len());

                            match client
                                .put_object()
                                .bucket(bucket.as_str())
                                .key(&key)
                                .body(ByteStream::from(data))
                                .send()
                                .await
                            {
                                Ok(_) => tracing::debug!("S3 upload succeeded: {}", &key),
                                Err(e) => tracing::error!("S3 upload failed {}: {}", &key, e),
                            }
                        }
                        S3SyncMessage::Delete { hash } => {
                            let key = format!("{}{}.bin", prefix, to_hex(&hash));
                            tracing::debug!("S3 deleting {}", &key);

                            if let Err(e) = client
                                .delete_object()
                                .bucket(bucket.as_str())
                                .key(&key)
                                .send()
                                .await
                            {
                                tracing::error!("S3 delete failed {}: {}", &key, e);
                            }
                        }
                    }
                });
            }
        });

        tracing::info!("S3 storage initialized: bucket={}, prefix={}", bucket, prefix);

        Ok(Self {
            local,
            s3_client: Some(s3_client),
            s3_bucket: Some(bucket),
            s3_prefix: prefix,
            sync_tx: Some(sync_tx),
        })
    }

    /// Store data - writes to LMDB, queues S3 upload in background
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        // Always write to local first
        let is_new = self.local.put_sync(hash, data)?;

        // Queue S3 upload if configured (non-blocking)
        // Always upload to S3 (even if not new locally) to ensure S3 has all blobs
        #[cfg(feature = "s3")]
        if let Some(ref tx) = self.sync_tx {
            tracing::info!("Queueing S3 upload for {} ({} bytes, is_new={})",
                crate::storage::to_hex(&hash)[..16].to_string(), data.len(), is_new);
            if let Err(e) = tx.send(S3SyncMessage::Upload { hash, data: data.to_vec() }) {
                tracing::error!("Failed to queue S3 upload: {}", e);
            }
        }

        Ok(is_new)
    }

    /// Get data - tries LMDB first, falls back to S3
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        // Try local first
        if let Some(data) = self.local.get_sync(hash)? {
            return Ok(Some(data));
        }

        // Fall back to S3 if configured
        #[cfg(feature = "s3")]
        if let (Some(ref client), Some(ref bucket)) = (&self.s3_client, &self.s3_bucket) {
            let key = format!("{}{}.bin", self.s3_prefix, to_hex(hash));

            match sync_block_on(async {
                client.get_object()
                    .bucket(bucket)
                    .key(&key)
                    .send()
                    .await
            }) {
                Ok(output) => {
                    if let Ok(body) = sync_block_on(output.body.collect()) {
                        let data = body.into_bytes().to_vec();
                        // Cache locally for future reads
                        let _ = self.local.put_sync(*hash, &data);
                        return Ok(Some(data));
                    }
                }
                Err(e) => {
                    let service_err = e.into_service_error();
                    if !service_err.is_no_such_key() {
                        tracing::warn!("S3 get failed: {}", service_err);
                    }
                }
            }
        }

        Ok(None)
    }

    /// Check if hash exists
    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        // Check local first
        if self.local.exists(hash)? {
            return Ok(true);
        }

        // Check S3 if configured
        #[cfg(feature = "s3")]
        if let (Some(ref client), Some(ref bucket)) = (&self.s3_client, &self.s3_bucket) {
            let key = format!("{}{}.bin", self.s3_prefix, to_hex(hash));

            match sync_block_on(async {
                client.head_object()
                    .bucket(bucket)
                    .key(&key)
                    .send()
                    .await
            }) {
                Ok(_) => return Ok(true),
                Err(e) => {
                    let service_err = e.into_service_error();
                    if !service_err.is_not_found() {
                        tracing::warn!("S3 head failed: {}", service_err);
                    }
                }
            }
        }

        Ok(false)
    }

    /// Delete data from both local and S3 stores
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        let deleted = self.local.delete_sync(hash)?;

        // Queue S3 delete if configured
        #[cfg(feature = "s3")]
        if let Some(ref tx) = self.sync_tx {
            let _ = tx.send(S3SyncMessage::Delete { hash: *hash });
        }

        Ok(deleted)
    }

    /// Delete data from local store only (don't propagate to S3)
    /// Used for eviction where we want to keep S3 as archive
    pub fn delete_local_only(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local.delete_sync(hash)
    }

    /// Get stats from local store
    pub fn stats(&self) -> Result<LocalStoreStats, StoreError> {
        self.local.stats()
    }

    /// List all hashes from local store
    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        self.local.list()
    }

    /// Get the underlying local store for HashTree operations
    pub fn local_store(&self) -> Arc<LocalStore> {
        Arc::clone(&self.local)
    }
}

// Implement async Store trait for StorageRouter so it can be used directly with HashTree
// This ensures all writes go through S3 sync
#[async_trait]
impl Store for StorageRouter {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.delete_sync(hash)
    }
}

pub struct HashtreeStore {
    env: heed::Env,
    /// Set of pinned hashes (32-byte raw hashes, prevents garbage collection)
    pins: Database<Bytes, Unit>,
    /// Blob ownership: sha256 (32 bytes) ++ pubkey (32 bytes) -> () (composite key for multi-owner)
    blob_owners: Database<Bytes, Unit>,
    /// Maps pubkey (32 bytes) -> blob metadata JSON (for blossom list)
    pubkey_blobs: Database<Bytes, Bytes>,
    /// Tree metadata for eviction: tree_root_hash (32 bytes) -> TreeMeta (msgpack)
    tree_meta: Database<Bytes, Bytes>,
    /// Blob-to-tree mapping: blob_hash ++ tree_hash (64 bytes) -> ()
    blob_trees: Database<Bytes, Unit>,
    /// Tree refs: "npub/path" -> tree_root_hash (32 bytes) - for replacing old versions
    tree_refs: Database<Str, Bytes>,
    /// Cached roots from Nostr: "pubkey_hex/tree_name" -> CachedRoot (msgpack)
    cached_roots: Database<Str, Bytes>,
    /// Storage router - handles LMDB + optional S3 (Arc for sharing with HashTree)
    router: Arc<StorageRouter>,
    /// Maximum storage size in bytes (from config)
    max_size_bytes: u64,
}

impl HashtreeStore {
    /// Create a new store with local LMDB storage only (10GB default limit)
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::with_options(path, None, 10 * 1024 * 1024 * 1024)
    }

    /// Create a new store with optional S3 backend (10GB default limit)
    pub fn with_s3<P: AsRef<Path>>(path: P, s3_config: Option<&S3Config>) -> Result<Self> {
        Self::with_options(path, s3_config, 10 * 1024 * 1024 * 1024)
    }

    /// Create a new store with optional S3 backend and custom size limit
    pub fn with_options<P: AsRef<Path>>(path: P, s3_config: Option<&S3Config>, max_size_bytes: u64) -> Result<Self> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;

        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(10 * 1024 * 1024 * 1024) // 10GB virtual address space
                .max_dbs(8)  // pins, blob_owners, pubkey_blobs, tree_meta, blob_trees, tree_refs, cached_roots, blobs
                .open(path)?
        };

        let mut wtxn = env.write_txn()?;
        let pins = env.create_database(&mut wtxn, Some("pins"))?;
        let blob_owners = env.create_database(&mut wtxn, Some("blob_owners"))?;
        let pubkey_blobs = env.create_database(&mut wtxn, Some("pubkey_blobs"))?;
        let tree_meta = env.create_database(&mut wtxn, Some("tree_meta"))?;
        let blob_trees = env.create_database(&mut wtxn, Some("blob_trees"))?;
        let tree_refs = env.create_database(&mut wtxn, Some("tree_refs"))?;
        let cached_roots = env.create_database(&mut wtxn, Some("cached_roots"))?;
        wtxn.commit()?;

        // Get storage backend from config
        let config = hashtree_config::Config::load_or_default();
        let backend = &config.storage.backend;

        // Create local blob store based on configured backend
        let local_store = Arc::new(LocalStore::new(path.join("blobs"), backend)
            .map_err(|e| anyhow::anyhow!("Failed to create blob store: {}", e))?);

        // Create storage router with optional S3
        #[cfg(feature = "s3")]
        let router = Arc::new(if let Some(s3_cfg) = s3_config {
            tracing::info!("Initializing S3 storage backend: bucket={}, endpoint={}",
                s3_cfg.bucket, s3_cfg.endpoint);

            sync_block_on(async {
                StorageRouter::with_s3(local_store, s3_cfg).await
            })?
        } else {
            StorageRouter::new(local_store)
        });

        #[cfg(not(feature = "s3"))]
        let router = Arc::new({
            if s3_config.is_some() {
                tracing::warn!("S3 config provided but S3 feature not enabled. Using local storage only.");
            }
            StorageRouter::new(local_store)
        });

        Ok(Self {
            env,
            pins,
            blob_owners,
            pubkey_blobs,
            tree_meta,
            blob_trees,
            tree_refs,
            cached_roots,
            router,
            max_size_bytes,
        })
    }

    /// Get the storage router
    pub fn router(&self) -> &StorageRouter {
        &self.router
    }

    /// Get the storage router as Arc (for use with HashTree which needs Arc<dyn Store>)
    /// All writes through this go to both LMDB and S3
    pub fn store_arc(&self) -> Arc<StorageRouter> {
        Arc::clone(&self.router)
    }

    /// Upload a file and return its CID (public/unencrypted), with auto-pin
    pub fn upload_file<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_internal(file_path, true)
    }

    /// Upload a file without pinning (for blossom uploads that can be evicted)
    pub fn upload_file_no_pin<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_internal(file_path, false)
    }

    fn upload_file_internal<P: AsRef<Path>>(&self, file_path: P, pin: bool) -> Result<String> {
        let file_path = file_path.as_ref();
        let file_content = std::fs::read(file_path)?;

        // Use hashtree to store the file (public mode - no encryption)
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let (cid, _size) = sync_block_on(async {
            tree.put(&file_content).await
        }).context("Failed to store file")?;

        // Only pin if requested (htree add = pin, blossom upload = no pin)
        if pin {
            let mut wtxn = self.env.write_txn()?;
            self.pins.put(&mut wtxn, cid.hash.as_slice(), &())?;
            wtxn.commit()?;
        }

        Ok(to_hex(&cid.hash))
    }

    /// Upload a file from a stream with progress callbacks
    pub fn upload_file_stream<R: Read, F>(
        &self,
        mut reader: R,
        _file_name: impl Into<String>,
        mut callback: F,
    ) -> Result<String>
    where
        F: FnMut(&str),
    {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        // Use HashTree.put for upload (public mode)
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let (cid, _size) = sync_block_on(async {
            tree.put(&data).await
        }).context("Failed to store file")?;

        let root_hex = to_hex(&cid.hash);
        callback(&root_hex);

        // Auto-pin on upload
        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(root_hex)
    }

    /// Upload a directory and return its root hash (hex)
    /// Respects .gitignore by default
    pub fn upload_dir<P: AsRef<Path>>(&self, dir_path: P) -> Result<String> {
        self.upload_dir_with_options(dir_path, true)
    }

    /// Upload a directory with options (public mode - no encryption)
    pub fn upload_dir_with_options<P: AsRef<Path>>(&self, dir_path: P, respect_gitignore: bool) -> Result<String> {
        let dir_path = dir_path.as_ref();

        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let root_cid = sync_block_on(async {
            self.upload_dir_recursive(&tree, dir_path, dir_path, respect_gitignore).await
        }).context("Failed to upload directory")?;

        let root_hex = to_hex(&root_cid.hash);

        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, root_cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(root_hex)
    }

    async fn upload_dir_recursive<S: Store>(
        &self,
        tree: &HashTree<S>,
        _root_path: &Path,
        current_path: &Path,
        respect_gitignore: bool,
    ) -> Result<Cid> {
        use ignore::WalkBuilder;
        use std::collections::HashMap;

        // Build directory structure from flat file list - store full Cid with key
        let mut dir_contents: HashMap<String, Vec<(String, Cid)>> = HashMap::new();
        dir_contents.insert(String::new(), Vec::new()); // Root

        let walker = WalkBuilder::new(current_path)
            .git_ignore(respect_gitignore)
            .git_global(respect_gitignore)
            .git_exclude(respect_gitignore)
            .hidden(false)
            .build();

        for result in walker {
            let entry = result?;
            let path = entry.path();

            // Skip the root directory itself
            if path == current_path {
                continue;
            }

            let relative = path.strip_prefix(current_path)
                .unwrap_or(path);

            if path.is_file() {
                let content = std::fs::read(path)?;
                let (cid, _size) = tree.put(&content).await
                    .map_err(|e| anyhow::anyhow!("Failed to upload file {}: {}", path.display(), e))?;

                // Get parent directory path and file name
                let parent = relative.parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let name = relative.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();

                dir_contents.entry(parent).or_default().push((name, cid));
            } else if path.is_dir() {
                // Ensure directory entry exists
                let dir_path = relative.to_string_lossy().to_string();
                dir_contents.entry(dir_path).or_default();
            }
        }

        // Build directory tree bottom-up
        self.build_directory_tree(tree, &mut dir_contents).await
    }

    async fn build_directory_tree<S: Store>(
        &self,
        tree: &HashTree<S>,
        dir_contents: &mut std::collections::HashMap<String, Vec<(String, Cid)>>,
    ) -> Result<Cid> {
        // Sort directories by depth (deepest first) to build bottom-up
        let mut dirs: Vec<String> = dir_contents.keys().cloned().collect();
        dirs.sort_by(|a, b| {
            let depth_a = a.matches('/').count() + if a.is_empty() { 0 } else { 1 };
            let depth_b = b.matches('/').count() + if b.is_empty() { 0 } else { 1 };
            depth_b.cmp(&depth_a) // Deepest first
        });

        let mut dir_cids: std::collections::HashMap<String, Cid> = std::collections::HashMap::new();

        for dir_path in dirs {
            let files = dir_contents.get(&dir_path).cloned().unwrap_or_default();

            let mut entries: Vec<HashTreeDirEntry> = files.into_iter()
                .map(|(name, cid)| HashTreeDirEntry::from_cid(name, &cid))
                .collect();

            // Add subdirectory entries
            for (subdir_path, cid) in &dir_cids {
                let parent = std::path::Path::new(subdir_path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                if parent == dir_path {
                    let name = std::path::Path::new(subdir_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    entries.push(HashTreeDirEntry::from_cid(name, cid));
                }
            }

            let cid = tree.put_directory(entries).await
                .map_err(|e| anyhow::anyhow!("Failed to create directory node: {}", e))?;

            dir_cids.insert(dir_path, cid);
        }

        // Return root Cid
        dir_cids.get("")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No root directory"))
    }

    /// Upload a file with CHK encryption, returns CID in format "hash:key"
    pub fn upload_file_encrypted<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        let file_path = file_path.as_ref();
        let file_content = std::fs::read(file_path)?;

        // Use unified API with encryption enabled (default)
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store));

        let (cid, _size) = sync_block_on(async {
            tree.put(&file_content).await
        }).map_err(|e| anyhow::anyhow!("Failed to encrypt file: {}", e))?;

        let cid_str = cid.to_string();

        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(cid_str)
    }

    /// Upload a directory with CHK encryption, returns CID
    /// Respects .gitignore by default
    pub fn upload_dir_encrypted<P: AsRef<Path>>(&self, dir_path: P) -> Result<String> {
        self.upload_dir_encrypted_with_options(dir_path, true)
    }

    /// Upload a directory with CHK encryption and options
    /// Returns CID as "hash:key" format for encrypted directories
    pub fn upload_dir_encrypted_with_options<P: AsRef<Path>>(&self, dir_path: P, respect_gitignore: bool) -> Result<String> {
        let dir_path = dir_path.as_ref();
        let store = self.store_arc();

        // Use unified API with encryption enabled (default)
        let tree = HashTree::new(HashTreeConfig::new(store));

        let root_cid = sync_block_on(async {
            self.upload_dir_recursive(&tree, dir_path, dir_path, respect_gitignore).await
        }).context("Failed to upload encrypted directory")?;

        let cid_str = root_cid.to_string(); // Returns "hash:key" or "hash"

        let mut wtxn = self.env.write_txn()?;
        // Pin by hash only (the key is for decryption, not identification)
        self.pins.put(&mut wtxn, root_cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(cid_str)
    }

    /// Get tree node by hash (raw bytes)
    pub fn get_tree_node(&self, hash: &[u8; 32]) -> Result<Option<TreeNode>> {
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        sync_block_on(async {
            tree.get_tree_node(hash).await
                .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))
        })
    }

    /// Store a raw blob, returns SHA256 hash as hex.
    pub fn put_blob(&self, data: &[u8]) -> Result<String> {
        let hash = sha256(data);
        self.router.put_sync(hash, data)
            .map_err(|e| anyhow::anyhow!("Failed to store blob: {}", e))?;
        Ok(to_hex(&hash))
    }

    /// Get a raw blob by SHA256 hash (raw bytes).
    pub fn get_blob(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        self.router.get_sync(hash)
            .map_err(|e| anyhow::anyhow!("Failed to get blob: {}", e))
    }

    /// Check if a blob exists by SHA256 hash (raw bytes).
    pub fn blob_exists(&self, hash: &[u8; 32]) -> Result<bool> {
        self.router.exists(hash)
            .map_err(|e| anyhow::anyhow!("Failed to check blob: {}", e))
    }

    // === Blossom ownership tracking ===
    // Uses composite key: sha256 (32 bytes) ++ pubkey (32 bytes) -> ()
    // This allows efficient multi-owner tracking with O(1) lookups

    /// Build composite key for blob_owners: sha256 ++ pubkey (64 bytes total)
    fn blob_owner_key(sha256: &[u8; 32], pubkey: &[u8; 32]) -> [u8; 64] {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(sha256);
        key[32..].copy_from_slice(pubkey);
        key
    }

    /// Add an owner (pubkey) to a blob for Blossom protocol
    /// Multiple users can own the same blob - it's only deleted when all owners remove it
    pub fn set_blob_owner(&self, sha256: &[u8; 32], pubkey: &[u8; 32]) -> Result<()> {
        let key = Self::blob_owner_key(sha256, pubkey);
        let mut wtxn = self.env.write_txn()?;

        // Add ownership entry (idempotent - put overwrites)
        self.blob_owners.put(&mut wtxn, &key[..], &())?;

        // Convert sha256 to hex for BlobMetadata (which stores sha256 as hex string)
        let sha256_hex = to_hex(sha256);

        // Get existing blobs for this pubkey (for /list endpoint)
        let mut blobs: Vec<BlobMetadata> = self
            .pubkey_blobs
            .get(&wtxn, pubkey)?
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default();

        // Check if blob already exists for this pubkey
        if !blobs.iter().any(|b| b.sha256 == sha256_hex) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Get size from raw blob
            let size = self
                .get_blob(sha256)?
                .map(|data| data.len() as u64)
                .unwrap_or(0);

            blobs.push(BlobMetadata {
                sha256: sha256_hex,
                size,
                mime_type: "application/octet-stream".to_string(),
                uploaded: now,
            });

            let blobs_json = serde_json::to_vec(&blobs)?;
            self.pubkey_blobs.put(&mut wtxn, pubkey, &blobs_json)?;
        }

        wtxn.commit()?;
        Ok(())
    }

    /// Check if a pubkey owns a blob
    pub fn is_blob_owner(&self, sha256: &[u8; 32], pubkey: &[u8; 32]) -> Result<bool> {
        let key = Self::blob_owner_key(sha256, pubkey);
        let rtxn = self.env.read_txn()?;
        Ok(self.blob_owners.get(&rtxn, &key[..])?.is_some())
    }

    /// Get all owners (pubkeys) of a blob via prefix scan (returns raw bytes)
    pub fn get_blob_owners(&self, sha256: &[u8; 32]) -> Result<Vec<[u8; 32]>> {
        let rtxn = self.env.read_txn()?;

        let mut owners = Vec::new();
        for item in self.blob_owners.prefix_iter(&rtxn, &sha256[..])? {
            let (key, _) = item?;
            if key.len() == 64 {
                // Extract pubkey from composite key (bytes 32-64)
                let mut pubkey = [0u8; 32];
                pubkey.copy_from_slice(&key[32..64]);
                owners.push(pubkey);
            }
        }
        Ok(owners)
    }

    /// Check if blob has any owners
    pub fn blob_has_owners(&self, sha256: &[u8; 32]) -> Result<bool> {
        let rtxn = self.env.read_txn()?;

        // Just check if any entry exists with this prefix
        for item in self.blob_owners.prefix_iter(&rtxn, &sha256[..])? {
            if item.is_ok() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Get the first owner (pubkey) of a blob (for backwards compatibility)
    pub fn get_blob_owner(&self, sha256: &[u8; 32]) -> Result<Option<[u8; 32]>> {
        Ok(self.get_blob_owners(sha256)?.into_iter().next())
    }

    /// Remove a user's ownership of a blossom blob
    /// Only deletes the actual blob when no owners remain
    /// Returns true if the blob was actually deleted (no owners left)
    pub fn delete_blossom_blob(&self, sha256: &[u8; 32], pubkey: &[u8; 32]) -> Result<bool> {
        let key = Self::blob_owner_key(sha256, pubkey);
        let mut wtxn = self.env.write_txn()?;

        // Remove this pubkey's ownership entry
        self.blob_owners.delete(&mut wtxn, &key[..])?;

        // Hex strings for logging and BlobMetadata (which stores sha256 as hex string)
        let sha256_hex = to_hex(sha256);

        // Remove from pubkey's blob list
        if let Some(blobs_bytes) = self.pubkey_blobs.get(&wtxn, pubkey)? {
            if let Ok(mut blobs) = serde_json::from_slice::<Vec<BlobMetadata>>(blobs_bytes) {
                blobs.retain(|b| b.sha256 != sha256_hex);
                let blobs_json = serde_json::to_vec(&blobs)?;
                self.pubkey_blobs.put(&mut wtxn, pubkey, &blobs_json)?;
            }
        }

        // Check if any other owners remain (prefix scan)
        let mut has_other_owners = false;
        for item in self.blob_owners.prefix_iter(&wtxn, &sha256[..])? {
            if item.is_ok() {
                has_other_owners = true;
                break;
            }
        }

        if has_other_owners {
            wtxn.commit()?;
            tracing::debug!(
                "Removed {} from blob {} owners, other owners remain",
                &to_hex(pubkey)[..8],
                &sha256_hex[..8]
            );
            return Ok(false);
        }

        // No owners left - delete the blob completely
        tracing::info!(
            "All owners removed from blob {}, deleting",
            &sha256_hex[..8]
        );

        // Delete raw blob (by content hash) - this deletes from S3 too
        let _ = self.router.delete_sync(sha256);

        wtxn.commit()?;
        Ok(true)
    }

    /// List all blobs owned by a pubkey (for Blossom /list endpoint)
    pub fn list_blobs_by_pubkey(&self, pubkey: &[u8; 32]) -> Result<Vec<crate::server::blossom::BlobDescriptor>> {
        let rtxn = self.env.read_txn()?;

        let blobs: Vec<BlobMetadata> = self
            .pubkey_blobs
            .get(&rtxn, pubkey)?
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default();

        Ok(blobs
            .into_iter()
            .map(|b| crate::server::blossom::BlobDescriptor {
                url: format!("/{}", b.sha256),
                sha256: b.sha256,
                size: b.size,
                mime_type: b.mime_type,
                uploaded: b.uploaded,
            })
            .collect())
    }

    /// Get a single chunk/blob by hash (raw bytes)
    pub fn get_chunk(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        self.router.get_sync(hash)
            .map_err(|e| anyhow::anyhow!("Failed to get chunk: {}", e))
    }

    /// Get file content by hash (raw bytes)
    /// Returns raw bytes (caller handles decryption if needed)
    pub fn get_file(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        sync_block_on(async {
            tree.read_file(hash).await
                .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))
        })
    }

    /// Get file content by Cid (hash + optional decryption key as raw bytes)
    /// Handles decryption automatically if key is present
    pub fn get_file_by_cid(&self, cid: &Cid) -> Result<Option<Vec<u8>>> {
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        sync_block_on(async {
            tree.get(cid).await
                .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))
        })
    }

    /// Resolve a path within a tree (returns Cid with key if encrypted)
    pub fn resolve_path(&self, cid: &Cid, path: &str) -> Result<Option<Cid>> {
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        sync_block_on(async {
            tree.resolve_path(cid, path).await
                .map_err(|e| anyhow::anyhow!("Failed to resolve path: {}", e))
        })
    }

    /// Get chunk metadata for a file (chunk list, sizes, total size)
    pub fn get_file_chunk_metadata(&self, hash: &[u8; 32]) -> Result<Option<FileChunkMetadata>> {
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());

        sync_block_on(async {
            // First check if the hash exists in the store at all
            // (either as a blob or tree node)
            let exists = store.has(&hash).await
                .map_err(|e| anyhow::anyhow!("Failed to check existence: {}", e))?;

            if !exists {
                return Ok(None);
            }

            // Get total size
            let total_size = tree.get_size(&hash).await
                .map_err(|e| anyhow::anyhow!("Failed to get size: {}", e))?;

            // Check if it's a tree (chunked) or blob
            let is_tree_node = tree.is_tree(&hash).await
                .map_err(|e| anyhow::anyhow!("Failed to check tree: {}", e))?;

            if !is_tree_node {
                // Single blob, not chunked
                return Ok(Some(FileChunkMetadata {
                    total_size,
                    chunk_hashes: vec![],
                    chunk_sizes: vec![],
                    is_chunked: false,
                }));
            }

            // Get tree node to extract chunk info
            let node = match tree.get_tree_node(&hash).await
                .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))? {
                Some(n) => n,
                None => return Ok(None),
            };

            // Check if it's a directory (has named links)
            let is_directory = tree.is_directory(&hash).await
                .map_err(|e| anyhow::anyhow!("Failed to check directory: {}", e))?;

            if is_directory {
                return Ok(None); // Not a file
            }

            // Extract chunk info from links
            let chunk_hashes: Vec<Hash> = node.links.iter().map(|l| l.hash).collect();
            let chunk_sizes: Vec<u64> = node.links.iter().map(|l| l.size).collect();

            Ok(Some(FileChunkMetadata {
                total_size,
                chunk_hashes,
                chunk_sizes,
                is_chunked: !node.links.is_empty(),
            }))
        })
    }

    /// Get byte range from file
    pub fn get_file_range(&self, hash: &[u8; 32], start: u64, end: Option<u64>) -> Result<Option<(Vec<u8>, u64)>> {
        let metadata = match self.get_file_chunk_metadata(hash)? {
            Some(m) => m,
            None => return Ok(None),
        };

        if metadata.total_size == 0 {
            return Ok(Some((Vec::new(), 0)));
        }

        if start >= metadata.total_size {
            return Ok(None);
        }

        let end = end.unwrap_or(metadata.total_size - 1).min(metadata.total_size - 1);

        // For non-chunked files, load entire file
        if !metadata.is_chunked {
            let content = self.get_file(hash)?.unwrap_or_default();
            let range_content = if start < content.len() as u64 {
                content[start as usize..=(end as usize).min(content.len() - 1)].to_vec()
            } else {
                Vec::new()
            };
            return Ok(Some((range_content, metadata.total_size)));
        }

        // For chunked files, load only needed chunks
        let mut result = Vec::new();
        let mut current_offset = 0u64;

        for (i, chunk_hash) in metadata.chunk_hashes.iter().enumerate() {
            let chunk_size = metadata.chunk_sizes[i];
            let chunk_end = current_offset + chunk_size - 1;

            // Check if this chunk overlaps with requested range
            if chunk_end >= start && current_offset <= end {
                let chunk_content = match self.get_chunk(chunk_hash)? {
                    Some(content) => content,
                    None => {
                        return Err(anyhow::anyhow!("Chunk {} not found", to_hex(chunk_hash)));
                    }
                };

                let chunk_read_start = if current_offset >= start {
                    0
                } else {
                    (start - current_offset) as usize
                };

                let chunk_read_end = if chunk_end <= end {
                    chunk_size as usize - 1
                } else {
                    (end - current_offset) as usize
                };

                result.extend_from_slice(&chunk_content[chunk_read_start..=chunk_read_end]);
            }

            current_offset += chunk_size;

            if current_offset > end {
                break;
            }
        }

        Ok(Some((result, metadata.total_size)))
    }

    /// Stream file range as chunks using Arc for async/Send contexts
    pub fn stream_file_range_chunks_owned(
        self: Arc<Self>,
        hash: &[u8; 32],
        start: u64,
        end: u64,
    ) -> Result<Option<FileRangeChunksOwned>> {
        let metadata = match self.get_file_chunk_metadata(hash)? {
            Some(m) => m,
            None => return Ok(None),
        };

        if metadata.total_size == 0 || start >= metadata.total_size {
            return Ok(None);
        }

        let end = end.min(metadata.total_size - 1);

        Ok(Some(FileRangeChunksOwned {
            store: self,
            metadata,
            start,
            end,
            current_chunk_idx: 0,
            current_offset: 0,
        }))
    }

    /// Get directory structure by hash (raw bytes)
    pub fn get_directory_listing(&self, hash: &[u8; 32]) -> Result<Option<DirectoryListing>> {
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        sync_block_on(async {
            // Check if it's a directory
            let is_dir = tree.is_directory(&hash).await
                .map_err(|e| anyhow::anyhow!("Failed to check directory: {}", e))?;

            if !is_dir {
                return Ok(None);
            }

            // Get directory entries (public Cid - no encryption key)
            let cid = hashtree_core::Cid::public(*hash);
            let tree_entries = tree.list_directory(&cid).await
                .map_err(|e| anyhow::anyhow!("Failed to list directory: {}", e))?;

            let entries: Vec<DirEntry> = tree_entries.into_iter().map(|e| DirEntry {
                name: e.name,
                cid: to_hex(&e.hash),
                is_directory: e.link_type.is_tree(),
                size: e.size,
            }).collect();

            Ok(Some(DirectoryListing {
                dir_name: String::new(),
                entries,
            }))
        })
    }

    /// Pin a hash (prevent garbage collection)
    pub fn pin(&self, hash: &[u8; 32]) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, hash.as_slice(), &())?;
        wtxn.commit()?;
        Ok(())
    }

    /// Unpin a hash (allow garbage collection)
    pub fn unpin(&self, hash: &[u8; 32]) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.pins.delete(&mut wtxn, hash.as_slice())?;
        wtxn.commit()?;
        Ok(())
    }

    /// Check if hash is pinned
    pub fn is_pinned(&self, hash: &[u8; 32]) -> Result<bool> {
        let rtxn = self.env.read_txn()?;
        Ok(self.pins.get(&rtxn, hash.as_slice())?.is_some())
    }

    /// List all pinned hashes (raw bytes)
    pub fn list_pins_raw(&self) -> Result<Vec<[u8; 32]>> {
        let rtxn = self.env.read_txn()?;
        let mut pins = Vec::new();

        for item in self.pins.iter(&rtxn)? {
            let (hash_bytes, _) = item?;
            if hash_bytes.len() == 32 {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(hash_bytes);
                pins.push(hash);
            }
        }

        Ok(pins)
    }

    /// List all pinned hashes with names
    pub fn list_pins_with_names(&self) -> Result<Vec<PinnedItem>> {
        let rtxn = self.env.read_txn()?;
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());
        let mut pins = Vec::new();

        for item in self.pins.iter(&rtxn)? {
            let (hash_bytes, _) = item?;
            if hash_bytes.len() != 32 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(hash_bytes);

            // Try to determine if it's a directory
            let is_directory = sync_block_on(async {
                tree.is_directory(&hash).await.unwrap_or(false)
            });

            pins.push(PinnedItem {
                cid: to_hex(&hash),
                name: "Unknown".to_string(),
                is_directory,
            });
        }

        Ok(pins)
    }

    // === Tree indexing for eviction ===

    /// Index a tree after sync - tracks all blobs in the tree for eviction
    ///
    /// If `ref_key` is provided (e.g. "npub.../name"), it will replace any existing
    /// tree with that ref, allowing old versions to be evicted.
    pub fn index_tree(
        &self,
        root_hash: &Hash,
        owner: &str,
        name: Option<&str>,
        priority: u8,
        ref_key: Option<&str>,
    ) -> Result<()> {
        let root_hex = to_hex(root_hash);

        // If ref_key provided, check for and unindex old version
        if let Some(key) = ref_key {
            let rtxn = self.env.read_txn()?;
            if let Some(old_hash_bytes) = self.tree_refs.get(&rtxn, key)? {
                if old_hash_bytes != root_hash.as_slice() {
                    let old_hash: Hash = old_hash_bytes.try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid hash in tree_refs"))?;
                    drop(rtxn);
                    // Unindex old tree (will delete orphaned blobs)
                    let _ = self.unindex_tree(&old_hash);
                    tracing::debug!("Replaced old tree for ref {}", key);
                }
            }
        }

        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        // Walk tree and collect all blob hashes + compute total size
        let (blob_hashes, total_size) = sync_block_on(async {
            self.collect_tree_blobs(&tree, root_hash).await
        })?;

        let mut wtxn = self.env.write_txn()?;

        // Store blob-tree relationships (64-byte key: blob_hash ++ tree_hash)
        for blob_hash in &blob_hashes {
            let mut key = [0u8; 64];
            key[..32].copy_from_slice(blob_hash);
            key[32..].copy_from_slice(root_hash);
            self.blob_trees.put(&mut wtxn, &key[..], &())?;
        }

        // Store tree metadata
        let meta = TreeMeta {
            owner: owner.to_string(),
            name: name.map(|s| s.to_string()),
            synced_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            total_size,
            priority,
        };
        let meta_bytes = rmp_serde::to_vec(&meta)
            .map_err(|e| anyhow::anyhow!("Failed to serialize TreeMeta: {}", e))?;
        self.tree_meta.put(&mut wtxn, root_hash.as_slice(), &meta_bytes)?;

        // Store ref -> hash mapping if ref_key provided
        if let Some(key) = ref_key {
            self.tree_refs.put(&mut wtxn, key, root_hash.as_slice())?;
        }

        wtxn.commit()?;

        tracing::debug!(
            "Indexed tree {} ({} blobs, {} bytes, priority {})",
            &root_hex[..8],
            blob_hashes.len(),
            total_size,
            priority
        );

        Ok(())
    }

    /// Collect all blob hashes in a tree and compute total size
    async fn collect_tree_blobs<S: Store>(
        &self,
        tree: &HashTree<S>,
        root: &Hash,
    ) -> Result<(Vec<Hash>, u64)> {
        let mut blobs = Vec::new();
        let mut total_size = 0u64;
        let mut stack = vec![*root];

        while let Some(hash) = stack.pop() {
            // Check if it's a tree node
            let is_tree = tree.is_tree(&hash).await
                .map_err(|e| anyhow::anyhow!("Failed to check tree: {}", e))?;

            if is_tree {
                // Get tree node and add children to stack
                if let Some(node) = tree.get_tree_node(&hash).await
                    .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))?
                {
                    for link in &node.links {
                        stack.push(link.hash);
                    }
                }
            } else {
                // It's a blob - get its size
                if let Some(data) = self.router.get_sync(&hash)
                    .map_err(|e| anyhow::anyhow!("Failed to get blob: {}", e))?
                {
                    total_size += data.len() as u64;
                    blobs.push(hash);
                }
            }
        }

        Ok((blobs, total_size))
    }

    /// Unindex a tree - removes blob-tree mappings and deletes orphaned blobs
    /// Returns the number of bytes freed
    pub fn unindex_tree(&self, root_hash: &Hash) -> Result<u64> {
        let root_hex = to_hex(root_hash);

        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        // Walk tree and collect all blob hashes
        let (blob_hashes, _) = sync_block_on(async {
            self.collect_tree_blobs(&tree, root_hash).await
        })?;

        let mut wtxn = self.env.write_txn()?;
        let mut freed = 0u64;

        // For each blob, remove the blob-tree entry and check if orphaned
        for blob_hash in &blob_hashes {
            // Delete blob-tree entry (64-byte key: blob_hash ++ tree_hash)
            let mut key = [0u8; 64];
            key[..32].copy_from_slice(blob_hash);
            key[32..].copy_from_slice(root_hash);
            self.blob_trees.delete(&mut wtxn, &key[..])?;

            // Check if blob is in any other tree (prefix scan on first 32 bytes)
            let rtxn = self.env.read_txn()?;
            let mut has_other_tree = false;

            for item in self.blob_trees.prefix_iter(&rtxn, &blob_hash[..])? {
                if item.is_ok() {
                    has_other_tree = true;
                    break;
                }
            }
            drop(rtxn);

            // If orphaned, delete the blob
            if !has_other_tree {
                if let Some(data) = self.router.get_sync(blob_hash)
                    .map_err(|e| anyhow::anyhow!("Failed to get blob: {}", e))?
                {
                    freed += data.len() as u64;
                    // Delete locally only - keep S3 as archive
                    self.router.delete_local_only(blob_hash)
                        .map_err(|e| anyhow::anyhow!("Failed to delete blob: {}", e))?;
                }
            }
        }

        // Delete tree node itself if exists
        if let Some(data) = self.router.get_sync(root_hash)
            .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))?
        {
            freed += data.len() as u64;
            // Delete locally only - keep S3 as archive
            self.router.delete_local_only(root_hash)
                .map_err(|e| anyhow::anyhow!("Failed to delete tree node: {}", e))?;
        }

        // Delete tree metadata
        self.tree_meta.delete(&mut wtxn, root_hash.as_slice())?;

        wtxn.commit()?;

        tracing::debug!(
            "Unindexed tree {} ({} bytes freed)",
            &root_hex[..8],
            freed
        );

        Ok(freed)
    }

    /// Get tree metadata
    pub fn get_tree_meta(&self, root_hash: &Hash) -> Result<Option<TreeMeta>> {
        let rtxn = self.env.read_txn()?;
        if let Some(bytes) = self.tree_meta.get(&rtxn, root_hash.as_slice())? {
            let meta: TreeMeta = rmp_serde::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;
            Ok(Some(meta))
        } else {
            Ok(None)
        }
    }

    /// List all indexed trees
    pub fn list_indexed_trees(&self) -> Result<Vec<(Hash, TreeMeta)>> {
        let rtxn = self.env.read_txn()?;
        let mut trees = Vec::new();

        for item in self.tree_meta.iter(&rtxn)? {
            let (hash_bytes, meta_bytes) = item?;
            let hash: Hash = hash_bytes.try_into()
                .map_err(|_| anyhow::anyhow!("Invalid hash in tree_meta"))?;
            let meta: TreeMeta = rmp_serde::from_slice(meta_bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;
            trees.push((hash, meta));
        }

        Ok(trees)
    }

    /// Get total tracked storage size (sum of all tree_meta.total_size)
    pub fn tracked_size(&self) -> Result<u64> {
        let rtxn = self.env.read_txn()?;
        let mut total = 0u64;

        for item in self.tree_meta.iter(&rtxn)? {
            let (_, bytes) = item?;
            let meta: TreeMeta = rmp_serde::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;
            total += meta.total_size;
        }

        Ok(total)
    }

    /// Get evictable trees sorted by (priority ASC, synced_at ASC)
    fn get_evictable_trees(&self) -> Result<Vec<(Hash, TreeMeta)>> {
        let mut trees = self.list_indexed_trees()?;

        // Sort by priority (lower first), then by synced_at (older first)
        trees.sort_by(|a, b| {
            match a.1.priority.cmp(&b.1.priority) {
                std::cmp::Ordering::Equal => a.1.synced_at.cmp(&b.1.synced_at),
                other => other,
            }
        });

        Ok(trees)
    }

    /// Run eviction if storage is over quota
    /// Returns bytes freed
    ///
    /// Eviction order:
    /// 1. Orphaned blobs (not in any indexed tree and not pinned)
    /// 2. Trees by priority (lowest first) and age (oldest first)
    pub fn evict_if_needed(&self) -> Result<u64> {
        // Get actual storage used
        let stats = self.router.stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;
        let current = stats.total_bytes;

        if current <= self.max_size_bytes {
            return Ok(0);
        }

        // Target 90% of max to avoid constant eviction
        let target = self.max_size_bytes * 90 / 100;
        let mut freed = 0u64;
        let mut current_size = current;

        // Phase 1: Evict orphaned blobs (not in any tree and not pinned)
        let orphan_freed = self.evict_orphaned_blobs()?;
        freed += orphan_freed;
        current_size = current_size.saturating_sub(orphan_freed);

        if orphan_freed > 0 {
            tracing::info!("Evicted orphaned blobs: {} bytes freed", orphan_freed);
        }

        // Check if we're now under target
        if current_size <= target {
            if freed > 0 {
                tracing::info!("Eviction complete: {} bytes freed", freed);
            }
            return Ok(freed);
        }

        // Phase 2: Evict trees by priority (lowest first) and age (oldest first)
        // Own trees CAN be evicted (just last), but PINNED trees are never evicted
        let evictable = self.get_evictable_trees()?;

        for (root_hash, meta) in evictable {
            if current_size <= target {
                break;
            }

            let root_hex = to_hex(&root_hash);

            // Never evict pinned trees
            if self.is_pinned(&root_hash)? {
                continue;
            }

            let tree_freed = self.unindex_tree(&root_hash)?;
            freed += tree_freed;
            current_size = current_size.saturating_sub(tree_freed);

            tracing::info!(
                "Evicted tree {} (owner={}, priority={}, {} bytes)",
                &root_hex[..8],
                &meta.owner[..8.min(meta.owner.len())],
                meta.priority,
                tree_freed
            );
        }

        if freed > 0 {
            tracing::info!("Eviction complete: {} bytes freed", freed);
        }

        Ok(freed)
    }

    /// Evict blobs that are not part of any indexed tree and not pinned
    fn evict_orphaned_blobs(&self) -> Result<u64> {
        let mut freed = 0u64;

        // Get all blob hashes from store
        let all_hashes = self.router.list()
            .map_err(|e| anyhow::anyhow!("Failed to list hashes: {}", e))?;

        // Get pinned hashes as raw bytes
        let rtxn = self.env.read_txn()?;
        let pinned: HashSet<Hash> = self.pins.iter(&rtxn)?
            .filter_map(|item| item.ok())
            .filter_map(|(hash_bytes, _)| {
                if hash_bytes.len() == 32 {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(hash_bytes);
                    Some(hash)
                } else {
                    None
                }
            })
            .collect();

        // Collect all blob hashes that are in at least one tree
        // Key format is blob_hash (32 bytes) ++ tree_hash (32 bytes)
        let mut blobs_in_trees: HashSet<Hash> = HashSet::new();
        for item in self.blob_trees.iter(&rtxn)? {
            if let Ok((key_bytes, _)) = item {
                if key_bytes.len() >= 32 {
                    let blob_hash: Hash = key_bytes[..32].try_into().unwrap();
                    blobs_in_trees.insert(blob_hash);
                }
            }
        }
        drop(rtxn);

        // Find and delete orphaned blobs
        for hash in all_hashes {
            // Skip if pinned
            if pinned.contains(&hash) {
                continue;
            }

            // Skip if part of any tree
            if blobs_in_trees.contains(&hash) {
                continue;
            }

            // This blob is orphaned - delete locally (keep S3 as archive)
            if let Ok(Some(data)) = self.router.get_sync(&hash) {
                freed += data.len() as u64;
                let _ = self.router.delete_local_only(&hash);
                tracing::debug!("Deleted orphaned blob {} ({} bytes)", &to_hex(&hash)[..8], data.len());
            }
        }

        Ok(freed)
    }

    /// Get the maximum storage size in bytes
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    /// Get storage usage by priority tier
    pub fn storage_by_priority(&self) -> Result<StorageByPriority> {
        let rtxn = self.env.read_txn()?;
        let mut own = 0u64;
        let mut followed = 0u64;
        let mut other = 0u64;

        for item in self.tree_meta.iter(&rtxn)? {
            let (_, bytes) = item?;
            let meta: TreeMeta = rmp_serde::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;

            if meta.priority >= PRIORITY_OWN {
                own += meta.total_size;
            } else if meta.priority >= PRIORITY_FOLLOWED {
                followed += meta.total_size;
            } else {
                other += meta.total_size;
            }
        }

        Ok(StorageByPriority { own, followed, other })
    }

    /// Get storage statistics
    pub fn get_storage_stats(&self) -> Result<StorageStats> {
        let rtxn = self.env.read_txn()?;
        let total_pins = self.pins.len(&rtxn)? as usize;

        let stats = self.router.stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;

        Ok(StorageStats {
            total_dags: stats.count,
            pinned_dags: total_pins,
            total_bytes: stats.total_bytes,
        })
    }

    // === Cached roots (replaces nostrdb event caching) ===

    /// Get cached root for a pubkey/tree_name pair
    pub fn get_cached_root(&self, pubkey_hex: &str, tree_name: &str) -> Result<Option<CachedRoot>> {
        let key = format!("{}/{}", pubkey_hex, tree_name);
        let rtxn = self.env.read_txn()?;
        if let Some(bytes) = self.cached_roots.get(&rtxn, &key)? {
            let root: CachedRoot = rmp_serde::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize CachedRoot: {}", e))?;
            Ok(Some(root))
        } else {
            Ok(None)
        }
    }

    /// Set cached root for a pubkey/tree_name pair
    pub fn set_cached_root(
        &self,
        pubkey_hex: &str,
        tree_name: &str,
        hash: &str,
        key: Option<&str>,
        visibility: &str,
        updated_at: u64,
    ) -> Result<()> {
        let db_key = format!("{}/{}", pubkey_hex, tree_name);
        let root = CachedRoot {
            hash: hash.to_string(),
            key: key.map(|k| k.to_string()),
            updated_at,
            visibility: visibility.to_string(),
        };
        let bytes = rmp_serde::to_vec(&root)
            .map_err(|e| anyhow::anyhow!("Failed to serialize CachedRoot: {}", e))?;
        let mut wtxn = self.env.write_txn()?;
        self.cached_roots.put(&mut wtxn, &db_key, &bytes)?;
        wtxn.commit()?;
        Ok(())
    }

    /// List all cached roots for a pubkey
    pub fn list_cached_roots(&self, pubkey_hex: &str) -> Result<Vec<(String, CachedRoot)>> {
        let prefix = format!("{}/", pubkey_hex);
        let rtxn = self.env.read_txn()?;
        let mut results = Vec::new();

        for item in self.cached_roots.iter(&rtxn)? {
            let (key, bytes) = item?;
            if key.starts_with(&prefix) {
                let tree_name = key.strip_prefix(&prefix).unwrap_or(key);
                let root: CachedRoot = rmp_serde::from_slice(bytes)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize CachedRoot: {}", e))?;
                results.push((tree_name.to_string(), root));
            }
        }

        Ok(results)
    }

    /// Delete a cached root
    pub fn delete_cached_root(&self, pubkey_hex: &str, tree_name: &str) -> Result<bool> {
        let key = format!("{}/{}", pubkey_hex, tree_name);
        let mut wtxn = self.env.write_txn()?;
        let deleted = self.cached_roots.delete(&mut wtxn, &key)?;
        wtxn.commit()?;
        Ok(deleted)
    }

    /// Garbage collect unpinned content
    pub fn gc(&self) -> Result<GcStats> {
        let rtxn = self.env.read_txn()?;

        // Get all pinned hashes as raw bytes
        let pinned: HashSet<Hash> = self.pins.iter(&rtxn)?
            .filter_map(|item| item.ok())
            .filter_map(|(hash_bytes, _)| {
                if hash_bytes.len() == 32 {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(hash_bytes);
                    Some(hash)
                } else {
                    None
                }
            })
            .collect();

        drop(rtxn);

        // Get all stored hashes
        let all_hashes = self.router.list()
            .map_err(|e| anyhow::anyhow!("Failed to list hashes: {}", e))?;

        // Delete unpinned hashes
        let mut deleted = 0;
        let mut freed_bytes = 0u64;

        for hash in all_hashes {
            if !pinned.contains(&hash) {
                if let Ok(Some(data)) = self.router.get_sync(&hash) {
                    freed_bytes += data.len() as u64;
                    // Delete locally only - keep S3 as archive
                    let _ = self.router.delete_local_only(&hash);
                    deleted += 1;
                }
            }
        }

        Ok(GcStats {
            deleted_dags: deleted,
            freed_bytes,
        })
    }

    /// Verify LMDB blob integrity - checks that stored data matches its key hash
    /// Returns verification statistics and optionally deletes corrupted entries
    pub fn verify_lmdb_integrity(&self, delete: bool) -> Result<VerifyResult> {
        let all_hashes = self.router.list()
            .map_err(|e| anyhow::anyhow!("Failed to list hashes: {}", e))?;

        let total = all_hashes.len();
        let mut valid = 0;
        let mut corrupted = 0;
        let mut deleted = 0;
        let mut corrupted_hashes = Vec::new();

        for hash in &all_hashes {
            let hash_hex = to_hex(hash);

            match self.router.get_sync(hash) {
                Ok(Some(data)) => {
                    // Compute actual SHA256 of data
                    let actual_hash = sha256(&data);

                    if actual_hash == *hash {
                        valid += 1;
                    } else {
                        corrupted += 1;
                        let actual_hex = to_hex(&actual_hash);
                        println!("  CORRUPTED: key={} actual={} size={}",
                            &hash_hex[..16], &actual_hex[..16], data.len());
                        corrupted_hashes.push(*hash);
                    }
                }
                Ok(None) => {
                    // Hash exists in index but data is missing
                    corrupted += 1;
                    println!("  MISSING: key={}", &hash_hex[..16]);
                    corrupted_hashes.push(*hash);
                }
                Err(e) => {
                    corrupted += 1;
                    println!("  ERROR: key={} err={}", &hash_hex[..16], e);
                    corrupted_hashes.push(*hash);
                }
            }
        }

        // Delete corrupted entries if requested
        if delete {
            for hash in &corrupted_hashes {
                match self.router.delete_sync(hash) {
                    Ok(true) => deleted += 1,
                    Ok(false) => {} // Already deleted
                    Err(e) => {
                        let hash_hex = to_hex(hash);
                        println!("  Failed to delete {}: {}", &hash_hex[..16], e);
                    }
                }
            }
        }

        Ok(VerifyResult {
            total,
            valid,
            corrupted,
            deleted,
        })
    }

    /// Verify R2/S3 blob integrity - lists all objects and verifies hash matches filename
    /// Returns verification statistics and optionally deletes corrupted entries
    #[cfg(feature = "s3")]
    pub async fn verify_r2_integrity(&self, delete: bool) -> Result<VerifyResult> {
        use aws_sdk_s3::Client as S3Client;

        // Get S3 client from router (we need to access it directly)
        // For now, we'll create a new client from config
        let config = crate::config::Config::load()?;
        let s3_config = config.storage.s3
            .ok_or_else(|| anyhow::anyhow!("S3 not configured"))?;

        // Build AWS config
        let aws_config = aws_config::from_env()
            .region(aws_sdk_s3::config::Region::new(s3_config.region.clone()))
            .load()
            .await;

        let s3_client = S3Client::from_conf(
            aws_sdk_s3::config::Builder::from(&aws_config)
                .endpoint_url(&s3_config.endpoint)
                .force_path_style(true)
                .build()
        );

        let bucket = &s3_config.bucket;
        let prefix = s3_config.prefix.as_deref().unwrap_or("");

        let mut total = 0;
        let mut valid = 0;
        let mut corrupted = 0;
        let mut deleted = 0;
        let mut corrupted_keys = Vec::new();

        // List all objects in bucket
        let mut continuation_token: Option<String> = None;

        loop {
            let mut list_req = s3_client.list_objects_v2()
                .bucket(bucket)
                .prefix(prefix);

            if let Some(ref token) = continuation_token {
                list_req = list_req.continuation_token(token);
            }

            let list_resp = list_req.send().await
                .map_err(|e| anyhow::anyhow!("Failed to list S3 objects: {}", e))?;

            for object in list_resp.contents() {
                let key = object.key().unwrap_or("");

                // Skip non-.bin files
                if !key.ends_with(".bin") {
                    continue;
                }

                total += 1;

                // Extract expected hash from filename (remove prefix and .bin)
                let filename = key.strip_prefix(prefix).unwrap_or(key);
                let expected_hash_hex = filename.strip_suffix(".bin").unwrap_or(filename);

                // Validate it's a valid hex hash
                if expected_hash_hex.len() != 64 {
                    corrupted += 1;
                    println!("  INVALID KEY: {}", key);
                    corrupted_keys.push(key.to_string());
                    continue;
                }

                let expected_hash = match from_hex(expected_hash_hex) {
                    Ok(h) => h,
                    Err(_) => {
                        corrupted += 1;
                        println!("  INVALID HEX: {}", key);
                        corrupted_keys.push(key.to_string());
                        continue;
                    }
                };

                // Download and verify content
                match s3_client.get_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        match resp.body.collect().await {
                            Ok(bytes) => {
                                let data = bytes.into_bytes();
                                let actual_hash = sha256(&data);

                                if actual_hash == expected_hash {
                                    valid += 1;
                                } else {
                                    corrupted += 1;
                                    let actual_hex = to_hex(&actual_hash);
                                    println!("  CORRUPTED: key={} actual={} size={}",
                                        &expected_hash_hex[..16], &actual_hex[..16], data.len());
                                    corrupted_keys.push(key.to_string());
                                }
                            }
                            Err(e) => {
                                corrupted += 1;
                                println!("  READ ERROR: {} - {}", key, e);
                                corrupted_keys.push(key.to_string());
                            }
                        }
                    }
                    Err(e) => {
                        corrupted += 1;
                        println!("  FETCH ERROR: {} - {}", key, e);
                        corrupted_keys.push(key.to_string());
                    }
                }

                // Progress indicator every 100 objects
                if total % 100 == 0 {
                    println!("  Progress: {} objects checked, {} corrupted so far", total, corrupted);
                }
            }

            // Check if there are more objects
            if list_resp.is_truncated() == Some(true) {
                continuation_token = list_resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        // Delete corrupted entries if requested
        if delete {
            for key in &corrupted_keys {
                match s3_client.delete_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                {
                    Ok(_) => deleted += 1,
                    Err(e) => {
                        println!("  Failed to delete {}: {}", key, e);
                    }
                }
            }
        }

        Ok(VerifyResult {
            total,
            valid,
            corrupted,
            deleted,
        })
    }

    /// Fallback for non-S3 builds
    #[cfg(not(feature = "s3"))]
    pub async fn verify_r2_integrity(&self, _delete: bool) -> Result<VerifyResult> {
        Err(anyhow::anyhow!("S3 feature not enabled"))
    }
}

/// Result of blob integrity verification
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub total: usize,
    pub valid: usize,
    pub corrupted: usize,
    pub deleted: usize,
}

#[derive(Debug)]
pub struct StorageStats {
    pub total_dags: usize,
    pub pinned_dags: usize,
    pub total_bytes: u64,
}

/// Storage usage broken down by priority tier
#[derive(Debug, Clone)]
pub struct StorageByPriority {
    /// Own/pinned trees (priority 255)
    pub own: u64,
    /// Followed users' trees (priority 128)
    pub followed: u64,
    /// Other trees (priority 64)
    pub other: u64,
}

#[derive(Debug, Clone)]
pub struct FileChunkMetadata {
    pub total_size: u64,
    pub chunk_hashes: Vec<Hash>,
    pub chunk_sizes: Vec<u64>,
    pub is_chunked: bool,
}

/// Owned iterator for async streaming
pub struct FileRangeChunksOwned {
    store: Arc<HashtreeStore>,
    metadata: FileChunkMetadata,
    start: u64,
    end: u64,
    current_chunk_idx: usize,
    current_offset: u64,
}

impl Iterator for FileRangeChunksOwned {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.metadata.is_chunked || self.current_chunk_idx >= self.metadata.chunk_hashes.len() {
            return None;
        }

        if self.current_offset > self.end {
            return None;
        }

        let chunk_hash = &self.metadata.chunk_hashes[self.current_chunk_idx];
        let chunk_size = self.metadata.chunk_sizes[self.current_chunk_idx];
        let chunk_end = self.current_offset + chunk_size - 1;

        self.current_chunk_idx += 1;

        if chunk_end < self.start || self.current_offset > self.end {
            self.current_offset += chunk_size;
            return self.next();
        }

        let chunk_content = match self.store.get_chunk(chunk_hash) {
            Ok(Some(content)) => content,
            Ok(None) => {
                return Some(Err(anyhow::anyhow!("Chunk {} not found", to_hex(chunk_hash))));
            }
            Err(e) => {
                return Some(Err(e));
            }
        };

        let chunk_read_start = if self.current_offset >= self.start {
            0
        } else {
            (self.start - self.current_offset) as usize
        };

        let chunk_read_end = if chunk_end <= self.end {
            chunk_size as usize - 1
        } else {
            (self.end - self.current_offset) as usize
        };

        let result = chunk_content[chunk_read_start..=chunk_read_end].to_vec();
        self.current_offset += chunk_size;

        Some(Ok(result))
    }
}

#[derive(Debug)]
pub struct GcStats {
    pub deleted_dags: usize,
    pub freed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub cid: String,
    pub is_directory: bool,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct DirectoryListing {
    pub dir_name: String,
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Clone)]
pub struct PinnedItem {
    pub cid: String,
    pub name: String,
    pub is_directory: bool,
}

/// Blob metadata for Blossom protocol
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlobMetadata {
    pub sha256: String,
    pub size: u64,
    pub mime_type: String,
    pub uploaded: u64,
}

// Implement ContentStore trait for WebRTC data exchange
impl crate::webrtc::ContentStore for HashtreeStore {
    fn get(&self, hash_hex: &str) -> Result<Option<Vec<u8>>> {
        let hash = from_hex(hash_hex)
            .map_err(|e| anyhow::anyhow!("Invalid hash: {}", e))?;
        self.get_chunk(&hash)
    }
}
