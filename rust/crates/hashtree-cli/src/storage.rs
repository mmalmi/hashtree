use crate::managed_env::ManagedEnv;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::executor::block_on as sync_block_on;
use futures::StreamExt;
use hashtree_config::StorageBackend;
use hashtree_core::store::{slice_blob_range, PutManyReport, Store, StoreError};
use hashtree_core::{sha256, to_hex, types::Hash, Cid, HashTree, HashTreeConfig, TreeNode};
use hashtree_fs::FsBlobStore;
#[cfg(feature = "lmdb")]
use hashtree_lmdb::{
    open_configured_lmdb_blob_store, open_shared_lmdb_blob_store, ConfiguredLmdbBlobStore,
    ExternalBlobOptions, LmdbBlobReader, LmdbBlobStore, PoolStore,
};
use heed::types::*;
use heed::{Database, EnvFlags, EnvOpenOptions, Error as HeedError, MdbError, PutFlags};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(feature = "s3")]
use std::future::Future;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod upload;
pub use upload::{AddProgress, AddProgressSnapshot};

mod maintenance;
mod quota;
mod retention;

use quota::CacheQuotaController;

#[cfg(feature = "s3")]
const DEFAULT_S3_SYNC_TIMEOUT_MS: u64 = 5_000;
#[cfg(feature = "s3")]
const S3_SYNC_TIMEOUT_MS_ENV: &str = "HTREE_S3_SYNC_TIMEOUT_MS";

pub use maintenance::{
    compact_lmdb_environments_under, CompactResult, R2ImportOptions, R2ImportResult, VerifyResult,
};
pub use retention::{
    OwnedBlobStats, PinTreeError, PinTreeResult, PinnedItem, RootRetentionReport,
    StorageByPriority, StorageStats, TreeIndexLimits, TreeMeta,
};

/// Priority levels for tree eviction
pub const PRIORITY_OTHER: u8 = 64;
pub const PRIORITY_FOLLOWED: u8 = 128;
pub const PRIORITY_OWN: u8 = 255;
const LMDB_MAX_READERS: u32 = 1024;
const LMDB_METADATA_MIN_MAP_SIZE_BYTES: u64 = 64 * 1024 * 1024;
const LMDB_METADATA_MAX_MAP_SIZE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const LMDB_METADATA_STORAGE_RATIO_DIVISOR: u64 = 1024;
const LMDB_METADATA_REOPEN_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(all(test, feature = "lmdb"))]
const LMDB_BLOB_MIN_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024;
const ACCESS_UPDATE_INTERVAL_SECS: u64 = 300;
const ACCESS_UPDATE_GATE_MAX_ENTRIES: usize = 4096;
const DEFAULT_ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT: usize = 64;
const ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT_ENV: &str = "HTREE_ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT";
const DEFAULT_FILE_METADATA_CACHE_ENTRIES: usize = 128;
const FILE_METADATA_CACHE_ENTRIES_ENV: &str = "HTREE_FILE_METADATA_CACHE_ENTRIES";
const SLOW_OWNED_BLOB_BATCH_LOG_MS_ENV: &str = "HTREE_SLOW_OWNED_BLOB_BATCH_LOG_MS";
const SLOW_CACHED_BLOB_BATCH_LOG_MS_ENV: &str = "HTREE_SLOW_CACHED_BLOB_BATCH_LOG_MS";
pub const LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME: &str = "blob-files-v1";
pub(crate) const POOL_MIGRATION_DELETE_DISABLED: &str =
    "explicit deletes are temporarily disabled during legacy PoolStore migration";
#[cfg(feature = "lmdb")]
const LMDB_HOT_BLOB_DIR_ENV: &str = "HTREE_LMDB_HOT_BLOB_DIR";
#[cfg(feature = "lmdb")]
const LMDB_HOT_BLOB_LEGACY_DIR_ENV: &str = "HTREE_LMDB_HOT_BLOB_LEGACY_DIR";
#[cfg(feature = "lmdb")]
const LMDB_HOT_EXTERNAL_BLOB_DIR_ENV: &str = "HTREE_LMDB_HOT_EXTERNAL_BLOB_DIR";
#[cfg(feature = "lmdb")]
const LMDB_LEGACY_EXTERNAL_BLOB_DIR_ENV: &str = "HTREE_LMDB_LEGACY_EXTERNAL_BLOB_DIR";
const LMDB_NO_READ_AHEAD_ENV: &str = "HTREE_LMDB_NO_READ_AHEAD";
const LMDB_NO_SYNC_ENV: &str = "HTREE_LMDB_NO_SYNC";
const LMDB_NO_META_SYNC_ENV: &str = "HTREE_LMDB_NO_META_SYNC";

fn slow_owned_blob_batch_log_ms() -> Option<u128> {
    std::env::var(SLOW_OWNED_BLOB_BATCH_LOG_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .filter(|value| *value > 0)
}

fn slow_cached_blob_batch_log_ms() -> Option<u128> {
    std::env::var(SLOW_CACHED_BLOB_BATCH_LOG_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .filter(|value| *value > 0)
}

fn access_update_background_batch_limit() -> usize {
    std::env::var(ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT)
}

fn file_metadata_cache_entries() -> NonZeroUsize {
    let entries = std::env::var(FILE_METADATA_CACHE_ENTRIES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_FILE_METADATA_CACHE_ENTRIES);
    NonZeroUsize::new(entries).unwrap_or(NonZeroUsize::new(1).expect("nonzero cache size"))
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().and_then(|value| {
        let value = value.trim();
        if value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes") {
            Some(true)
        } else if value == "0"
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("no")
        {
            Some(false)
        } else {
            None
        }
    })
}

fn lmdb_env_flags_from_env() -> EnvFlags {
    let mut flags = EnvFlags::empty();
    if env_bool(LMDB_NO_READ_AHEAD_ENV).unwrap_or(false) {
        flags |= EnvFlags::NO_READ_AHEAD;
    }
    if env_bool(LMDB_NO_SYNC_ENV).unwrap_or(false) {
        flags |= EnvFlags::NO_SYNC;
    }
    if env_bool(LMDB_NO_META_SYNC_ENV).unwrap_or(false) {
        flags |= EnvFlags::NO_META_SYNC;
    }
    flags
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Cached root info from Nostr events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRoot {
    /// Root hash (hex)
    pub hash: String,
    /// Optional decryption key (hex)
    pub key: Option<String>,
    /// Unix timestamp when this was cached (from event created_at)
    pub updated_at: u64,
    /// Visibility: "public", "link-visible", or "private"
    pub visibility: String,
}

/// Storage statistics
#[derive(Debug, Clone)]
pub struct LocalStoreStats {
    pub count: usize,
    pub total_bytes: u64,
}

#[derive(Default)]
struct BlobAccessUpdateGate {
    next_update_by_hash: Mutex<HashMap<Hash, u64>>,
}

impl BlobAccessUpdateGate {
    fn due_hashes<I>(&self, hashes: I, now: u64) -> Vec<Hash>
    where
        I: IntoIterator<Item = Hash>,
    {
        let Ok(mut next_update_by_hash) = self.next_update_by_hash.try_lock() else {
            return Vec::new();
        };

        if next_update_by_hash.len() >= ACCESS_UPDATE_GATE_MAX_ENTRIES {
            next_update_by_hash.retain(|_, next_update| *next_update > now);
            if next_update_by_hash.len() >= ACCESS_UPDATE_GATE_MAX_ENTRIES {
                next_update_by_hash.clear();
            }
        }

        let mut due = Vec::new();
        let mut seen = HashSet::new();
        for hash in hashes {
            if !seen.insert(hash) {
                continue;
            }
            if next_update_by_hash
                .get(&hash)
                .is_some_and(|next_update| now < *next_update)
            {
                continue;
            }
            next_update_by_hash.insert(hash, now.saturating_add(ACCESS_UPDATE_INTERVAL_SECS));
            due.push(hash);
        }
        due
    }
}

/// Local blob store - wraps either FsBlobStore or LmdbBlobStore
pub enum LocalStore {
    Fs(FsBlobStore),
    #[cfg(feature = "lmdb")]
    Lmdb(LmdbBlobStore),
    #[cfg(feature = "lmdb")]
    Pool(Box<PoolStoreWithFallbacks>),
}

/// A PoolStore with temporary exact-hash lookup fallbacks used during an
/// online migration from the former hot and legacy LMDB tiers.
///
/// PoolStore remains the sole writable and enumerable store. Fallbacks are
/// consulted only after a pool miss, so new writes stop extending the legacy
/// migration tail immediately. Explicit deletes fail closed until the
/// fallbacks are removed after migration.
#[cfg(feature = "lmdb")]
pub struct PoolStoreWithFallbacks {
    primary: PoolStore,
    fallbacks: Vec<LmdbBlobReader>,
    promotion_gates: Vec<Mutex<()>>,
}

#[cfg(feature = "lmdb")]
impl std::ops::Deref for PoolStoreWithFallbacks {
    type Target = PoolStore;

    fn deref(&self) -> &Self::Target {
        &self.primary
    }
}

#[cfg(feature = "lmdb")]
impl PoolStoreWithFallbacks {
    fn new(primary: PoolStore, fallbacks: Vec<LmdbBlobReader>) -> Self {
        Self {
            primary,
            fallbacks,
            promotion_gates: (0..64).map(|_| Mutex::new(())).collect(),
        }
    }

    fn during_migration(primary: PoolStore, fallbacks: Vec<LmdbBlobReader>) -> Self {
        if !fallbacks.is_empty() {
            tracing::warn!(
                "Blocking explicit full-store deletes until legacy PoolStore migration finishes"
            );
        }
        Self::new(primary, fallbacks)
    }

    fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let mut first_error = None;
        match self.primary.get_sync(hash) {
            Ok(Some(data)) => return Ok(Some(data)),
            Ok(None) => {}
            Err(error) => first_error = Some(error),
        }

        let stripe = usize::from(hash[0]) % self.promotion_gates.len();
        let _promotion_guard = self.promotion_gates[stripe]
            .lock()
            .map_err(|_| StoreError::Other("PoolStore promotion lock poisoned".into()))?;

        // A concurrent request may have completed the same promotion while
        // this request waited for the stripe.
        match self.primary.get_sync(hash) {
            Ok(Some(data)) => return Ok(Some(data)),
            Ok(None) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
        for fallback in &self.fallbacks {
            match fallback.get_sync(hash) {
                Ok(Some(data)) if sha256(&data) == *hash => {
                    if let Err(error) = self.primary.put_sync(*hash, &data) {
                        tracing::warn!(
                            hash = %to_hex(hash),
                            %error,
                            "Serving verified legacy blob after PoolStore promotion failed"
                        );
                    }
                    return Ok(Some(data));
                }
                Ok(Some(_)) if first_error.is_none() => {
                    first_error = Some(StoreError::Other(format!(
                        "legacy fallback returned corrupt blob {}",
                        to_hex(hash)
                    )))
                }
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }

    fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if let Ok(Some(data)) = self.primary.get_range_sync(hash, start, end_inclusive) {
            return Ok(Some(data));
        }
        self.get_sync(hash)?
            .map(|data| slice_blob_range(&data, start, end_inclusive))
            .transpose()
    }

    fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let mut first_error = None;
        match self.primary.blob_size_sync(hash) {
            Ok(Some(size)) => return Ok(Some(size)),
            Ok(None) => {}
            Err(error) => first_error = Some(error),
        }
        for fallback in &self.fallbacks {
            match fallback.blob_size_sync(hash) {
                Ok(Some(size)) => return Ok(Some(size)),
                Ok(None) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }

    fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.get_sync(hash).map(|data| data.is_some())
    }

    fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        if !self.fallbacks.is_empty() {
            return Err(StoreError::Other(POOL_MIGRATION_DELETE_DISABLED.into()));
        }
        self.primary.delete_sync(hash)
    }

    fn delete_many_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        if !self.fallbacks.is_empty() {
            return Err(StoreError::Other(POOL_MIGRATION_DELETE_DISABLED.into()));
        }
        self.primary.delete_many_sync(hashes)
    }

    fn delete_writable_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.primary.delete_sync(hash)
    }

    fn delete_many_writable_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        self.primary.delete_many_sync(hashes)
    }

    fn full_deletes_blocked(&self) -> bool {
        !self.fallbacks.is_empty()
    }
}

#[cfg(feature = "lmdb")]
fn is_fs_blob_shard_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.len() == 2 && name.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .unwrap_or(false)
}

fn lmdb_metadata_map_size_for_storage_budget(max_size_bytes: u64) -> u64 {
    if max_size_bytes == 0 {
        return LMDB_METADATA_MAX_MAP_SIZE_BYTES;
    }

    max_size_bytes
        .saturating_div(LMDB_METADATA_STORAGE_RATIO_DIVISOR)
        .clamp(
            LMDB_METADATA_MIN_MAP_SIZE_BYTES,
            LMDB_METADATA_MAX_MAP_SIZE_BYTES,
        )
}

fn lmdb_map_size_for_existing_env(path: &Path, requested_bytes: u64) -> Result<usize> {
    let existing_bytes = std::fs::metadata(path.join("data.mdb"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let requested = if existing_bytes > requested_bytes {
        let existing_headroom = existing_bytes
            .saturating_div(10)
            .max(LMDB_METADATA_REOPEN_HEADROOM_BYTES);
        existing_bytes.saturating_add(existing_headroom)
    } else {
        requested_bytes
    };
    let requested = align_lmdb_map_size(requested);
    usize::try_from(requested).context("LMDB map size exceeds usize")
}

fn align_lmdb_map_size(bytes: u64) -> u64 {
    let page_size = (page_size::get() as u64).max(4096);
    let remainder = bytes % page_size;
    if remainder == 0 {
        bytes
    } else {
        bytes.saturating_add(page_size - remainder)
    }
}

#[cfg(feature = "lmdb")]
fn remove_stale_fs_blob_shards(path: &Path) -> Result<(), StoreError> {
    let entries = std::fs::read_dir(path).map_err(StoreError::Io)?;
    for entry in entries {
        let entry = entry.map_err(StoreError::Io)?;
        let entry_path = entry.path();
        if entry_path.is_dir() && is_fs_blob_shard_dir(&entry_path) {
            std::fs::remove_dir_all(&entry_path).map_err(StoreError::Io)?;
            tracing::info!(
                "Removed stale filesystem blob shard directory after LMDB cutover: {}",
                entry_path.display()
            );
        }
    }
    Ok(())
}

#[cfg(feature = "lmdb")]
fn local_add_external_blob_reopen_options(store_path: &Path) -> ExternalBlobOptions {
    ExternalBlobOptions {
        base_path: store_path.with_file_name(LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME),
        min_bytes: usize::MAX,
        sync: true,
        pack_target_bytes: None,
    }
}

#[cfg(feature = "lmdb")]
fn external_blob_options_for(store_path: &Path) -> ExternalBlobOptions {
    ExternalBlobOptions::from_env(store_path).unwrap_or_else(|| {
        // `htree add --local` spills large blobs into this deterministic sibling
        // directory. Keep ordinary opens able to read those markers without
        // changing their write placement; local add opts into packed writes via
        // its process-local environment.
        local_add_external_blob_reopen_options(store_path)
    })
}

#[cfg(feature = "lmdb")]
fn external_blob_options_for_fallback(
    store_path: &Path,
    external_dir_env: &str,
) -> ExternalBlobOptions {
    let options = external_blob_options_for(store_path);
    std::env::var(external_dir_env)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| options.clone().with_base_path(path))
        .unwrap_or(options)
}

#[cfg(feature = "lmdb")]
fn paths_refer_to_same_location(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(feature = "lmdb")]
fn open_pool_fallbacks(data_dir: &Path) -> Result<Vec<LmdbBlobReader>, StoreError> {
    let canonical_legacy = data_dir.join("blobs");
    let configured_legacy = std::env::var(LMDB_HOT_BLOB_LEGACY_DIR_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let Some(configured_legacy) = configured_legacy else {
        return Ok(Vec::new());
    };
    if !paths_refer_to_same_location(&canonical_legacy, &configured_legacy) {
        tracing::warn!(
            expected = %canonical_legacy.display(),
            configured = %configured_legacy.display(),
            "Ignoring PoolStore fallbacks because the legacy tier guard does not match"
        );
        return Ok(Vec::new());
    }

    let hot = std::env::var(LMDB_HOT_BLOB_DIR_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let candidates = [
        hot.map(|path| (path, LMDB_HOT_EXTERNAL_BLOB_DIR_ENV)),
        Some((configured_legacy, LMDB_LEGACY_EXTERNAL_BLOB_DIR_ENV)),
    ];
    let mut fallbacks = Vec::new();
    let mut opened = HashSet::new();
    for candidate in candidates.into_iter().flatten() {
        let (path, external_dir_env) = candidate;
        if !path.join("data.mdb").exists() {
            continue;
        }
        let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !opened.insert(identity) {
            continue;
        }
        let external = external_blob_options_for_fallback(&path, external_dir_env);
        let store = LmdbBlobReader::open(&path, Some(external))?;
        tracing::info!(
            path = %path.display(),
            external_dir_env,
            "Enabled read-only exact-hash LMDB fallback behind writable PoolStore"
        );
        fallbacks.push(store);
    }
    Ok(fallbacks)
}

#[cfg(feature = "lmdb")]
fn open_lmdb_blob_store<P: AsRef<Path>>(
    path: P,
    map_size_bytes: Option<u64>,
) -> Result<LmdbBlobStore, StoreError> {
    std::fs::create_dir_all(path.as_ref()).map_err(StoreError::Io)?;
    remove_stale_fs_blob_shards(path.as_ref())?;
    let external_blobs = Some(external_blob_options_for(path.as_ref()));
    match map_size_bytes {
        Some(map_size_bytes) => {
            LmdbBlobStore::with_max_bytes_and_external_blob_options(path, map_size_bytes, |_| {
                external_blobs
            })
        }
        None => LmdbBlobStore::with_external_blob_options(path, external_blobs),
    }
}

impl LocalStore {
    /// Create a new unbounded local store.
    ///
    /// Higher-level stores that need quota enforcement should manage eviction
    /// above this layer so tree metadata, pins, and archival policies stay
    /// coherent.
    pub fn new<P: AsRef<Path>>(path: P, backend: &StorageBackend) -> Result<Self, StoreError> {
        Self::new_unbounded(path, backend)
    }

    /// Create a new local store with an explicit LMDB logical size cap when using the LMDB backend.
    ///
    /// The requested size is used for both the LMDB map and the store's built-in
    /// byte quota, so standalone blob envs can evict instead of growing forever.
    pub fn new_with_lmdb_map_size<P: AsRef<Path>>(
        path: P,
        backend: &StorageBackend,
        _map_size_bytes: Option<u64>,
    ) -> Result<Self, StoreError> {
        match backend {
            StorageBackend::Fs => Ok(LocalStore::Fs(FsBlobStore::new(path)?)),
            #[cfg(feature = "lmdb")]
            StorageBackend::Lmdb => Ok(LocalStore::Lmdb(open_lmdb_blob_store(
                path,
                _map_size_bytes,
            )?)),
            #[cfg(not(feature = "lmdb"))]
            StorageBackend::Lmdb => {
                tracing::warn!(
                    "LMDB backend requested but lmdb feature not enabled, using filesystem storage"
                );
                Ok(LocalStore::Fs(FsBlobStore::new(path)?))
            }
        }
    }

    /// Create a new unbounded local store for a specific backend.
    pub fn new_unbounded<P: AsRef<Path>>(
        path: P,
        backend: &StorageBackend,
    ) -> Result<Self, StoreError> {
        Self::new_with_lmdb_map_size(path, backend, None)
    }

    /// Create a local store with an explicit LMDB map size but without adapter-level eviction.
    ///
    /// Higher layers use this when they need a large mmap while enforcing quota
    /// with richer retention policy.
    pub fn new_unbounded_with_lmdb_map_size<P: AsRef<Path>>(
        path: P,
        backend: &StorageBackend,
        _map_size_bytes: Option<u64>,
    ) -> Result<Self, StoreError> {
        match backend {
            StorageBackend::Fs => Ok(LocalStore::Fs(FsBlobStore::new(path)?)),
            #[cfg(feature = "lmdb")]
            StorageBackend::Lmdb => Ok(
                match open_configured_lmdb_blob_store(path, _map_size_bytes)? {
                    ConfiguredLmdbBlobStore::Single(store) => LocalStore::Lmdb(store),
                    ConfiguredLmdbBlobStore::Pool(store) => {
                        LocalStore::Pool(Box::new(PoolStoreWithFallbacks::new(*store, Vec::new())))
                    }
                },
            ),
            #[cfg(not(feature = "lmdb"))]
            StorageBackend::Lmdb => {
                tracing::warn!(
                    "LMDB backend requested but lmdb feature not enabled, using filesystem storage"
                );
                Ok(LocalStore::Fs(FsBlobStore::new(path)?))
            }
        }
    }

    pub fn backend(&self) -> StorageBackend {
        match self {
            LocalStore::Fs(_) => StorageBackend::Fs,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(_) | LocalStore::Pool(_) => StorageBackend::Lmdb,
        }
    }

    pub fn force_sync(&self) -> Result<(), StoreError> {
        match self {
            LocalStore::Fs(_) => Ok(()),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.force_sync(),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.force_sync(),
        }
    }

    /// Sync put operation
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.put_sync(hash, data),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.put_sync(hash, data),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.put_sync(hash, data),
        }
    }

    /// Sync batch put operation.
    pub fn put_many_report_sync(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> Result<PutManyReport, StoreError> {
        match self {
            LocalStore::Fs(store) => {
                let mut report = PutManyReport {
                    total: items.len(),
                    ..PutManyReport::default()
                };
                for (hash, data) in items {
                    if store.put_sync(*hash, data.as_slice())? {
                        report.inserted = report.inserted.saturating_add(1);
                        report.inserted_bytes =
                            report.inserted_bytes.saturating_add(data.len() as u64);
                        report.inserted_hashes.push(*hash);
                    }
                }
                Ok(report)
            }
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.put_many_report_sync(items),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.put_many_report_sync(items),
        }
    }

    /// Sync batch put operation.
    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_report_sync(items)
            .map(|report| report.inserted)
    }

    /// Sync batch put for locally generated content-addressed candidates.
    pub fn put_many_optimistic_report_sync(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> Result<PutManyReport, StoreError> {
        match self {
            LocalStore::Fs(_) => self.put_many_report_sync(items),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(_) => self.put_many_report_sync(items),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.put_many_optimistic_report_sync(items),
        }
    }

    pub fn put_many_optimistic_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_optimistic_report_sync(items)
            .map(|report| report.inserted)
    }

    /// Sync get operation
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.get_sync(hash),
        }
    }

    pub fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get_range_sync(hash, start, end_inclusive),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get_range_sync(hash, start, end_inclusive),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.get_range_sync(hash, start, end_inclusive),
        }
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.blob_size_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.blob_size_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.blob_size_sync(hash),
        }
    }

    pub fn touch_accessed_sync(&self, hash: &Hash, now: u64) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.touch_accessed_sync(hash, now),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.touch_accessed_sync(hash, now),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.touch_accessed_sync(hash, now),
        }
    }

    pub fn touch_many_accessed_sync(&self, hashes: &[Hash], now: u64) -> Result<usize, StoreError> {
        match self {
            LocalStore::Fs(store) => store.touch_many_accessed_sync(hashes, now),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.touch_many_accessed_sync(hashes, now),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.touch_many_accessed_sync(hashes, now),
        }
    }

    pub fn last_accessed_at_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.last_accessed_at_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.last_accessed_at_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.last_accessed_at_sync(hash),
        }
    }

    pub fn many_last_accessed_at_sync(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<(Hash, u64)>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.many_last_accessed_at_sync(hashes),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.many_last_accessed_at_sync(hashes),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.many_last_accessed_at_sync(hashes),
        }
    }

    /// Check if hash exists
    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => Ok(store.exists(hash)),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.exists(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.exists(hash),
        }
    }

    /// Mark which sorted hashes already exist in local storage.
    pub fn existing_hashes_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        match self {
            LocalStore::Fs(store) => Ok(sorted_hashes
                .iter()
                .map(|hash| store.exists(hash))
                .collect()),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.existing_hashes_in_sorted_candidates(sorted_hashes),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.existing_hashes_in_sorted_candidates(sorted_hashes),
        }
    }

    /// Sync delete operation
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.delete_sync(hash),
        }
    }

    pub fn delete_many_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        match self {
            LocalStore::Fs(store) => {
                let mut deleted = 0usize;
                for hash in hashes {
                    if store.delete_sync(hash)? {
                        deleted += 1;
                    }
                }
                Ok(deleted)
            }
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete_many_sync(hashes),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.delete_many_sync(hashes),
        }
    }

    pub fn delete_writable_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.delete_writable_sync(hash),
        }
    }

    pub fn delete_many_writable_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        match self {
            LocalStore::Fs(store) => {
                let mut deleted = 0usize;
                for hash in hashes {
                    if store.delete_sync(hash)? {
                        deleted += 1;
                    }
                }
                Ok(deleted)
            }
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete_many_sync(hashes),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.delete_many_writable_sync(hashes),
        }
    }

    pub fn full_deletes_blocked(&self) -> bool {
        match self {
            LocalStore::Fs(_) => false,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(_) => false,
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.full_deletes_blocked(),
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
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => {
                let stats = store.stats()?;
                Ok(LocalStoreStats {
                    count: stats.count as usize,
                    total_bytes: stats.bytes,
                })
            }
        }
    }

    /// Get storage statistics for the canonical writable store.
    pub fn writable_stats(&self) -> Result<LocalStoreStats, StoreError> {
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
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => {
                let stats = store.stats()?;
                Ok(LocalStoreStats {
                    count: stats.count as usize,
                    total_bytes: stats.bytes,
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
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.list(),
        }
    }

    /// List hashes in the canonical writable store.
    pub fn list_writable(&self) -> Result<Vec<Hash>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.list(),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.list(),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(store) => store.list(),
        }
    }

    /// Scan the canonical writable store in bounded lexicographic pages.
    ///
    /// Quota cleanup must fail closed when the backend cannot distinguish a
    /// disposable writable cache copy from durable data. In particular,
    /// `PoolStore::delete_sync` removes every member copy and its catalog
    /// location, so treating the whole pool catalog as a hot-cache scan would
    /// be unsafe.
    pub fn scan_writable_hashes_after(
        &self,
        after: Option<Hash>,
        limit: usize,
    ) -> Result<Vec<Hash>, StoreError> {
        match self {
            // Preserve embedded/filesystem quota behavior. The filesystem
            // backend has no native cursor yet, so this compatibility path
            // still enumerates once and only bounds the retention work done by
            // the caller. LMDB below is the scalable production path.
            LocalStore::Fs(store) => {
                let mut hashes = store.list()?;
                hashes.sort_unstable();
                let start = after
                    .map(|after| hashes.partition_point(|hash| *hash <= after))
                    .unwrap_or(0);
                hashes.truncate(start.saturating_add(limit).min(hashes.len()));
                Ok(hashes.drain(start..).collect())
            }
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.scan_hashes_after(after, limit),
            #[cfg(feature = "lmdb")]
            LocalStore::Pool(_) => Err(StoreError::Other(
                "bounded orphan cleanup is unsupported for PoolStore without tier-aware deletion"
                    .into(),
            )),
        }
    }
}

#[async_trait]
impl Store for LocalStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_sync(&items)
    }

    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_optimistic_sync(&items)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_range_sync(hash, start, end_inclusive)
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.blob_size_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.delete_sync(hash)
    }

    async fn delete_many(&self, hashes: Vec<Hash>) -> Result<usize, StoreError> {
        self.delete_many_sync(&hashes)
    }
}

fn open_local_blob_store_with_options<P: AsRef<Path>>(
    data_dir: P,
    backend: &StorageBackend,
    max_size_bytes: u64,
) -> Result<Arc<LocalStore>, StoreError> {
    #[cfg(feature = "lmdb")]
    if *backend == StorageBackend::Lmdb {
        let data_dir = data_dir.as_ref();
        return open_shared_lmdb_blob_store(data_dir, max_size_bytes).and_then(|store| {
            let local = match store {
                ConfiguredLmdbBlobStore::Single(store) => LocalStore::Lmdb(store),
                ConfiguredLmdbBlobStore::Pool(store) => {
                    let fallbacks = open_pool_fallbacks(data_dir)?;
                    LocalStore::Pool(Box::new(PoolStoreWithFallbacks::during_migration(
                        *store, fallbacks,
                    )))
                }
            };
            Ok(Arc::new(local))
        });
    }

    #[cfg(not(feature = "lmdb"))]
    let _ = max_size_bytes;

    LocalStore::new_unbounded(data_dir.as_ref().join("blobs"), backend).map(Arc::new)
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
    #[cfg(feature = "s3")]
    fn s3_sync_timeout() -> std::time::Duration {
        let millis = std::env::var(S3_SYNC_TIMEOUT_MS_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_S3_SYNC_TIMEOUT_MS);
        std::time::Duration::from_millis(millis)
    }

    #[cfg(feature = "s3")]
    fn s3_sync_timeout_error(timeout: std::time::Duration) -> StoreError {
        StoreError::Other(format!(
            "S3 sync operation timed out after {}ms",
            timeout.as_millis()
        ))
    }

    #[cfg(feature = "s3")]
    fn run_s3_future_sync<F, T>(future: F) -> Result<T, StoreError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let timeout = Self::s3_sync_timeout();
        if tokio::runtime::Handle::try_current().is_ok() {
            return std::thread::Builder::new()
                .name("storage-s3-sync".to_string())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| {
                            StoreError::Other(format!("build storage s3 sync runtime: {err}"))
                        })?;
                    runtime.block_on(async move {
                        tokio::time::timeout(timeout, future)
                            .await
                            .map_err(|_| Self::s3_sync_timeout_error(timeout))
                    })
                })
                .map_err(|err| StoreError::Other(format!("spawn S3 sync helper thread: {err}")))?
                .join()
                .map_err(|_| StoreError::Other("S3 sync helper thread panicked".to_string()))?;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| StoreError::Other(format!("build storage s3 sync runtime: {err}")))?;
        runtime.block_on(async move {
            tokio::time::timeout(timeout, future)
                .await
                .map_err(|_| Self::s3_sync_timeout_error(timeout))
        })
    }

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

    pub fn force_sync(&self) -> Result<(), StoreError> {
        self.local.force_sync()
    }

    /// Create router with local storage + S3 backup
    #[cfg(feature = "s3")]
    pub async fn with_s3(local: Arc<LocalStore>, config: &S3Config) -> Result<Self, anyhow::Error> {
        use aws_sdk_s3::Client as S3Client;

        // Build AWS config
        let mut aws_config_loader = aws_config::from_env();
        aws_config_loader =
            aws_config_loader.region(aws_sdk_s3::config::Region::new(config.region.clone()));
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

            // Keep S3 writes parallel, but avoid dispatch failures during mirror backfill bursts.
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
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

                            let mut attempt = 1u8;
                            loop {
                                match client
                                    .put_object()
                                    .bucket(bucket.as_str())
                                    .key(&key)
                                    .body(ByteStream::from(data.clone()))
                                    .send()
                                    .await
                                {
                                    Ok(_) => {
                                        tracing::debug!("S3 upload succeeded: {}", &key);
                                        break;
                                    }
                                    Err(e) if attempt < 3 => {
                                        tracing::warn!(
                                            "S3 upload retrying {}: attempt={} error={}",
                                            &key,
                                            attempt,
                                            e
                                        );
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            250 * u64::from(attempt),
                                        ))
                                        .await;
                                        attempt += 1;
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "S3 upload failed {} after {} attempts: {}",
                                            &key,
                                            attempt,
                                            e
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                        S3SyncMessage::Delete { hash } => {
                            let key = format!("{}{}.bin", prefix, to_hex(&hash));
                            tracing::debug!("S3 deleting {}", &key);

                            let mut attempt = 1u8;
                            loop {
                                match client
                                    .delete_object()
                                    .bucket(bucket.as_str())
                                    .key(&key)
                                    .send()
                                    .await
                                {
                                    Ok(_) => break,
                                    Err(e) if attempt < 3 => {
                                        tracing::warn!(
                                            "S3 delete retrying {}: attempt={} error={}",
                                            &key,
                                            attempt,
                                            e
                                        );
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            250 * u64::from(attempt),
                                        ))
                                        .await;
                                        attempt += 1;
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "S3 delete failed {} after {} attempts: {}",
                                            &key,
                                            attempt,
                                            e
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });

        tracing::info!(
            "S3 storage initialized: bucket={}, prefix={}",
            bucket,
            prefix
        );

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

        // Queue S3 upload only for newly inserted blobs.
        // Existing local blobs were already persisted or are handled by explicit repair/push flows.
        #[cfg(feature = "s3")]
        if is_new {
            if let Some(ref tx) = self.sync_tx {
                tracing::debug!(
                    "Queueing S3 upload for {} ({} bytes)",
                    crate::storage::to_hex(&hash)[..16].to_string(),
                    data.len(),
                );
                if let Err(e) = tx.send(S3SyncMessage::Upload {
                    hash,
                    data: data.to_vec(),
                }) {
                    tracing::error!("Failed to queue S3 upload: {}", e);
                }
            }
        }

        Ok(is_new)
    }

    /// Store multiple blobs with a single local batch write when supported.
    pub fn put_many_report_sync(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> Result<PutManyReport, StoreError> {
        let report = self.local.put_many_report_sync(items)?;

        #[cfg(feature = "s3")]
        if let Some(ref tx) = self.sync_tx {
            if !report.inserted_hashes.is_empty() {
                let inserted: HashSet<Hash> = report.inserted_hashes.iter().copied().collect();
                let mut queued = HashSet::new();
                for (hash, data) in items {
                    if inserted.contains(hash) && queued.insert(*hash) {
                        if let Err(e) = tx.send(S3SyncMessage::Upload {
                            hash: *hash,
                            data: data.clone(),
                        }) {
                            tracing::error!("Failed to queue S3 upload: {}", e);
                        }
                    }
                }
            }
        }

        Ok(report)
    }

    /// Store multiple blobs with a single local batch write when supported.
    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_report_sync(items)
            .map(|report| report.inserted)
    }

    /// Store a locally generated content-addressed batch without rereading
    /// committed PoolStore candidates that are already catalogued.
    pub fn put_many_optimistic_report_sync(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> Result<PutManyReport, StoreError> {
        let report = self.local.put_many_optimistic_report_sync(items)?;

        #[cfg(feature = "s3")]
        if let Some(ref tx) = self.sync_tx {
            if !report.inserted_hashes.is_empty() {
                let inserted: HashSet<Hash> = report.inserted_hashes.iter().copied().collect();
                let mut queued = HashSet::new();
                for (hash, data) in items {
                    if inserted.contains(hash) && queued.insert(*hash) {
                        if let Err(e) = tx.send(S3SyncMessage::Upload {
                            hash: *hash,
                            data: data.clone(),
                        }) {
                            tracing::error!("Failed to queue S3 upload: {}", e);
                        }
                    }
                }
            }
        }

        Ok(report)
    }

    pub fn put_many_optimistic_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_optimistic_report_sync(items)
            .map(|report| report.inserted)
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
            let client = client.clone();
            let bucket = bucket.clone();

            match Self::run_s3_future_sync(async move {
                client.get_object().bucket(bucket).key(key).send().await
            }) {
                Ok(Ok(output)) => {
                    match Self::run_s3_future_sync(async move { output.body.collect().await }) {
                        Ok(Ok(body)) => {
                            let data = body.into_bytes().to_vec();
                            // Cache locally for future reads
                            let _ = self.local.put_sync(*hash, &data);
                            return Ok(Some(data));
                        }
                        Ok(Err(err)) => {
                            tracing::warn!("S3 body collect failed: {}", err);
                        }
                        Err(err) => {
                            tracing::warn!("S3 body collect runtime failed: {}", err);
                        }
                    }
                }
                Ok(Err(err)) => {
                    let service_err = err.into_service_error();
                    if !service_err.is_no_such_key() {
                        tracing::warn!("S3 get failed: {}", service_err);
                    }
                }
                Err(err) => {
                    tracing::warn!("S3 get runtime failed: {}", err);
                }
            }
        }

        Ok(None)
    }

    pub fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.local.get_range_sync(hash, start, end_inclusive)
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.local.blob_size_sync(hash)
    }

    pub fn touch_accessed_sync(&self, hash: &Hash, now: u64) -> Result<bool, StoreError> {
        self.local.touch_accessed_sync(hash, now)
    }

    pub fn touch_many_accessed_sync(&self, hashes: &[Hash], now: u64) -> Result<usize, StoreError> {
        self.local.touch_many_accessed_sync(hashes, now)
    }

    pub fn last_accessed_at_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.local.last_accessed_at_sync(hash)
    }

    pub fn many_last_accessed_at_sync(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<(Hash, u64)>, StoreError> {
        self.local.many_last_accessed_at_sync(hashes)
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
            let client = client.clone();
            let bucket = bucket.clone();

            match Self::run_s3_future_sync(async move {
                client.head_object().bucket(bucket).key(&key).send().await
            }) {
                Ok(Ok(_)) => return Ok(true),
                Ok(Err(err)) => {
                    let service_err = err.into_service_error();
                    if !service_err.is_not_found() {
                        tracing::warn!("S3 head failed: {}", service_err);
                    }
                }
                Err(err) => {
                    tracing::warn!("S3 head runtime failed: {}", err);
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

    pub fn delete_many_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        let deleted = self.local.delete_many_sync(hashes)?;

        #[cfg(feature = "s3")]
        if let Some(ref tx) = self.sync_tx {
            for hash in hashes {
                let _ = tx.send(S3SyncMessage::Delete { hash: *hash });
            }
        }

        Ok(deleted)
    }

    /// Delete data from local store only (don't propagate to S3)
    /// Used for eviction where we want to keep archives and cold tiers intact.
    pub fn delete_local_only(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local.delete_writable_sync(hash)
    }

    pub fn delete_many_local_only(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        self.local.delete_many_writable_sync(hashes)
    }

    pub fn full_deletes_blocked(&self) -> bool {
        self.local.full_deletes_blocked()
    }

    /// Get stats from local store
    pub fn stats(&self) -> Result<LocalStoreStats, StoreError> {
        self.local.stats()
    }

    /// Get stats for the writable local tier used for quota and eviction pressure.
    pub fn writable_stats(&self) -> Result<LocalStoreStats, StoreError> {
        self.local.writable_stats()
    }

    /// List all hashes from local store
    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        self.local.list()
    }

    /// List hashes from the writable local tier used for quota and eviction pressure.
    pub fn list_writable(&self) -> Result<Vec<Hash>, StoreError> {
        self.local.list_writable()
    }

    /// Scan hashes from the writable local tier without materializing it.
    pub fn scan_writable_hashes_after(
        &self,
        after: Option<Hash>,
        limit: usize,
    ) -> Result<Vec<Hash>, StoreError> {
        self.local.scan_writable_hashes_after(after, limit)
    }

    /// Mark which sorted candidate hashes already exist in local storage.
    pub fn existing_local_hashes_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        self.local
            .existing_hashes_in_sorted_candidates(sorted_hashes)
    }

    /// Get the underlying local store for HashTree operations
    pub fn local_store(&self) -> Arc<LocalStore> {
        Arc::clone(&self.local)
    }
}

#[derive(Clone)]
struct AccessRecordingStore {
    inner: Arc<StorageRouter>,
    accessed: Arc<Mutex<HashSet<Hash>>>,
}

impl AccessRecordingStore {
    fn new(inner: Arc<StorageRouter>) -> Self {
        Self {
            inner,
            accessed: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn take_accessed_hashes(&self) -> Vec<Hash> {
        let Ok(mut accessed) = self.accessed.lock() else {
            return Vec::new();
        };
        accessed.drain().collect()
    }

    fn record_access(&self, hash: &Hash) {
        let Ok(mut accessed) = self.accessed.lock() else {
            return;
        };
        accessed.insert(*hash);
    }
}

#[async_trait]
impl Store for AccessRecordingStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.inner.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let data = self.inner.get(hash).await?;
        if data.is_some() {
            self.record_access(hash);
        }
        Ok(data)
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let data = self.inner.get_range(hash, start, end_inclusive).await?;
        if data.is_some() {
            self.record_access(hash);
        }
        Ok(data)
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.inner.blob_size(hash).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }

    async fn delete_many(&self, hashes: Vec<Hash>) -> Result<usize, StoreError> {
        self.inner.delete_many(hashes).await
    }
}

// Implement async Store trait for StorageRouter so it can be used directly with HashTree
// This ensures all writes go through S3 sync
#[async_trait]
impl Store for StorageRouter {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_sync(&items)
    }

    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_optimistic_sync(&items)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(data) = self.get_range_sync(hash, start, end_inclusive)? {
            return Ok(Some(data));
        }
        let Some(data) = self.get_sync(hash)? else {
            return Ok(None);
        };
        Ok(Some(slice_blob_range(&data, start, end_inclusive)?))
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        if let Some(size) = self.blob_size_sync(hash)? {
            return Ok(Some(size));
        }
        Ok(self.get_sync(hash)?.map(|data| data.len() as u64))
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.delete_sync(hash)
    }

    async fn delete_many(&self, hashes: Vec<Hash>) -> Result<usize, StoreError> {
        self.delete_many_sync(&hashes)
    }
}

#[derive(Debug, Clone, Copy)]
struct OrphanSweep {
    /// Cursor immediately before this sweep began. A non-`None` sweep wraps at
    /// the end of the keyspace and finishes once it reaches this boundary.
    start_after: Option<Hash>,
    wrapped: bool,
}

#[derive(Debug, Default)]
struct OrphanScanState {
    /// Last candidate examined. It need not still exist after cleanup.
    cursor: Option<Hash>,
    sweep: Option<OrphanSweep>,
    socialgraph_roots: Option<[Option<Hash>; 4]>,
    socialgraph_protected: Arc<HashSet<Hash>>,
}

pub struct HashtreeStore {
    base_path: PathBuf,
    env: ManagedEnv,
    /// Set of pinned hashes (32-byte raw hashes, prevents garbage collection)
    pins: Database<Bytes, Unit>,
    /// Mutable published refs that should stay subscribed and keep following updates
    pinned_refs: Database<Str, Unit>,
    /// Authors whose hashtree publications should be mirrored continuously
    tracked_authors: Database<Str, Unit>,
    /// Blob ownership: sha256 (32 bytes) ++ pubkey (32 bytes) -> () (composite key for multi-owner)
    blob_owners: Database<Bytes, Unit>,
    /// Maps pubkey (32 bytes) -> blob metadata JSON (for blossom list)
    pubkey_blobs: Database<Bytes, Bytes>,
    /// Pubkey listing index: pubkey (32 bytes) ++ sha256 (32 bytes) -> BlobMetadata JSON
    pubkey_blob_index: Database<Bytes, Bytes>,
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
    /// Whether quota enforcement may delete local blobs not tracked by any indexed tree.
    evict_orphans: bool,
    /// Coalesces retention cleanup, accounts cache writes, and protects durable metadata gaps.
    cache_quota: CacheQuotaController,
    /// Resumable bounded orphan scan and its per-sweep socialgraph protection.
    orphan_scan: Mutex<OrphanScanState>,
    /// Best-effort in-memory throttle for blob access metadata writes.
    blob_access_update_gate: BlobAccessUpdateGate,
    /// Keeps access-time maintenance out of foreground blob reads.
    blob_access_update_inflight: Arc<AtomicBool>,
    /// Immutable file chunk metadata cache for hot range-read workloads.
    file_metadata_cache: Mutex<LruCache<Hash, Arc<FileChunkMetadata>>>,
}

impl HashtreeStore {
    /// Create a new store with the configured local storage limit.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let config = hashtree_config::Config::load_or_default();
        let max_size_bytes = config
            .storage
            .max_size_gb
            .saturating_mul(1024 * 1024 * 1024);
        Self::with_options_and_backend(
            path,
            None,
            max_size_bytes,
            config.storage.evict_orphans,
            &config.storage.backend,
        )
    }

    /// Create a new store with an explicit local backend and size limit.
    pub fn new_with_backend<P: AsRef<Path>>(
        path: P,
        backend: hashtree_config::StorageBackend,
        max_size_bytes: u64,
    ) -> Result<Self> {
        Self::with_options_and_backend(path, None, max_size_bytes, true, &backend)
    }

    /// Create a new store with optional S3 backend and the configured local storage limit.
    pub fn with_s3<P: AsRef<Path>>(path: P, s3_config: Option<&S3Config>) -> Result<Self> {
        let config = hashtree_config::Config::load_or_default();
        let max_size_bytes = config
            .storage
            .max_size_gb
            .saturating_mul(1024 * 1024 * 1024);
        Self::with_options_and_backend(
            path,
            s3_config,
            max_size_bytes,
            config.storage.evict_orphans,
            &config.storage.backend,
        )
    }

    /// Create a new store with optional S3 backend and custom size limit.
    ///
    /// The raw local blob backend remains unbounded. `HashtreeStore` enforces
    /// `max_size_bytes` at the tree-management layer so eviction can honor pins,
    /// orphan handling, and local-only eviction when S3 is used as archive.
    pub fn with_options<P: AsRef<Path>>(
        path: P,
        s3_config: Option<&S3Config>,
        max_size_bytes: u64,
    ) -> Result<Self> {
        let config = hashtree_config::Config::load_or_default();
        Self::with_options_and_backend(
            path,
            s3_config,
            max_size_bytes,
            config.storage.evict_orphans,
            &config.storage.backend,
        )
    }

    pub fn with_options_and_backend<P: AsRef<Path>>(
        path: P,
        s3_config: Option<&S3Config>,
        max_size_bytes: u64,
        evict_orphans: bool,
        backend: &hashtree_config::StorageBackend,
    ) -> Result<Self> {
        Self::with_options_and_backend_and_env_flags(
            path,
            s3_config,
            max_size_bytes,
            evict_orphans,
            backend,
            EnvFlags::empty(),
        )
    }

    /// Create a store for an embedded, single-process hashtree host.
    ///
    /// The macOS app sandbox denies LMDB's default System V semaphore locks.
    /// The embedded host owns this data directory in one process, so it uses
    /// external process isolation plus LMDB `NO_LOCK` for metadata and the
    /// filesystem blob backend to avoid opening a second LMDB environment.
    pub fn with_embedded_options<P: AsRef<Path>>(
        path: P,
        s3_config: Option<&S3Config>,
        max_size_bytes: u64,
    ) -> Result<Self> {
        Self::with_options_and_backend_and_env_flags(
            path,
            s3_config,
            max_size_bytes,
            true,
            &hashtree_config::StorageBackend::Fs,
            EnvFlags::NO_LOCK,
        )
    }

    fn with_options_and_backend_and_env_flags<P: AsRef<Path>>(
        path: P,
        s3_config: Option<&S3Config>,
        max_size_bytes: u64,
        evict_orphans: bool,
        backend: &hashtree_config::StorageBackend,
        env_flags: EnvFlags,
    ) -> Result<Self> {
        let env_flags = env_flags | lmdb_env_flags_from_env();
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let metadata_map_size = lmdb_map_size_for_existing_env(
            path,
            lmdb_metadata_map_size_for_storage_budget(max_size_bytes),
        )?;

        let mut env_options = EnvOpenOptions::new();
        env_options
            .map_size(metadata_map_size)
            .max_dbs(11) // pins, pinned_refs, tracked_authors, blob_owners, pubkey_blobs, pubkey_blob_index, tree_meta, blob_trees, tree_refs, cached_roots, blobs
            .max_readers(LMDB_MAX_READERS);
        unsafe {
            env_options.flags(env_flags);
        }
        let env = unsafe { ManagedEnv::open(&env_options, path)? };
        let _ = env.clear_stale_readers();
        if env.info().map_size < metadata_map_size {
            unsafe { env.resize(metadata_map_size) }?;
        }

        let mut wtxn = env.write_txn()?;
        let pins = env.create_database(&mut wtxn, Some("pins"))?;
        let pinned_refs = env.create_database(&mut wtxn, Some("pinned_refs"))?;
        let tracked_authors = env.create_database(&mut wtxn, Some("tracked_authors"))?;
        let blob_owners = env.create_database(&mut wtxn, Some("blob_owners"))?;
        let pubkey_blobs = env.create_database(&mut wtxn, Some("pubkey_blobs"))?;
        let pubkey_blob_index = env.create_database(&mut wtxn, Some("pubkey_blob_index"))?;
        let tree_meta = env.create_database(&mut wtxn, Some("tree_meta"))?;
        let blob_trees = env.create_database(&mut wtxn, Some("blob_trees"))?;
        let tree_refs = env.create_database(&mut wtxn, Some("tree_refs"))?;
        let cached_roots = env.create_database(&mut wtxn, Some("cached_roots"))?;
        wtxn.commit()?;

        // Intentionally keep the raw blob backend unbounded here. HashtreeStore
        // owns quota policy above this layer, where it can coordinate eviction
        // with tree refs, blob ownership, pins, and S3 archival behavior.
        let local_store = open_local_blob_store_with_options(path, backend, max_size_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to create blob store: {}", e))?;

        // Create storage router with optional S3
        #[cfg(feature = "s3")]
        let router = Arc::new(if let Some(s3_cfg) = s3_config {
            tracing::info!(
                "Initializing S3 storage backend: bucket={}, endpoint={}",
                s3_cfg.bucket,
                s3_cfg.endpoint
            );

            sync_block_on(async { StorageRouter::with_s3(local_store, s3_cfg).await })?
        } else {
            StorageRouter::new(local_store)
        });

        #[cfg(not(feature = "s3"))]
        let router = Arc::new({
            if s3_config.is_some() {
                tracing::warn!(
                    "S3 config provided but S3 feature not enabled. Using local storage only."
                );
            }
            StorageRouter::new(local_store)
        });

        Ok(Self {
            base_path: path.to_path_buf(),
            env,
            pins,
            pinned_refs,
            tracked_authors,
            blob_owners,
            pubkey_blobs,
            pubkey_blob_index,
            tree_meta,
            blob_trees,
            tree_refs,
            cached_roots,
            router,
            max_size_bytes,
            evict_orphans,
            cache_quota: CacheQuotaController::default(),
            orphan_scan: Mutex::new(OrphanScanState::default()),
            blob_access_update_gate: BlobAccessUpdateGate::default(),
            blob_access_update_inflight: Arc::new(AtomicBool::new(false)),
            file_metadata_cache: Mutex::new(LruCache::new(file_metadata_cache_entries())),
        })
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
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

    pub fn force_sync(&self) -> Result<()> {
        self.env.force_sync()?;
        self.router
            .force_sync()
            .map_err(|err| anyhow::anyhow!("Failed to sync blob store: {}", err))
    }

    fn access_tracking_tree(&self) -> (HashTree<AccessRecordingStore>, AccessRecordingStore) {
        let access_store = AccessRecordingStore::new(self.store_arc());
        let tree = HashTree::new(HashTreeConfig::new(Arc::new(access_store.clone())).public());
        (tree, access_store)
    }

    pub fn record_blob_accesses<I>(&self, hashes: I)
    where
        I: IntoIterator<Item = Hash>,
    {
        let access_update_batch_limit = access_update_background_batch_limit();
        if access_update_batch_limit == 0 {
            return;
        }

        let now = unix_timestamp_now();
        let mut due_hashes = self.blob_access_update_gate.due_hashes(hashes, now);
        if due_hashes.is_empty() {
            return;
        }

        if self
            .blob_access_update_inflight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        if due_hashes.len() > access_update_batch_limit {
            due_hashes.truncate(access_update_batch_limit);
        }

        let router = Arc::clone(&self.router);
        let inflight = Arc::clone(&self.blob_access_update_inflight);
        let spawn_result = std::thread::Builder::new()
            .name("blob-access-update".to_string())
            .spawn(move || {
                if let Err(err) = router.touch_many_accessed_sync(&due_hashes, now) {
                    tracing::debug!("Failed to update blob access metadata: {}", err);
                }
                inflight.store(false, Ordering::Release);
            });
        if let Err(err) = spawn_result {
            self.blob_access_update_inflight
                .store(false, Ordering::Release);
            tracing::debug!("Failed to spawn blob access metadata updater: {}", err);
        }
    }

    pub fn blob_last_accessed_at(&self, hash: &Hash) -> Result<Option<u64>> {
        self.router
            .last_accessed_at_sync(hash)
            .map_err(|e| anyhow::anyhow!("Failed to read blob access metadata: {}", e))
    }

    pub fn blob_last_accessed_many(&self, hashes: &[Hash]) -> Result<Vec<(Hash, u64)>> {
        self.router
            .many_last_accessed_at_sync(hashes)
            .map_err(|e| anyhow::anyhow!("Failed to read blob access metadata: {}", e))
    }

    /// Get tree node by hash (raw bytes)
    pub fn get_tree_node(&self, hash: &[u8; 32]) -> Result<Option<TreeNode>> {
        let (tree, access_store) = self.access_tracking_tree();

        let result = sync_block_on(async {
            tree.get_tree_node(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))
        })?;
        if result.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        Ok(result)
    }

    /// Store a raw blob, returns SHA256 hash as hex.
    pub fn put_blob(&self, data: &[u8]) -> Result<String> {
        let hash = sha256(data);
        self.router
            .put_sync(hash, data)
            .map_err(|e| anyhow::anyhow!("Failed to store blob: {}", e))?;
        Ok(to_hex(&hash))
    }

    /// Store an owned Blossom blob under the configured durable storage limit.
    pub fn put_owned_blob_with_inserted(
        &self,
        data: &[u8],
        pubkey: &[u8; 32],
    ) -> Result<(String, bool)> {
        self.put_owned_blob_with_inserted_after_body(data, pubkey, || {})
    }

    fn put_owned_blob_with_inserted_after_body(
        &self,
        data: &[u8],
        pubkey: &[u8; 32],
        after_body_write: impl FnOnce(),
    ) -> Result<(String, bool)> {
        let hash = sha256(data);
        let incoming_bytes = data.len() as u64;
        let _retention_guard = self
            .cache_quota
            .protect_retention_hashes(vec![hash])
            .map_err(|denial| {
                anyhow::anyhow!("owned blob write cannot race retention cleanup: {denial}")
            })?;
        let mut retried_after_cleanup = false;
        let inserted = loop {
            match self.router.put_sync(hash, data) {
                Ok(inserted) => break inserted,
                Err(err) if !retried_after_cleanup && is_map_full_store_error(&err) => {
                    let freed = self.make_room_for_durable_blob(incoming_bytes)?;
                    if freed == 0 {
                        return Err(anyhow::anyhow!("Failed to store blob: {}", err));
                    }
                    retried_after_cleanup = true;
                }
                Err(err) => return Err(anyhow::anyhow!("Failed to store blob: {}", err)),
            }
        };

        after_body_write();
        self.set_blob_owner_with_size(&hash, pubkey, incoming_bytes)?;
        if inserted {
            if let Err(err) = self.enforce_durable_blob_budget_after_insert(incoming_bytes) {
                let _ = self.delete_blossom_blob(&hash, pubkey);
                return Err(err);
            }
        }

        Ok((to_hex(&hash), inserted))
    }

    pub fn put_owned_blob(&self, data: &[u8], pubkey: &[u8; 32]) -> Result<String> {
        self.put_owned_blob_with_inserted(data, pubkey)
            .map(|(hash, _)| hash)
    }

    fn put_blob_owners_for_batch(
        &self,
        items: &[(Hash, Vec<u8>)],
        pubkey: &[u8; 32],
    ) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut wtxn = self.env.write_txn()?;
        for (hash, data) in items {
            let owner_key = Self::blob_owner_key(hash, pubkey);
            match self.blob_owners.put_with_flags(
                &mut wtxn,
                PutFlags::NO_OVERWRITE,
                &owner_key[..],
                &(),
            ) {
                Ok(()) => {}
                Err(HeedError::Mdb(MdbError::KeyExist)) => continue,
                Err(error) => return Err(error.into()),
            }

            let index_key = Self::pubkey_blob_key(pubkey, hash);
            let metadata = BlobMetadata {
                sha256: to_hex(hash),
                size: data.len() as u64,
                mime_type: "application/octet-stream".to_string(),
                uploaded: now,
            };
            self.pubkey_blob_index.put(
                &mut wtxn,
                &index_key[..],
                &serde_json::to_vec(&metadata)?,
            )?;
        }
        wtxn.commit()?;
        Ok(())
    }

    fn put_many_durable_blob_bodies(
        &self,
        items: &[(Hash, Vec<u8>)],
        incoming_bytes: u64,
    ) -> Result<PutManyReport> {
        let mut retried_after_cleanup = false;
        loop {
            match self.router.put_many_report_sync(items) {
                Ok(report) => return Ok(report),
                Err(err) if !retried_after_cleanup && is_map_full_store_error(&err) => {
                    let freed = self.make_room_for_durable_blob(incoming_bytes)?;
                    if freed == 0 {
                        return Err(anyhow::anyhow!("Failed to store blob batch: {}", err));
                    }
                    retried_after_cleanup = true;
                }
                Err(err) => return Err(anyhow::anyhow!("Failed to store blob batch: {}", err)),
            }
        }
    }

    /// Store multiple owned Blossom blobs, batching raw blob and owner-index writes.
    pub fn put_owned_blobs_report(
        &self,
        items: &[(Hash, Vec<u8>)],
        pubkey: &[u8; 32],
    ) -> Result<PutManyReport> {
        self.put_owned_blobs_report_after_bodies(items, pubkey, || {})
    }

    fn put_owned_blobs_report_after_bodies(
        &self,
        items: &[(Hash, Vec<u8>)],
        pubkey: &[u8; 32],
        after_body_write: impl FnOnce(),
    ) -> Result<PutManyReport> {
        let started_at = Instant::now();
        let slow_log_ms = slow_owned_blob_batch_log_ms();
        if items.is_empty() {
            return Ok(PutManyReport::default());
        }
        let _retention_guard = self
            .cache_quota
            .protect_retention_hashes(items.iter().map(|(hash, _)| *hash).collect())
            .map_err(|denial| {
                anyhow::anyhow!("owned blob batch cannot race retention cleanup: {denial}")
            })?;
        let incoming_bytes = items.iter().fold(0u64, |total, (_, data)| {
            total.saturating_add(data.len() as u64)
        });
        let count = items.len();
        let raw_started = Instant::now();
        let report = self.put_many_durable_blob_bodies(items, incoming_bytes)?;
        let raw_write_ms = raw_started.elapsed().as_millis();

        after_body_write();
        let owner_started = Instant::now();
        self.put_blob_owners_for_batch(items, pubkey)?;
        let owner_index_ms = owner_started.elapsed().as_millis();
        let quota_started = Instant::now();
        if report.inserted_bytes > 0 {
            if let Err(err) = self.enforce_durable_blob_budget_after_insert(report.inserted_bytes) {
                for hash in &report.inserted_hashes {
                    let _ = self.delete_blossom_blob(hash, pubkey);
                }
                return Err(err);
            }
        }
        let quota_ms = quota_started.elapsed().as_millis();
        let total_ms = started_at.elapsed().as_millis();
        if slow_log_ms.is_some_and(|threshold| total_ms >= threshold) {
            tracing::warn!(
                blobs = count,
                inserted = report.inserted,
                incoming_bytes,
                inserted_bytes = report.inserted_bytes,
                total_ms,
                raw_write_ms,
                owner_index_ms,
                quota_ms,
                "slow owned Blossom blob batch write"
            );
        }
        Ok(report)
    }

    /// Store multiple owned Blossom blobs, returning only the number of new blobs.
    pub fn put_owned_blobs(&self, items: &[(Hash, Vec<u8>)], pubkey: &[u8; 32]) -> Result<usize> {
        self.put_owned_blobs_report(items, pubkey)
            .map(|report| report.inserted)
    }

    /// Store an opportunistically cached blob.
    ///
    /// Unlike durable `put_blob` writes, this path may evict disposable orphaned
    /// blobs to make room under storage pressure. It intentionally avoids touching
    /// indexed trees, social-graph roots, explicit pins, and owned Blossom blobs.
    pub fn put_cached_blob_with_inserted(&self, data: &[u8]) -> Result<(String, bool)> {
        let hash = sha256(data);
        let incoming_bytes = data.len() as u64;

        // Existing blobs are a no-op and must remain available even while a
        // cleanup leader is working or a failed cleanup is backing off.
        let locally_present = self
            .router
            .existing_local_hashes_in_sorted_candidates(std::slice::from_ref(&hash))
            .map_err(|error| anyhow::anyhow!("Failed to check cached blob: {error}"))?
            .first()
            .copied()
            .unwrap_or(false);
        if locally_present {
            let inserted = self
                .router
                .put_sync(hash, data)
                .map_err(|error| anyhow::anyhow!("Failed to store cached blob: {error}"))?;
            return Ok((to_hex(&hash), inserted));
        }

        let mut permit = self.prepare_cached_blob_write(incoming_bytes, vec![hash], false)?;
        let mut retried_after_cleanup = false;
        loop {
            match self.router.put_sync(hash, data) {
                Ok(inserted) => {
                    permit.commit(if inserted { incoming_bytes } else { 0 });
                    return Ok((to_hex(&hash), inserted));
                }
                Err(err) if !retried_after_cleanup && is_map_full_store_error(&err) => {
                    drop(permit);
                    permit = self.prepare_cached_blob_write(incoming_bytes, vec![hash], true)?;
                    retried_after_cleanup = true;
                }
                Err(err) => return Err(anyhow::anyhow!("Failed to store cached blob: {}", err)),
            }
        }
    }

    pub fn put_cached_blob(&self, data: &[u8]) -> Result<String> {
        self.put_cached_blob_with_inserted(data)
            .map(|(hash, _)| hash)
    }

    /// Store multiple opportunistically cached blobs in one raw storage batch.
    pub fn put_cached_blobs_report(&self, items: &[(Hash, Vec<u8>)]) -> Result<PutManyReport> {
        let started_at = Instant::now();
        let slow_log_ms = slow_cached_blob_batch_log_ms();
        if items.is_empty() {
            return Ok(PutManyReport::default());
        }

        let candidate_bytes = items.iter().fold(0u64, |total, (_, data)| {
            total.saturating_add(data.len() as u64)
        });

        let mut unique_candidates = HashMap::<Hash, u64>::new();
        for (hash, data) in items {
            unique_candidates.entry(*hash).or_insert(data.len() as u64);
        }
        let mut sorted_candidates = unique_candidates.into_iter().collect::<Vec<_>>();
        sorted_candidates.sort_unstable_by_key(|(hash, _)| *hash);
        let sorted_hashes = sorted_candidates
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<Vec<_>>();
        let existing = self
            .router
            .existing_local_hashes_in_sorted_candidates(&sorted_hashes)
            .map_err(|error| anyhow::anyhow!("Failed to check cached blob batch: {error}"))?;
        let mut missing_hashes = Vec::new();
        let mut missing_bytes = 0u64;
        for ((hash, size), present) in sorted_candidates.into_iter().zip(existing) {
            if !present {
                missing_hashes.push(hash);
                missing_bytes = missing_bytes.saturating_add(size);
            }
        }

        // A duplicate-only batch is a no-op and must not be rejected merely
        // because an unrelated cleanup attempt is active or backing off.
        let mut permit = if missing_hashes.is_empty() {
            None
        } else {
            Some(self.prepare_cached_blob_write(missing_bytes, missing_hashes.clone(), false)?)
        };
        let mut retried_after_cleanup = false;
        loop {
            let raw_started = Instant::now();
            match self.router.put_many_report_sync(items) {
                Ok(report) => {
                    let raw_write_ms = raw_started.elapsed().as_millis();
                    if let Some(permit) = permit.take() {
                        permit.commit(report.inserted_bytes);
                    }
                    let quota_ms = 0u128;
                    let total_ms = started_at.elapsed().as_millis();
                    if slow_log_ms.is_some_and(|threshold| total_ms >= threshold) {
                        tracing::warn!(
                            blobs = items.len(),
                            inserted = report.inserted,
                            candidate_bytes,
                            inserted_bytes = report.inserted_bytes,
                            total_ms,
                            raw_write_ms,
                            quota_ms,
                            "slow cached Blossom blob batch write"
                        );
                    }
                    return Ok(report);
                }
                Err(err) if !retried_after_cleanup && is_map_full_store_error(&err) => {
                    drop(permit.take());
                    if missing_hashes.is_empty() {
                        return Err(anyhow::anyhow!(
                            "Failed to store cached blob batch: {}",
                            err
                        ));
                    }
                    permit = Some(self.prepare_cached_blob_write(
                        missing_bytes,
                        missing_hashes.clone(),
                        true,
                    )?);
                    retried_after_cleanup = true;
                }
                Err(err) => {
                    return Err(anyhow::anyhow!(
                        "Failed to store cached blob batch: {}",
                        err
                    ));
                }
            }
        }
    }

    /// Store multiple opportunistically cached blobs, returning only the number of new blobs.
    pub fn put_cached_blobs(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize> {
        self.put_cached_blobs_report(items)
            .map(|report| report.inserted)
    }

    /// Get a raw blob by SHA256 hash (raw bytes).
    pub fn get_blob(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let data = self
            .router
            .get_sync(hash)
            .map_err(|e| anyhow::anyhow!("Failed to get blob: {}", e))?;
        if data.is_some() {
            self.record_blob_accesses(std::iter::once(*hash));
        }
        Ok(data)
    }

    pub fn get_blob_range(
        &self,
        hash: &[u8; 32],
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>> {
        let data = self
            .router
            .get_range_sync(hash, start, end_inclusive)
            .map_err(|e| anyhow::anyhow!("Failed to get blob range: {}", e))?;
        if data.is_some() {
            self.record_blob_accesses(std::iter::once(*hash));
        }
        Ok(data)
    }

    pub fn blob_size(&self, hash: &[u8; 32]) -> Result<Option<u64>> {
        self.router
            .blob_size_sync(hash)
            .map_err(|e| anyhow::anyhow!("Failed to get blob size: {}", e))
    }

    /// Check if a blob exists by SHA256 hash (raw bytes).
    pub fn blob_exists(&self, hash: &[u8; 32]) -> Result<bool> {
        self.router
            .exists(hash)
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

    fn pubkey_blob_key(pubkey: &[u8; 32], sha256: &[u8; 32]) -> [u8; 64] {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(pubkey);
        key[32..].copy_from_slice(sha256);
        key
    }

    /// Add an owner (pubkey) to a blob for Blossom protocol
    /// Multiple users can own the same blob - it's only deleted when all owners remove it
    pub fn set_blob_owner(&self, sha256: &[u8; 32], pubkey: &[u8; 32]) -> Result<()> {
        let _retention_guard = self
            .cache_quota
            .protect_retention_hashes(vec![*sha256])
            .map_err(|denial| {
                anyhow::anyhow!("blob ownership update cannot race retention cleanup: {denial}")
            })?;
        let size = self
            .router
            .blob_size_sync(sha256)
            .map_err(|e| anyhow::anyhow!("Failed to get blob size: {}", e))?
            .unwrap_or(0);
        self.set_blob_owner_with_size(sha256, pubkey, size)
    }

    fn set_blob_owner_with_size(
        &self,
        sha256: &[u8; 32],
        pubkey: &[u8; 32],
        size: u64,
    ) -> Result<()> {
        let key = Self::blob_owner_key(sha256, pubkey);
        let index_key = Self::pubkey_blob_key(pubkey, sha256);
        let mut wtxn = self.env.write_txn()?;

        match self
            .blob_owners
            .put_with_flags(&mut wtxn, PutFlags::NO_OVERWRITE, &key[..], &())
        {
            Ok(()) => {}
            Err(HeedError::Mdb(MdbError::KeyExist)) => {
                wtxn.commit()?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let metadata = BlobMetadata {
            sha256: to_hex(sha256),
            size,
            mime_type: "application/octet-stream".to_string(),
            uploaded: now,
        };
        self.pubkey_blob_index
            .put(&mut wtxn, &index_key[..], &serde_json::to_vec(&metadata)?)?;

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
        self.pubkey_blob_index
            .delete(&mut wtxn, &Self::pubkey_blob_key(pubkey, sha256)[..])?;

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

        if self.router.full_deletes_blocked() {
            return Err(anyhow::anyhow!(POOL_MIGRATION_DELETE_DISABLED));
        }

        // Delete raw blob (by content hash) - this deletes from S3 too
        self.router.delete_sync(sha256)?;

        wtxn.commit()?;
        Ok(true)
    }

    /// List all blobs owned by a pubkey (for Blossom /list endpoint)
    pub fn list_blobs_by_pubkey(
        &self,
        pubkey: &[u8; 32],
    ) -> Result<Vec<crate::server::blossom::BlobDescriptor>> {
        let rtxn = self.env.read_txn()?;

        let mut blobs: Vec<BlobMetadata> = self
            .pubkey_blobs
            .get(&rtxn, pubkey)?
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default();
        let mut seen: HashSet<String> = blobs.iter().map(|blob| blob.sha256.clone()).collect();

        for item in self.pubkey_blob_index.prefix_iter(&rtxn, pubkey)? {
            let (_, metadata_bytes) = item?;
            let metadata: BlobMetadata = match serde_json::from_slice(metadata_bytes) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if seen.insert(metadata.sha256.clone()) {
                blobs.push(metadata);
            }
        }

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
        let data = self
            .router
            .get_sync(hash)
            .map_err(|e| anyhow::anyhow!("Failed to get chunk: {}", e))?;
        if data.is_some() {
            self.record_blob_accesses(std::iter::once(*hash));
        }
        Ok(data)
    }

    /// Get file content by hash (raw bytes)
    /// Returns raw bytes (caller handles decryption if needed)
    pub fn get_file(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let (tree, access_store) = self.access_tracking_tree();

        let result = sync_block_on(async {
            tree.read_file(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))
        })?;
        if result.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        Ok(result)
    }

    /// Get file content by Cid (hash + optional decryption key as raw bytes)
    /// Handles decryption automatically if key is present
    pub fn get_file_by_cid(&self, cid: &Cid) -> Result<Option<Vec<u8>>> {
        let (tree, access_store) = self.access_tracking_tree();

        let result = sync_block_on(async {
            tree.get(cid, None)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))
        })?;
        if result.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        Ok(result)
    }

    fn ensure_cid_exists(&self, cid: &Cid) -> Result<()> {
        let exists = self
            .router
            .exists(&cid.hash)
            .map_err(|e| anyhow::anyhow!("Failed to check cid existence: {}", e))?;
        if !exists {
            anyhow::bail!("CID not found: {}", to_hex(&cid.hash));
        }
        Ok(())
    }

    /// Stream file content identified by Cid into a writer without buffering full file in memory.
    pub fn write_file_by_cid_to_writer<W: Write>(&self, cid: &Cid, writer: &mut W) -> Result<u64> {
        self.ensure_cid_exists(cid)?;

        let (tree, access_store) = self.access_tracking_tree();
        let mut total_bytes = 0u64;
        let mut streamed_any_chunk = false;

        sync_block_on(async {
            let mut stream = tree.get_stream(cid);
            while let Some(chunk) = stream.next().await {
                streamed_any_chunk = true;
                let chunk =
                    chunk.map_err(|e| anyhow::anyhow!("Failed to stream file chunk: {}", e))?;
                writer
                    .write_all(&chunk)
                    .map_err(|e| anyhow::anyhow!("Failed to write file chunk: {}", e))?;
                total_bytes += chunk.len() as u64;
            }
            Ok::<(), anyhow::Error>(())
        })?;

        if !streamed_any_chunk {
            anyhow::bail!("CID not found: {}", to_hex(&cid.hash));
        }
        self.record_blob_accesses(access_store.take_accessed_hashes());

        writer
            .flush()
            .map_err(|e| anyhow::anyhow!("Failed to flush output: {}", e))?;
        Ok(total_bytes)
    }

    /// Stream file content identified by Cid directly into a destination path.
    pub fn write_file_by_cid<P: AsRef<Path>>(&self, cid: &Cid, output_path: P) -> Result<u64> {
        self.ensure_cid_exists(cid)?;

        let output_path = output_path.as_ref();
        if let Some(parent) = output_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create output directory {}", parent.display())
                })?;
            }
        }

        let mut file = std::fs::File::create(output_path)
            .with_context(|| format!("Failed to create output file {}", output_path.display()))?;
        self.write_file_by_cid_to_writer(cid, &mut file)
    }

    /// Stream a public (unencrypted) file by hash directly into a destination path.
    pub fn write_file<P: AsRef<Path>>(&self, hash: &[u8; 32], output_path: P) -> Result<u64> {
        self.write_file_by_cid(&Cid::public(*hash), output_path)
    }

    /// Resolve a path within a tree (returns Cid with key if encrypted)
    pub fn resolve_path(&self, cid: &Cid, path: &str) -> Result<Option<Cid>> {
        let (tree, access_store) = self.access_tracking_tree();

        let result = sync_block_on(async {
            tree.resolve_path(cid, path)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to resolve path: {}", e))
        })?;
        if result.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        Ok(result)
    }

    /// Get chunk metadata for a file (chunk list, sizes, total size)
    pub fn get_file_chunk_metadata(
        &self,
        hash: &[u8; 32],
    ) -> Result<Option<Arc<FileChunkMetadata>>> {
        if let Ok(mut cache) = self.file_metadata_cache.lock() {
            if let Some(metadata) = cache.get(hash).cloned() {
                self.record_blob_accesses(std::iter::once(*hash));
                return Ok(Some(metadata));
            }
        }

        let access_store = AccessRecordingStore::new(self.store_arc());
        let tree = HashTree::new(HashTreeConfig::new(Arc::new(access_store.clone())).public());

        let metadata: Result<Option<FileChunkMetadata>> = sync_block_on(async {
            // First check if the hash exists in the store at all
            // (either as a blob or tree node)
            let exists = access_store
                .has(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to check existence: {}", e))?;

            if !exists {
                return Ok(None);
            }

            // Get total size
            let total_size = tree
                .get_size(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get size: {}", e))?;

            // Check if it's a tree (chunked) or blob
            let is_tree_node = tree
                .is_tree(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to check tree: {}", e))?;

            if !is_tree_node {
                // Single blob, not chunked
                return Ok(Some(FileChunkMetadata::single_blob(total_size)));
            }

            // Get tree node to extract chunk info
            let node = match tree
                .get_tree_node(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))?
            {
                Some(n) => n,
                None => return Ok(None),
            };

            // Check if it's a directory (has named links)
            let is_directory = tree
                .is_directory(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to check directory: {}", e))?;

            if is_directory {
                return Ok(None); // Not a file
            }

            // Extract chunk info from links
            let chunk_hashes: Vec<Hash> = node.links.iter().map(|l| l.hash).collect();
            let chunk_sizes: Vec<u64> = node.links.iter().map(|l| l.size).collect();

            Ok(Some(FileChunkMetadata::new(
                total_size,
                chunk_hashes,
                chunk_sizes,
            )))
        });
        let metadata = metadata?;
        if metadata.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        let metadata = Arc::new(metadata);
        if let Ok(mut cache) = self.file_metadata_cache.lock() {
            cache.put(*hash, Arc::clone(&metadata));
        }
        Ok(Some(metadata))
    }

    /// Get byte range from file
    pub fn get_file_range(
        &self,
        hash: &[u8; 32],
        start: u64,
        end: Option<u64>,
    ) -> Result<Option<(Vec<u8>, u64)>> {
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

        let end = end
            .unwrap_or(metadata.total_size - 1)
            .min(metadata.total_size - 1);

        // For non-chunked files, read only the requested blob range.
        if !metadata.is_chunked {
            let range_content = match self.get_blob_range(hash, start, end)? {
                Some(content) => content,
                None => return Ok(None),
            };
            return Ok(Some((range_content, metadata.total_size)));
        }

        // For chunked files, load only needed chunks
        let mut result = Vec::new();
        let (start_idx, mut current_offset) = metadata.chunk_start_for_range(start);

        for (i, chunk_hash) in metadata.chunk_hashes.iter().enumerate().skip(start_idx) {
            let chunk_size = metadata.chunk_sizes[i];
            let chunk_end = current_offset + chunk_size - 1;

            // Check if this chunk overlaps with requested range
            if chunk_end >= start && current_offset <= end {
                let chunk_read_start = start.saturating_sub(current_offset);

                let chunk_read_end = if chunk_end <= end {
                    chunk_size - 1
                } else {
                    end - current_offset
                };

                let chunk_content =
                    match self.get_blob_range(chunk_hash, chunk_read_start, chunk_read_end)? {
                        Some(content) => content,
                        None => {
                            return Err(anyhow::anyhow!("Chunk {} not found", to_hex(chunk_hash)));
                        }
                    };

                let expected_len = chunk_read_end.saturating_sub(chunk_read_start) + 1;
                if chunk_content.len() as u64 != expected_len {
                    return Err(anyhow::anyhow!(
                        "Chunk {} range returned {} bytes, expected {}",
                        to_hex(chunk_hash),
                        chunk_content.len(),
                        expected_len
                    ));
                }

                result.extend_from_slice(&chunk_content);
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

        let (current_chunk_idx, current_offset) = metadata.chunk_start_for_range(start);

        Ok(Some(FileRangeChunksOwned {
            store: self,
            metadata,
            start,
            end,
            current_chunk_idx,
            current_offset,
        }))
    }

    /// Get directory structure by hash (raw bytes)
    pub fn get_directory_listing(&self, hash: &[u8; 32]) -> Result<Option<DirectoryListing>> {
        let (tree, access_store) = self.access_tracking_tree();

        let listing: Result<Option<DirectoryListing>> = sync_block_on(async {
            // Check if it's a directory
            let is_dir = tree
                .is_directory(hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to check directory: {}", e))?;

            if !is_dir {
                return Ok(None);
            }

            // Get directory entries (public Cid - no encryption key)
            let cid = hashtree_core::Cid::public(*hash);
            let tree_entries = tree
                .list_directory(&cid)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to list directory: {}", e))?;

            let entries: Vec<DirEntry> = tree_entries
                .into_iter()
                .map(|e| DirEntry {
                    name: e.name,
                    cid: to_hex(&e.hash),
                    is_directory: e.link_type.is_tree(),
                    size: e.size,
                })
                .collect();

            Ok(Some(DirectoryListing {
                dir_name: String::new(),
                entries,
            }))
        });
        let listing = listing?;
        if listing.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        Ok(listing)
    }

    /// Get directory structure by CID, supporting encrypted directories.
    pub fn get_directory_listing_by_cid(&self, cid: &Cid) -> Result<Option<DirectoryListing>> {
        let (tree, access_store) = self.access_tracking_tree();
        let cid = cid.clone();

        let listing: Result<Option<DirectoryListing>> = sync_block_on(async {
            let is_dir = tree
                .is_dir(&cid)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to check directory: {}", e))?;

            if !is_dir {
                return Ok(None);
            }

            let tree_entries = tree
                .list_directory(&cid)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to list directory: {}", e))?;

            let entries: Vec<DirEntry> = tree_entries
                .into_iter()
                .map(|e| DirEntry {
                    name: e.name,
                    cid: Cid {
                        hash: e.hash,
                        key: e.key,
                    }
                    .to_string(),
                    is_directory: e.link_type.is_tree(),
                    size: e.size,
                })
                .collect();

            Ok(Some(DirectoryListing {
                dir_name: String::new(),
                entries,
            }))
        });
        let listing = listing?;
        if listing.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        Ok(listing)
    }

    // === Cached roots ===

    /// Persist a mutable published ref that should stay subscribed.
    pub fn add_pinned_ref(&self, key: &str) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.pinned_refs.put(&mut wtxn, key, &())?;
        wtxn.commit()?;
        Ok(())
    }

    /// Remove a mutable published ref from the live pinned set.
    pub fn remove_pinned_ref(&self, key: &str) -> Result<bool> {
        let mut wtxn = self.env.write_txn()?;
        let removed = self.pinned_refs.delete(&mut wtxn, key)?;
        wtxn.commit()?;
        Ok(removed)
    }

    /// List mutable published refs that should stay subscribed.
    pub fn list_pinned_refs(&self) -> Result<Vec<String>> {
        let rtxn = self.env.read_txn()?;
        let mut refs = Vec::new();

        for item in self.pinned_refs.iter(&rtxn)? {
            let (key, _) = item?;
            refs.push(key.to_string());
        }

        refs.sort();
        Ok(refs)
    }

    /// Persist an author whose published trees should stay mirrored.
    pub fn add_tracked_author(&self, npub: &str) -> Result<bool> {
        let mut wtxn = self.env.write_txn()?;
        let inserted = self.tracked_authors.get(&wtxn, npub)?.is_none();
        self.tracked_authors.put(&mut wtxn, npub, &())?;
        wtxn.commit()?;
        Ok(inserted)
    }

    /// Remove an author from the continuous mirror set.
    pub fn remove_tracked_author(&self, npub: &str) -> Result<bool> {
        let mut wtxn = self.env.write_txn()?;
        let removed = self.tracked_authors.delete(&mut wtxn, npub)?;
        wtxn.commit()?;
        Ok(removed)
    }

    /// List authors whose published trees should stay mirrored.
    pub fn list_tracked_authors(&self) -> Result<Vec<String>> {
        let rtxn = self.env.read_txn()?;
        let mut authors = Vec::new();

        for item in self.tracked_authors.iter(&rtxn)? {
            let (npub, _) = item?;
            authors.push(npub.to_string());
        }

        authors.sort();
        Ok(authors)
    }

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
}

fn is_map_full_store_error(err: &StoreError) -> bool {
    let message = err.to_string();
    message.contains("MDB_MAP_FULL") || message.contains("MapFull")
}

#[derive(Debug, Clone)]
pub struct FileChunkMetadata {
    pub total_size: u64,
    pub chunk_hashes: Vec<Hash>,
    pub chunk_sizes: Vec<u64>,
    pub is_chunked: bool,
    uniform_chunk_size: Option<u64>,
}

impl FileChunkMetadata {
    fn new(total_size: u64, chunk_hashes: Vec<Hash>, chunk_sizes: Vec<u64>) -> Self {
        let is_chunked = !chunk_hashes.is_empty();
        let uniform_chunk_size = uniform_chunk_size(&chunk_sizes);
        Self {
            total_size,
            chunk_hashes,
            chunk_sizes,
            is_chunked,
            uniform_chunk_size,
        }
    }

    fn single_blob(total_size: u64) -> Self {
        Self {
            total_size,
            chunk_hashes: Vec::new(),
            chunk_sizes: Vec::new(),
            is_chunked: false,
            uniform_chunk_size: None,
        }
    }

    fn chunk_start_for_range(&self, start: u64) -> (usize, u64) {
        if !self.is_chunked || self.chunk_sizes.is_empty() {
            return (0, 0);
        }

        if let Some(chunk_size) = self.uniform_chunk_size {
            let index = start
                .checked_div(chunk_size)
                .unwrap_or(0)
                .min(self.chunk_sizes.len().saturating_sub(1) as u64)
                as usize;
            return (index, chunk_size.saturating_mul(index as u64));
        }

        let mut offset = 0u64;
        for (index, chunk_size) in self.chunk_sizes.iter().copied().enumerate() {
            let next_offset = offset.saturating_add(chunk_size);
            if start < next_offset {
                return (index, offset);
            }
            offset = next_offset;
        }

        (self.chunk_sizes.len(), offset)
    }
}

fn uniform_chunk_size(chunk_sizes: &[u64]) -> Option<u64> {
    let (&first, rest) = chunk_sizes.split_first()?;
    if first == 0 {
        return None;
    }
    if rest.is_empty() {
        return Some(first);
    }
    let (last, prefix) = rest.split_last()?;
    if prefix.iter().any(|size| *size != first) || *last > first {
        return None;
    }
    Some(first)
}

/// Owned iterator for async streaming
pub struct FileRangeChunksOwned {
    store: Arc<HashtreeStore>,
    metadata: Arc<FileChunkMetadata>,
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

        let chunk_read_start = self.start.saturating_sub(self.current_offset);

        let chunk_read_end = if chunk_end <= self.end {
            chunk_size - 1
        } else {
            self.end - self.current_offset
        };

        let chunk_content =
            match self
                .store
                .get_blob_range(chunk_hash, chunk_read_start, chunk_read_end)
            {
                Ok(Some(content)) => content,
                Ok(None) => {
                    return Some(Err(anyhow::anyhow!(
                        "Chunk {} not found",
                        to_hex(chunk_hash)
                    )));
                }
                Err(e) => {
                    return Some(Err(e));
                }
            };

        let expected_len = chunk_read_end.saturating_sub(chunk_read_start) + 1;
        if chunk_content.len() as u64 != expected_len {
            return Some(Err(anyhow::anyhow!(
                "Chunk {} range returned {} bytes, expected {}",
                to_hex(chunk_hash),
                chunk_content.len(),
                expected_len
            )));
        }

        let result = chunk_content;
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

/// Blob metadata for Blossom protocol
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlobMetadata {
    pub sha256: String,
    pub size: u64,
    pub mime_type: String,
    pub uploaded: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "lmdb")]
    use hashtree_lmdb::{PoolMemberConfig, PoolStoreConfig};
    #[cfg(feature = "lmdb")]
    use tempfile::TempDir;

    #[cfg(feature = "lmdb")]
    #[test]
    fn drop_closes_mutable_metadata_and_blob_environments() -> Result<()> {
        let temp = TempDir::new()?;
        let root = temp.path().join("store");
        let store = HashtreeStore::with_options(&root, None, LMDB_BLOB_MIN_MAP_SIZE_BYTES)?;
        let metadata_path = std::fs::canonicalize(&root)?;
        let pool_path = std::fs::canonicalize(root.join("blob-pool-v1"))?;
        let member_path = std::fs::canonicalize(root.join("blobs"))?;

        drop(store);

        assert!(
            heed::env_closing_event(metadata_path).is_none(),
            "mutable application metadata environment must close"
        );
        assert!(
            heed::env_closing_event(pool_path).is_none(),
            "blob pool catalog environment must close"
        );
        assert!(
            heed::env_closing_event(member_path).is_none(),
            "blob pool member environment must close"
        );
        Ok(())
    }

    #[test]
    fn blob_access_update_gate_deduplicates_and_throttles() {
        let gate = BlobAccessUpdateGate::default();
        let first = sha256(b"first");
        let second = sha256(b"second");

        assert_eq!(
            gate.due_hashes([first, first, second], 10),
            vec![first, second]
        );
        assert!(gate.due_hashes([first, second], 11).is_empty());
        assert_eq!(
            gate.due_hashes([second, first], 10 + ACCESS_UPDATE_INTERVAL_SECS),
            vec![second, first]
        );
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn pool_migration_fallback_reads_old_tiers_and_writes_only_pool() -> Result<()> {
        let temp = TempDir::new()?;
        let pool = PoolStore::open(temp.path().join("pool"), PoolStoreConfig::default())?;
        let member_map_size = 64 * 1024 * 1024;
        pool.add_member(
            PoolMemberConfig::new(temp.path().join("member"), member_map_size)
                .with_map_size_bytes(member_map_size),
        )?;
        let hot_path = temp.path().join("hot");
        let legacy_path = temp.path().join("legacy");

        let hot_data = b"existing hot-tier blob".to_vec();
        let hot_hash = sha256(&hot_data);
        let legacy_data = b"existing legacy-tier blob".to_vec();
        let legacy_hash = sha256(&legacy_data);
        {
            let hot = LmdbBlobStore::new(&hot_path)?;
            hot.put_sync(hot_hash, &hot_data)?;
            let legacy = LmdbBlobStore::new(&legacy_path)?;
            legacy.put_sync(legacy_hash, &legacy_data)?;
        }
        let hot_data_mdb = hot_path.join("data.mdb");
        let legacy_data_mdb = legacy_path.join("data.mdb");
        let hot_before = std::fs::metadata(&hot_data_mdb)?;
        let legacy_before = std::fs::metadata(&legacy_data_mdb)?;
        let hot = LmdbBlobReader::open(&hot_path, None)?;
        let legacy = LmdbBlobReader::open(&legacy_path, None)?;
        let store = PoolStoreWithFallbacks::new(pool.clone(), vec![hot, legacy]);

        let new_data = b"new writes belong only in PoolStore".to_vec();
        let new_hash = sha256(&new_data);
        assert!(store.put_sync(new_hash, &new_data)?);
        assert_eq!(pool.get_sync(&new_hash)?, Some(new_data));
        assert!(
            store
                .fallbacks
                .iter()
                .all(|fallback| fallback.get_sync(&new_hash).unwrap().is_none()),
            "migration fallbacks must never receive new writes"
        );

        let mut candidates = vec![hot_hash, new_hash, legacy_hash];
        candidates.sort_unstable();
        let canonical_before_reads = store.existing_hashes_in_sorted_candidates(&candidates)?;
        for (hash, exists) in candidates.iter().zip(canonical_before_reads) {
            assert_eq!(
                exists,
                *hash == new_hash,
                "legacy fallback entries must not suppress migration writes"
            );
        }

        assert_eq!(store.get_sync(&hot_hash)?, Some(hot_data.clone()));
        assert_eq!(
            store.blob_size_sync(&legacy_hash)?,
            Some(legacy_data.len() as u64)
        );
        assert_eq!(
            pool.get_sync(&legacy_hash)?,
            None,
            "metadata-only size checks must not read or promote the body"
        );
        assert_eq!(
            store.get_range_sync(&legacy_hash, 9, 14)?,
            Some(legacy_data[9..=14].to_vec())
        );
        assert_eq!(pool.get_sync(&hot_hash)?, Some(hot_data));
        assert_eq!(pool.get_sync(&legacy_hash)?, Some(legacy_data));
        let canonical_after_reads = store.existing_hashes_in_sorted_candidates(&candidates)?;
        assert_eq!(canonical_after_reads, vec![true; candidates.len()]);
        let hot_after = std::fs::metadata(hot_data_mdb)?;
        let legacy_after = std::fs::metadata(legacy_data_mdb)?;
        assert_eq!(hot_after.len(), hot_before.len());
        assert_eq!(hot_after.modified()?, hot_before.modified()?);
        assert_eq!(legacy_after.len(), legacy_before.len());
        assert_eq!(legacy_after.modified()?, legacy_before.modified()?);
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn pool_migration_fallback_blocks_racy_full_deletes() -> Result<()> {
        let temp = TempDir::new()?;
        let pool = PoolStore::open(temp.path().join("pool"), PoolStoreConfig::default())?;
        let member_map_size = 64 * 1024 * 1024;
        pool.add_member(
            PoolMemberConfig::new(temp.path().join("member"), member_map_size)
                .with_map_size_bytes(member_map_size),
        )?;
        let fallback_path = temp.path().join("legacy");
        let data = b"source blob under active migration".to_vec();
        let hash = sha256(&data);
        {
            let fallback = LmdbBlobStore::new(&fallback_path)?;
            fallback.put_sync(hash, &data)?;
        }
        let fallback = LmdbBlobReader::open(fallback_path, None)?;
        let store = PoolStoreWithFallbacks::during_migration(pool, vec![fallback]);

        let error = store
            .delete_sync(&hash)
            .expect_err("full delete must fail closed while source replay can race it");
        assert!(error.to_string().contains("temporarily disabled"));
        assert_eq!(store.get_sync(&hash)?, Some(data));
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn file_range_reads_reuse_metadata_and_seek_to_uniform_chunk() -> Result<()> {
        let temp = TempDir::new()?;
        let store = Arc::new(HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            LMDB_BLOB_MIN_MAP_SIZE_BYTES,
            true,
            &StorageBackend::Fs,
        )?);
        let tree = HashTree::new(
            HashTreeConfig::new(store.store_arc())
                .with_chunk_size(4)
                .public(),
        );
        let data = (0u8..20).collect::<Vec<_>>();
        let (cid, _) = sync_block_on(tree.put_file(&data))?;

        let first = store.get_file_chunk_metadata(&cid.hash)?.unwrap();
        let second = store.get_file_chunk_metadata(&cid.hash)?.unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "hot file metadata should be returned from the in-process cache"
        );
        assert_eq!(first.uniform_chunk_size, Some(4));
        assert_eq!(first.chunk_start_for_range(14), (3, 12));

        let mut chunks = Arc::clone(&store)
            .stream_file_range_chunks_owned(&cid.hash, 14, 17)?
            .unwrap();
        assert_eq!(chunks.current_chunk_idx, 3);
        assert_eq!(chunks.current_offset, 12);
        assert_eq!(chunks.next().unwrap()?, vec![14, 15]);
        assert_eq!(chunks.next().unwrap()?, vec![16, 17]);
        assert!(chunks.next().is_none());

        let (range, total_size) = store.get_file_range(&cid.hash, 14, Some(17))?.unwrap();
        assert_eq!(total_size, data.len() as u64);
        assert_eq!(range, vec![14, 15, 16, 17]);

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn hashtree_store_expands_blob_lmdb_map_size_to_storage_budget() -> Result<()> {
        let temp = TempDir::new()?;
        let requested = LMDB_BLOB_MIN_MAP_SIZE_BYTES + 64 * 1024 * 1024;
        let store = HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            requested,
            true,
            &StorageBackend::Lmdb,
        )?;

        let map_size = match store.router.local.as_ref() {
            LocalStore::Lmdb(local) => local.map_size_bytes() as u64,
            LocalStore::Pool(pool) => {
                pool.largest_member_map_size_bytes()?
                    .expect("fresh pool should have a blob member") as u64
            }
            LocalStore::Fs(_) => panic!("expected LMDB local store"),
        };

        assert!(
            map_size >= requested,
            "expected blob LMDB map to grow to at least {requested} bytes, got {map_size}"
        );

        drop(store);
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn hashtree_store_expands_metadata_lmdb_map_size_to_storage_budget() -> Result<()> {
        let temp = TempDir::new()?;
        let storage_budget = 256 * 1024 * 1024 * 1024u64;
        let expected = lmdb_metadata_map_size_for_storage_budget(storage_budget);
        let store = HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            storage_budget,
            true,
            &StorageBackend::Lmdb,
        )?;

        let map_size = store.env.info().map_size as u64;
        assert!(
            map_size >= expected,
            "expected metadata LMDB map to grow to at least {expected} bytes, got {map_size}"
        );

        drop(store);
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn embedded_store_uses_filesystem_blobs_and_no_lmdb_lock() -> Result<()> {
        let temp = TempDir::new()?;
        let store =
            HashtreeStore::with_embedded_options(temp.path(), None, LMDB_BLOB_MIN_MAP_SIZE_BYTES)?;

        assert_eq!(store.router.local_store().backend(), StorageBackend::Fs);
        let flags = store.env.flags()?.unwrap_or(EnvFlags::empty());
        assert!(flags.contains(EnvFlags::NO_LOCK));

        drop(store);
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn lmdb_map_size_for_existing_env_keeps_matching_requested_size() -> Result<()> {
        let temp = TempDir::new()?;
        let requested = LMDB_METADATA_MIN_MAP_SIZE_BYTES;
        std::fs::File::create(temp.path().join("data.mdb"))?.set_len(requested)?;

        let map_size = lmdb_map_size_for_existing_env(temp.path(), requested)? as u64;

        assert_eq!(map_size, align_lmdb_map_size(requested));
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn lmdb_map_size_for_existing_env_adds_headroom_when_existing_is_larger() -> Result<()> {
        let temp = TempDir::new()?;
        let requested = LMDB_METADATA_MIN_MAP_SIZE_BYTES;
        let existing = requested + 4096;
        std::fs::File::create(temp.path().join("data.mdb"))?.set_len(existing)?;

        let map_size = lmdb_map_size_for_existing_env(temp.path(), requested)? as u64;
        let expected = align_lmdb_map_size(existing + LMDB_METADATA_REOPEN_HEADROOM_BYTES);

        assert_eq!(map_size, expected);
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn local_store_can_override_lmdb_map_size() -> Result<()> {
        let temp = TempDir::new()?;
        let requested = 512 * 1024 * 1024u64;
        let store = LocalStore::new_with_lmdb_map_size(
            temp.path().join("lmdb-blobs"),
            &StorageBackend::Lmdb,
            Some(requested),
        )?;

        let map_size = match store {
            LocalStore::Lmdb(local) => local.map_size_bytes() as u64,
            LocalStore::Pool(pool) => {
                pool.largest_member_map_size_bytes()?
                    .expect("fresh pool should have a blob member") as u64
            }
            LocalStore::Fs(_) => panic!("expected LMDB local store"),
        };

        assert!(
            map_size >= requested,
            "expected LMDB map to grow to at least {requested} bytes, got {map_size}"
        );

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn local_add_reopen_options_leave_ordinary_writes_inline() -> Result<()> {
        let temp = TempDir::new()?;
        let store_path = temp.path().join("blobs");
        let external_path = temp.path().join(LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME);
        let store = LmdbBlobStore::with_external_blob_options(
            &store_path,
            Some(local_add_external_blob_reopen_options(&store_path)),
        )?;
        let data = vec![7; 192 * 1024];
        let hash = sha256(&data);

        assert!(store.put_sync(hash, &data)?);
        assert_eq!(store.get_sync(&hash)?, Some(data));
        assert!(!external_path.exists());
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn lmdb_local_store_removes_stale_fs_blob_shard_dirs() -> Result<()> {
        let temp = TempDir::new()?;
        let path = temp.path().join("lmdb-blobs");
        std::fs::create_dir_all(path.join("aa"))?;
        std::fs::create_dir_all(path.join("b2"))?;
        std::fs::create_dir_all(path.join("keep-me"))?;
        std::fs::write(path.join("aa").join("blob.bin"), b"old fs shard")?;
        std::fs::write(path.join("b2").join("blob.bin"), b"old fs shard")?;
        std::fs::write(path.join("keep-me").join("note.txt"), b"keep")?;

        let _store = LocalStore::new_with_lmdb_map_size(
            &path,
            &StorageBackend::Lmdb,
            Some(128 * 1024 * 1024),
        )?;

        assert!(!path.join("aa").exists());
        assert!(!path.join("b2").exists());
        assert!(path.join("keep-me").exists());
        assert!(path.join("data.mdb").exists());
        assert!(path.join("lock.mdb").exists());

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn duplicate_blossom_writes_do_not_refresh_blob_last_accessed() -> Result<()> {
        let temp = TempDir::new()?;
        let store = HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            LMDB_BLOB_MIN_MAP_SIZE_BYTES,
            true,
            &StorageBackend::Lmdb,
        )?;

        let raw = b"raw duplicate";
        let raw_hash = sha256(raw);
        store.put_blob(raw)?;
        let raw_accessed = store.blob_last_accessed_at(&raw_hash)?;
        store.put_blob(raw)?;
        assert_eq!(store.blob_last_accessed_at(&raw_hash)?, raw_accessed);

        let data = b"cached blossom duplicate";
        let hash = sha256(data);
        store.put_cached_blob(data)?;
        let cached_accessed = store.blob_last_accessed_at(&hash)?;
        store.put_cached_blob(data)?;
        assert_eq!(store.blob_last_accessed_at(&hash)?, cached_accessed);

        let cached_batch = [
            (
                sha256(b"cached blossom batch 1"),
                b"cached blossom batch 1".to_vec(),
            ),
            (
                sha256(b"cached blossom batch 2"),
                b"cached blossom batch 2".to_vec(),
            ),
        ];
        assert_eq!(store.put_cached_blobs(&cached_batch)?, 2);
        assert_eq!(store.put_cached_blobs(&cached_batch)?, 0);
        assert_eq!(
            store.get_blob(&cached_batch[0].0)?.as_deref(),
            Some(cached_batch[0].1.as_slice())
        );

        let owned = b"owned blossom duplicate";
        let owned_hash = sha256(owned);
        let owner = [7u8; 32];
        store.put_owned_blob(owned, &owner)?;
        let owned_accessed = store.blob_last_accessed_at(&owned_hash)?;
        store.put_owned_blob(owned, &owner)?;
        assert_eq!(store.blob_last_accessed_at(&owned_hash)?, owned_accessed);
        let owned_blobs = store.list_blobs_by_pubkey(&owner)?;
        assert_eq!(owned_blobs.len(), 1);
        assert_eq!(owned_blobs[0].sha256, to_hex(&owned_hash));

        let other_owner = [8u8; 32];
        store.put_owned_blob(owned, &other_owner)?;
        assert_eq!(store.blob_last_accessed_at(&owned_hash)?, owned_accessed);
        let other_owned_blobs = store.list_blobs_by_pubkey(&other_owner)?;
        assert_eq!(other_owned_blobs.len(), 1);
        assert_eq!(other_owned_blobs[0].sha256, to_hex(&owned_hash));

        let batch = [
            (
                sha256(b"owned blossom batch 1"),
                b"owned blossom batch 1".to_vec(),
            ),
            (
                sha256(b"owned blossom batch 2"),
                b"owned blossom batch 2".to_vec(),
            ),
        ];
        store.put_owned_blobs(&batch, &owner)?;
        assert_eq!(store.put_owned_blobs(&batch, &owner)?, 0);
        let owned_blobs = store.list_blobs_by_pubkey(&owner)?;
        assert_eq!(owned_blobs.len(), 3);

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn owned_blob_body_survives_concurrent_orphan_cleanup_until_owner_commit() -> Result<()> {
        let temp = TempDir::new()?;
        drop(LocalStore::new_unbounded_with_lmdb_map_size(
            temp.path().join("blobs"),
            &StorageBackend::Lmdb,
            Some(LMDB_BLOB_MIN_MAP_SIZE_BYTES),
        )?);
        let store = Arc::new(HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            LMDB_BLOB_MIN_MAP_SIZE_BYTES,
            true,
            &StorageBackend::Lmdb,
        )?);
        let data = vec![0x5a; 64 * 1024];
        let hash = sha256(&data);
        let owner = [0x42; 32];
        let (body_ready_tx, body_ready_rx) = std::sync::mpsc::channel();
        let (allow_owner_tx, allow_owner_rx) = std::sync::mpsc::channel();

        let writer_store = Arc::clone(&store);
        let writer = std::thread::spawn(move || {
            writer_store.put_owned_blob_with_inserted_after_body(&data, &owner, || {
                body_ready_tx.send(()).expect("signal body write");
                allow_owner_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("owner commit release");
            })
        });

        body_ready_rx.recv_timeout(std::time::Duration::from_secs(10))?;
        assert!(store.blob_exists(&hash)?, "body write must be visible");
        assert!(
            !store.blob_has_owners(&hash)?,
            "test must pause before owner metadata commits"
        );

        assert_eq!(
            store.relieve_cached_blob_write_pressure(64 * 1024)?,
            0,
            "orphan cleanup must skip a body awaiting durable metadata"
        );
        assert!(
            store.blob_exists(&hash)?,
            "concurrent cleanup deleted an in-flight owned body"
        );

        allow_owner_tx.send(())?;
        let (_, inserted) = writer.join().expect("owned writer panicked")?;
        assert!(inserted);
        assert!(store.blob_exists(&hash)?);
        assert!(store.is_blob_owner(&hash, &owner)?);
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn owned_blob_batch_survives_concurrent_orphan_cleanup_until_owner_commit() -> Result<()> {
        let temp = TempDir::new()?;
        drop(LocalStore::new_unbounded_with_lmdb_map_size(
            temp.path().join("blobs"),
            &StorageBackend::Lmdb,
            Some(LMDB_BLOB_MIN_MAP_SIZE_BYTES),
        )?);
        let store = Arc::new(HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            LMDB_BLOB_MIN_MAP_SIZE_BYTES,
            true,
            &StorageBackend::Lmdb,
        )?);
        let first = vec![0x61; 32 * 1024];
        let second = vec![0x62; 48 * 1024];
        let first_hash = sha256(&first);
        let second_hash = sha256(&second);
        let owner = [0x24; 32];
        let items = vec![(first_hash, first), (second_hash, second)];
        let (bodies_ready_tx, bodies_ready_rx) = std::sync::mpsc::channel();
        let (allow_owner_tx, allow_owner_rx) = std::sync::mpsc::channel();

        let writer_store = Arc::clone(&store);
        let writer = std::thread::spawn(move || {
            writer_store.put_owned_blobs_report_after_bodies(&items, &owner, || {
                bodies_ready_tx.send(()).expect("signal batch body write");
                allow_owner_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("batch owner commit release");
            })
        });

        bodies_ready_rx.recv_timeout(std::time::Duration::from_secs(10))?;
        for hash in [first_hash, second_hash] {
            assert!(store.blob_exists(&hash)?, "batch body must be visible");
            assert!(
                !store.blob_has_owners(&hash)?,
                "test must pause before batch owner metadata commits"
            );
        }

        assert_eq!(
            store.relieve_cached_blob_write_pressure(80 * 1024)?,
            0,
            "orphan cleanup must skip all bodies awaiting batch metadata"
        );
        assert!(store.blob_exists(&first_hash)?);
        assert!(store.blob_exists(&second_hash)?);

        allow_owner_tx.send(())?;
        let report = writer.join().expect("owned batch writer panicked")?;
        assert_eq!(report.inserted, 2);
        for hash in [first_hash, second_hash] {
            assert!(store.blob_exists(&hash)?);
            assert!(store.is_blob_owner(&hash, &owner)?);
        }
        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn duplicate_heavy_cached_batch_uses_actual_inserted_bytes_for_quota() -> Result<()> {
        let temp = TempDir::new()?;
        let store = HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            35,
            true,
            &StorageBackend::Lmdb,
        )?;

        let first = [1u8; 10];
        let second = [2u8; 10];
        let third = [3u8; 10];
        let new = [4u8; 5];
        let first_hash = sha256(&first);
        let second_hash = sha256(&second);
        let third_hash = sha256(&third);
        let new_hash = sha256(&new);

        store.put_cached_blob(&first)?;
        store.put_cached_blob(&second)?;
        store.put_cached_blob(&third)?;
        assert_eq!(store.router.writable_stats()?.total_bytes, 30);

        let inserted = store.put_cached_blobs(&[
            (first_hash, first.to_vec()),
            (second_hash, second.to_vec()),
            (new_hash, new.to_vec()),
        ])?;

        assert_eq!(inserted, 1);
        assert_eq!(store.router.writable_stats()?.total_bytes, 35);
        assert!(store.blob_exists(&first_hash)?);
        assert!(store.blob_exists(&second_hash)?);
        assert!(store.blob_exists(&third_hash)?);
        assert!(store.blob_exists(&new_hash)?);

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn replacing_tree_ref_unpins_and_unindexes_superseded_root() -> Result<()> {
        let temp = TempDir::new()?;
        let store = HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            LMDB_BLOB_MIN_MAP_SIZE_BYTES,
            true,
            &StorageBackend::Lmdb,
        )?;

        let old_bytes = b"old published root";
        let new_bytes = b"new published root";
        let old_root = sha256(old_bytes);
        let new_root = sha256(new_bytes);

        store.put_blob(old_bytes)?;
        store.pin(&old_root)?;
        store.index_tree(
            &old_root,
            "owner",
            Some("playlist"),
            PRIORITY_OWN,
            Some("npub1owner/playlist"),
        )?;

        assert!(store.is_pinned(&old_root)?);
        assert!(store.get_tree_meta(&old_root)?.is_some());

        store.put_blob(new_bytes)?;
        store.pin(&new_root)?;
        store.index_tree(
            &new_root,
            "owner",
            Some("playlist"),
            PRIORITY_OWN,
            Some("npub1owner/playlist"),
        )?;

        assert!(
            !store.is_pinned(&old_root)?,
            "superseded root should be unpinned when ref is replaced"
        );
        assert!(
            store.get_tree_meta(&old_root)?.is_none(),
            "superseded root metadata should be removed when ref is replaced"
        );
        assert!(store.is_pinned(&new_root)?);
        assert!(store.get_tree_meta(&new_root)?.is_some());

        Ok(())
    }

    #[test]
    fn tracked_authors_round_trip_sorted_and_deduplicated() -> Result<()> {
        let temp = TempDir::new()?;
        let store = HashtreeStore::with_options(temp.path(), None, 1024 * 1024)?;

        store
            .add_tracked_author("npub1zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzs9d3kk")?;
        store
            .add_tracked_author("npub1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaqf5slm")?;
        store
            .add_tracked_author("npub1zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzs9d3kk")?;

        assert_eq!(
            store.list_tracked_authors()?,
            vec![
                "npub1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaqf5slm".to_string(),
                "npub1zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzs9d3kk".to_string(),
            ]
        );
        assert!(store.remove_tracked_author(
            "npub1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaqf5slm"
        )?);
        assert!(!store.remove_tracked_author(
            "npub1bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbpqqqqq"
        )?);
        assert_eq!(
            store.list_tracked_authors()?,
            vec!["npub1zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzs9d3kk".to_string()]
        );

        Ok(())
    }

    #[cfg(feature = "s3")]
    #[test]
    fn async_store_s3_fallback_does_not_reenter_futures_executor() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let local = Arc::new(LocalStore::new(
            temp.path().join("blobs"),
            &StorageBackend::Fs,
        )?);

        let outcome = std::panic::catch_unwind(|| {
            sync_block_on(async {
                let aws_config = aws_config::from_env()
                    .region(aws_sdk_s3::config::Region::new("auto"))
                    .load()
                    .await;
                let s3_client = aws_sdk_s3::Client::from_conf(
                    aws_sdk_s3::config::Builder::from(&aws_config)
                        .endpoint_url("http://127.0.0.1:9")
                        .force_path_style(true)
                        .build(),
                );

                let router = StorageRouter {
                    local,
                    s3_client: Some(s3_client),
                    s3_bucket: Some("test-bucket".to_string()),
                    s3_prefix: String::new(),
                    sync_tx: None,
                };
                let hash = [0u8; 32];

                let _ = Store::has(&router, &hash).await;
                let _ = Store::get(&router, &hash).await;
            });
        });

        assert!(
            outcome.is_ok(),
            "S3-backed async store methods should not panic inside futures::block_on"
        );

        Ok(())
    }
}
