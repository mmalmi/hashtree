use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::executor::block_on as sync_block_on;
use futures::StreamExt;
use hashtree_config::StorageBackend;
use hashtree_core::store::{PutManyReport, Store, StoreError};
use hashtree_core::{
    from_hex, sha256, to_hex, types::Hash, Cid, HashTree, HashTreeConfig, TreeNode,
};
use hashtree_fs::FsBlobStore;
#[cfg(feature = "lmdb")]
use hashtree_lmdb::LmdbBlobStore;
use heed::types::*;
use heed::{Database, EnvFlags, EnvOpenOptions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(feature = "s3")]
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod upload;
pub use upload::{AddProgress, AddProgressSnapshot};

mod maintenance;
mod retention;

#[cfg(feature = "s3")]
const DEFAULT_S3_SYNC_TIMEOUT_MS: u64 = 5_000;
#[cfg(feature = "s3")]
const S3_SYNC_TIMEOUT_MS_ENV: &str = "HTREE_S3_SYNC_TIMEOUT_MS";

pub use maintenance::{
    compact_lmdb_environments_under, CompactResult, R2ImportOptions, R2ImportResult, VerifyResult,
};
pub use retention::{OwnedBlobStats, PinnedItem, StorageByPriority, StorageStats, TreeMeta};

/// Priority levels for tree eviction
pub const PRIORITY_OTHER: u8 = 64;
pub const PRIORITY_FOLLOWED: u8 = 128;
pub const PRIORITY_OWN: u8 = 255;
const LMDB_MAX_READERS: u32 = 1024;
const LMDB_METADATA_MIN_MAP_SIZE_BYTES: u64 = 64 * 1024 * 1024;
const LMDB_METADATA_MAX_MAP_SIZE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const LMDB_METADATA_STORAGE_RATIO_DIVISOR: u64 = 1024;
const LMDB_METADATA_REOPEN_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(feature = "lmdb")]
const LMDB_BLOB_MIN_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024;
const ACCESS_UPDATE_INTERVAL_SECS: u64 = 300;
const ACCESS_UPDATE_GATE_MAX_ENTRIES: usize = 4096;
const DEFAULT_ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT: usize = 64;
const ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT_ENV: &str = "HTREE_ACCESS_UPDATE_BACKGROUND_BATCH_LIMIT";
const SLOW_OWNED_BLOB_BATCH_LOG_MS_ENV: &str = "HTREE_SLOW_OWNED_BLOB_BATCH_LOG_MS";
const SLOW_CACHED_BLOB_BATCH_LOG_MS_ENV: &str = "HTREE_SLOW_CACHED_BLOB_BATCH_LOG_MS";
#[cfg(feature = "lmdb")]
const LMDB_HOT_BLOB_DIR_ENV: &str = "HTREE_LMDB_HOT_BLOB_DIR";
#[cfg(feature = "lmdb")]
const LMDB_HOT_BLOB_LEGACY_DIR_ENV: &str = "HTREE_LMDB_HOT_BLOB_LEGACY_DIR";

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
    TieredLmdb {
        primary: Box<LmdbBlobStore>,
        legacy: Box<LmdbBlobStore>,
    },
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
fn lmdb_hot_blob_dir_for(legacy_path: &Path) -> Option<PathBuf> {
    if let Ok(expected_legacy) = std::env::var(LMDB_HOT_BLOB_LEGACY_DIR_ENV) {
        let expected_legacy = expected_legacy.trim();
        if !expected_legacy.is_empty()
            && !paths_refer_to_same_location(legacy_path, Path::new(expected_legacy))
        {
            return None;
        }
    }

    std::env::var(LMDB_HOT_BLOB_DIR_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
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
fn open_lmdb_blob_store<P: AsRef<Path>>(
    path: P,
    map_size_bytes: Option<u64>,
) -> Result<LmdbBlobStore, StoreError> {
    std::fs::create_dir_all(path.as_ref()).map_err(StoreError::Io)?;
    remove_stale_fs_blob_shards(path.as_ref())?;
    match map_size_bytes {
        Some(map_size_bytes) => LmdbBlobStore::with_max_bytes(path, map_size_bytes),
        None => LmdbBlobStore::new(path),
    }
}

#[cfg(feature = "lmdb")]
fn open_unbounded_lmdb_blob_store<P: AsRef<Path>>(
    path: P,
    map_size_bytes: Option<u64>,
) -> Result<LmdbBlobStore, StoreError> {
    std::fs::create_dir_all(path.as_ref()).map_err(StoreError::Io)?;
    remove_stale_fs_blob_shards(path.as_ref())?;
    match map_size_bytes {
        Some(map_size_bytes) => {
            let map_size = usize::try_from(align_lmdb_map_size(map_size_bytes))
                .map_err(|_| StoreError::Other("LMDB map size exceeds usize".to_string()))?;
            LmdbBlobStore::with_map_size(path, map_size)
        }
        None => LmdbBlobStore::new(path),
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
            StorageBackend::Lmdb => {
                if let Some(hot_path) = lmdb_hot_blob_dir_for(path.as_ref()) {
                    let legacy_path = path.as_ref().to_path_buf();
                    if hot_path != legacy_path {
                        let primary = open_lmdb_blob_store(&hot_path, _map_size_bytes)?;
                        let legacy = open_lmdb_blob_store(&legacy_path, _map_size_bytes)?;
                        tracing::info!(
                            "Using tiered LMDB blob storage: primary={}, legacy={}",
                            hot_path.display(),
                            legacy_path.display()
                        );
                        return Ok(LocalStore::TieredLmdb {
                            primary: Box::new(primary),
                            legacy: Box::new(legacy),
                        });
                    }
                }
                Ok(LocalStore::Lmdb(open_lmdb_blob_store(
                    path,
                    _map_size_bytes,
                )?))
            }
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
    /// with richer retention policy. In tiered LMDB mode, both the hot primary
    /// and cold legacy envs are opened without adapter-level eviction so merely
    /// enabling the hot tier can never reclaim legacy data on startup.
    pub fn new_unbounded_with_lmdb_map_size<P: AsRef<Path>>(
        path: P,
        backend: &StorageBackend,
        _map_size_bytes: Option<u64>,
    ) -> Result<Self, StoreError> {
        match backend {
            StorageBackend::Fs => Ok(LocalStore::Fs(FsBlobStore::new(path)?)),
            #[cfg(feature = "lmdb")]
            StorageBackend::Lmdb => {
                if let Some(hot_path) = lmdb_hot_blob_dir_for(path.as_ref()) {
                    let legacy_path = path.as_ref().to_path_buf();
                    if hot_path != legacy_path {
                        let primary = open_unbounded_lmdb_blob_store(&hot_path, _map_size_bytes)?;
                        let legacy = open_unbounded_lmdb_blob_store(&legacy_path, _map_size_bytes)?;
                        tracing::info!(
                            "Using tiered LMDB blob storage: primary={}, legacy={}",
                            hot_path.display(),
                            legacy_path.display()
                        );
                        return Ok(LocalStore::TieredLmdb {
                            primary: Box::new(primary),
                            legacy: Box::new(legacy),
                        });
                    }
                }
                Ok(LocalStore::Lmdb(open_unbounded_lmdb_blob_store(
                    path,
                    _map_size_bytes,
                )?))
            }
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
            LocalStore::Lmdb(_) | LocalStore::TieredLmdb { .. } => StorageBackend::Lmdb,
        }
    }

    /// Sync put operation
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.put_sync(hash, data),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.put_sync(hash, data),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, .. } => primary.put_sync(hash, data),
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
            LocalStore::TieredLmdb { primary, .. } => primary.put_many_report_sync(items),
        }
    }

    /// Sync batch put operation.
    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_report_sync(items)
            .map(|report| report.inserted)
    }

    /// Sync get operation
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, legacy } => {
                if let Some(data) = primary.get_sync(hash)? {
                    return Ok(Some(data));
                }
                legacy.get_sync(hash)
            }
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
            LocalStore::TieredLmdb { primary, legacy } => {
                if primary.exists(hash)? {
                    return primary.get_range_sync(hash, start, end_inclusive);
                }
                legacy.get_range_sync(hash, start, end_inclusive)
            }
        }
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.blob_size_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.blob_size_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, legacy } => {
                if let Some(size) = primary.blob_size_sync(hash)? {
                    return Ok(Some(size));
                }
                legacy.blob_size_sync(hash)
            }
        }
    }

    pub fn touch_accessed_sync(&self, hash: &Hash, now: u64) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.touch_accessed_sync(hash, now),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.touch_accessed_sync(hash, now),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, .. } => {
                if primary.exists(hash)? {
                    return primary.touch_accessed_sync(hash, now);
                }
                Ok(false)
            }
        }
    }

    pub fn touch_many_accessed_sync(&self, hashes: &[Hash], now: u64) -> Result<usize, StoreError> {
        match self {
            LocalStore::Fs(store) => store.touch_many_accessed_sync(hashes, now),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.touch_many_accessed_sync(hashes, now),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, .. } => {
                let mut primary_hashes = Vec::new();
                for hash in hashes {
                    if primary.exists(hash)? {
                        primary_hashes.push(*hash);
                    }
                }
                primary.touch_many_accessed_sync(&primary_hashes, now)
            }
        }
    }

    pub fn last_accessed_at_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.last_accessed_at_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.last_accessed_at_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, legacy } => {
                if let Some(accessed_at) = primary.last_accessed_at_sync(hash)? {
                    return Ok(Some(accessed_at));
                }
                legacy.last_accessed_at_sync(hash)
            }
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
            LocalStore::TieredLmdb { primary, legacy } => {
                let mut results = primary.many_last_accessed_at_sync(hashes)?;
                let found: HashSet<Hash> = results.iter().map(|(hash, _)| *hash).collect();
                let missing = hashes
                    .iter()
                    .copied()
                    .filter(|hash| !found.contains(hash))
                    .collect::<Vec<_>>();
                results.extend(legacy.many_last_accessed_at_sync(&missing)?);
                Ok(results)
            }
        }
    }

    /// Check if hash exists
    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => Ok(store.exists(hash)),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.exists(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, legacy } => {
                Ok(primary.exists(hash)? || legacy.exists(hash)?)
            }
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
            LocalStore::TieredLmdb { primary, legacy } => {
                let mut existing = primary.existing_hashes_in_sorted_candidates(sorted_hashes)?;
                let missing = sorted_hashes
                    .iter()
                    .copied()
                    .zip(existing.iter().copied())
                    .filter_map(|(hash, exists)| (!exists).then_some(hash))
                    .collect::<Vec<_>>();
                if missing.is_empty() {
                    return Ok(existing);
                }
                let legacy_existing = legacy.existing_hashes_in_sorted_candidates(&missing)?;
                let legacy_existing_by_hash: HashSet<Hash> = missing
                    .into_iter()
                    .zip(legacy_existing)
                    .filter_map(|(hash, exists)| exists.then_some(hash))
                    .collect();
                for (hash, exists) in sorted_hashes.iter().zip(existing.iter_mut()) {
                    *exists |= legacy_existing_by_hash.contains(hash);
                }
                Ok(existing)
            }
        }
    }

    /// Sync delete operation
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, legacy } => {
                let deleted_primary = if primary.exists(hash)? {
                    primary.delete_sync(hash)?
                } else {
                    false
                };
                let deleted_legacy = if legacy.exists(hash)? {
                    legacy.delete_sync(hash)?
                } else {
                    false
                };
                Ok(deleted_primary || deleted_legacy)
            }
        }
    }

    pub fn delete_writable_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, .. } => {
                if primary.exists(hash)? {
                    primary.delete_sync(hash)
                } else {
                    Ok(false)
                }
            }
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
            LocalStore::TieredLmdb { primary, legacy } => {
                let primary_stats = primary.stats()?;
                let legacy_stats = legacy.stats()?;
                Ok(LocalStoreStats {
                    count: primary_stats.count.saturating_add(legacy_stats.count),
                    total_bytes: primary_stats
                        .total_bytes
                        .saturating_add(legacy_stats.total_bytes),
                })
            }
        }
    }

    /// Get storage statistics for the tier that receives new writes.
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
            LocalStore::TieredLmdb { primary, .. } => {
                let stats = primary.stats()?;
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
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, legacy } => {
                let mut hashes = primary.list()?;
                let mut seen: HashSet<Hash> = hashes.iter().copied().collect();
                for hash in legacy.list()? {
                    if seen.insert(hash) {
                        hashes.push(hash);
                    }
                }
                Ok(hashes)
            }
        }
    }

    /// List hashes in the tier that receives new writes.
    pub fn list_writable(&self) -> Result<Vec<Hash>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.list(),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.list(),
            #[cfg(feature = "lmdb")]
            LocalStore::TieredLmdb { primary, .. } => primary.list(),
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

    /// Delete data from local store only (don't propagate to S3)
    /// Used for eviction where we want to keep archives and cold tiers intact.
    pub fn delete_local_only(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local.delete_writable_sync(hash)
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

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
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
    base_path: PathBuf,
    env: heed::Env,
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
    /// Best-effort in-memory throttle for blob access metadata writes.
    blob_access_update_gate: BlobAccessUpdateGate,
    /// Keeps access-time maintenance out of foreground blob reads.
    blob_access_update_inflight: Arc<AtomicBool>,
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
        let env = unsafe { env_options.open(path)? };
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
        #[cfg(feature = "lmdb")]
        let blob_map_size = Some(max_size_bytes.max(LMDB_BLOB_MIN_MAP_SIZE_BYTES));
        #[cfg(not(feature = "lmdb"))]
        let blob_map_size = None;
        let local_store = Arc::new(
            LocalStore::new_unbounded_with_lmdb_map_size(
                path.join("blobs"),
                backend,
                blob_map_size,
            )
            .map_err(|e| anyhow::anyhow!("Failed to create blob store: {}", e))?,
        );

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
            blob_access_update_gate: BlobAccessUpdateGate::default(),
            blob_access_update_inflight: Arc::new(AtomicBool::new(false)),
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

    fn record_blob_access_now(&self, hash: &Hash) {
        if let Err(err) = self.router.touch_accessed_sync(hash, unix_timestamp_now()) {
            tracing::debug!("Failed to update blob access metadata: {}", err);
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
        let inserted = self
            .router
            .put_sync(hash, data)
            .map_err(|e| anyhow::anyhow!("Failed to store blob: {}", e))?;
        if !inserted {
            self.record_blob_access_now(&hash);
        }
        Ok(to_hex(&hash))
    }

    /// Store an owned Blossom blob under the configured durable storage limit.
    pub fn put_owned_blob(&self, data: &[u8], pubkey: &[u8; 32]) -> Result<String> {
        let hash = sha256(data);
        let incoming_bytes = data.len() as u64;
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

        if !inserted {
            self.record_blob_access_now(&hash);
        }

        self.set_blob_owner_with_size(&hash, pubkey, incoming_bytes)?;
        if inserted {
            if let Err(err) = self.enforce_durable_blob_budget_after_insert(incoming_bytes) {
                let _ = self.delete_blossom_blob(&hash, pubkey);
                return Err(err);
            }
        }

        Ok(to_hex(&hash))
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
            self.blob_owners.put(&mut wtxn, &owner_key[..], &())?;

            let index_key = Self::pubkey_blob_key(pubkey, hash);
            if self.pubkey_blob_index.get(&wtxn, &index_key[..])?.is_none() {
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
        let started_at = Instant::now();
        let slow_log_ms = slow_owned_blob_batch_log_ms();
        if items.is_empty() {
            return Ok(PutManyReport::default());
        }
        let incoming_bytes = items.iter().fold(0u64, |total, (_, data)| {
            total.saturating_add(data.len() as u64)
        });
        let count = items.len();
        let raw_started = Instant::now();
        let report = self.put_many_durable_blob_bodies(items, incoming_bytes)?;
        let raw_write_ms = raw_started.elapsed().as_millis();

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
    pub fn put_cached_blob(&self, data: &[u8]) -> Result<String> {
        let hash = sha256(data);
        let incoming_bytes = data.len() as u64;

        let mut retried_after_cleanup = false;
        loop {
            match self.router.put_sync(hash, data) {
                Ok(inserted) => {
                    if !inserted {
                        self.record_blob_access_now(&hash);
                    } else if let Err(err) =
                        self.enforce_cached_blob_budget_after_insert(incoming_bytes)
                    {
                        tracing::debug!("Failed to enforce cached blob budget: {}", err);
                    }
                    return Ok(to_hex(&hash));
                }
                Err(err) if !retried_after_cleanup && is_map_full_store_error(&err) => {
                    let freed = self.relieve_cached_blob_write_pressure(incoming_bytes)?;
                    if freed == 0 {
                        return Err(anyhow::anyhow!("Failed to store cached blob: {}", err));
                    }
                    retried_after_cleanup = true;
                }
                Err(err) => return Err(anyhow::anyhow!("Failed to store cached blob: {}", err)),
            }
        }
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

        let mut retried_after_cleanup = false;
        loop {
            let raw_started = Instant::now();
            match self.router.put_many_report_sync(items) {
                Ok(report) => {
                    let raw_write_ms = raw_started.elapsed().as_millis();
                    let quota_started = Instant::now();
                    if report.inserted_bytes > 0 {
                        if let Err(err) =
                            self.enforce_cached_blob_budget_after_insert(report.inserted_bytes)
                        {
                            tracing::debug!("Failed to enforce cached blob budget: {}", err);
                        }
                    }
                    let quota_ms = quota_started.elapsed().as_millis();
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
                    let freed = self.relieve_cached_blob_write_pressure(candidate_bytes)?;
                    if freed == 0 {
                        return Err(anyhow::anyhow!(
                            "Failed to store cached blob batch: {}",
                            err
                        ));
                    }
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

        // Add ownership entry (idempotent - put overwrites)
        self.blob_owners.put(&mut wtxn, &key[..], &())?;

        if self.pubkey_blob_index.get(&wtxn, &index_key[..])?.is_none() {
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
            self.pubkey_blob_index.put(
                &mut wtxn,
                &index_key[..],
                &serde_json::to_vec(&metadata)?,
            )?;
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

        // Delete raw blob (by content hash) - this deletes from S3 too
        let _ = self.router.delete_sync(sha256);

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
    pub fn get_file_chunk_metadata(&self, hash: &[u8; 32]) -> Result<Option<FileChunkMetadata>> {
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
                return Ok(Some(FileChunkMetadata {
                    total_size,
                    chunk_hashes: vec![],
                    chunk_sizes: vec![],
                    is_chunked: false,
                }));
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

            Ok(Some(FileChunkMetadata {
                total_size,
                chunk_hashes,
                chunk_sizes,
                is_chunked: !node.links.is_empty(),
            }))
        });
        let metadata = metadata?;
        if metadata.is_some() {
            self.record_blob_accesses(access_store.take_accessed_hashes());
        }
        Ok(metadata)
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
                return Some(Err(anyhow::anyhow!(
                    "Chunk {} not found",
                    to_hex(chunk_hash)
                )));
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
        let hash = from_hex(hash_hex).map_err(|e| anyhow::anyhow!("Invalid hash: {}", e))?;
        self.get_chunk(&hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "lmdb")]
    use std::ffi::OsString;
    #[cfg(feature = "lmdb")]
    use std::path::Path;
    #[cfg(feature = "lmdb")]
    use std::sync::Mutex;
    #[cfg(feature = "lmdb")]
    use tempfile::TempDir;

    #[cfg(feature = "lmdb")]
    static HOT_BLOB_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(feature = "lmdb")]
    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    #[cfg(feature = "lmdb")]
    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    #[cfg(feature = "lmdb")]
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
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
            LocalStore::TieredLmdb { primary, .. } => primary.map_size_bytes() as u64,
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
            LocalStore::TieredLmdb { primary, .. } => primary.map_size_bytes() as u64,
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
    fn lmdb_hot_blob_legacy_guard_scopes_tiered_store() -> Result<()> {
        let _lock = HOT_BLOB_ENV_LOCK.lock().unwrap();
        let temp = TempDir::new()?;
        let legacy = temp.path().join("legacy-blobs");
        let unrelated = temp.path().join("unrelated-blobs");
        let hot = temp.path().join("hot-blobs");
        let _hot_guard = EnvGuard::set(LMDB_HOT_BLOB_DIR_ENV, &hot);
        let _legacy_guard = EnvGuard::set(LMDB_HOT_BLOB_LEGACY_DIR_ENV, &legacy);

        let store = LocalStore::new_with_lmdb_map_size(
            &legacy,
            &StorageBackend::Lmdb,
            Some(128 * 1024 * 1024),
        )?;
        assert!(matches!(store, LocalStore::TieredLmdb { .. }));

        let store = LocalStore::new_with_lmdb_map_size(
            &unrelated,
            &StorageBackend::Lmdb,
            Some(128 * 1024 * 1024),
        )?;
        assert!(matches!(store, LocalStore::Lmdb(_)));

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn hashtree_store_uses_scoped_lmdb_hot_blob_dir() -> Result<()> {
        let _lock = HOT_BLOB_ENV_LOCK.lock().unwrap();
        let temp = TempDir::new()?;
        let data_dir = temp.path().join("store");
        let hot = temp.path().join("hot-main-blobs");
        let legacy = data_dir.join("blobs");
        let _hot_guard = EnvGuard::set(LMDB_HOT_BLOB_DIR_ENV, &hot);
        let _legacy_guard = EnvGuard::set(LMDB_HOT_BLOB_LEGACY_DIR_ENV, &legacy);

        let store = HashtreeStore::with_options_and_backend(
            &data_dir,
            None,
            128 * 1024 * 1024,
            true,
            &StorageBackend::Lmdb,
        )?;

        let local = store.router.local_store();
        assert!(matches!(local.as_ref(), LocalStore::TieredLmdb { .. }));

        Ok(())
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn tiered_lmdb_legacy_bytes_do_not_drive_hot_quota() -> Result<()> {
        let _lock = HOT_BLOB_ENV_LOCK.lock().unwrap();
        let temp = TempDir::new()?;
        let data_dir = temp.path().join("store");
        let hot = temp.path().join("hot-main-blobs");
        let legacy = data_dir.join("blobs");
        let legacy_blob = vec![7u8; 10 * 1024 * 1024];
        let legacy_hash = sha256(&legacy_blob);
        let hot_blob = vec![3u8; 8 * 1024 * 1024];

        let _hot_guard = EnvGuard::set(LMDB_HOT_BLOB_DIR_ENV, &hot);
        let _legacy_guard = EnvGuard::set(LMDB_HOT_BLOB_LEGACY_DIR_ENV, &legacy);
        let store = HashtreeStore::with_options_and_backend(
            &data_dir,
            None,
            LMDB_BLOB_MIN_MAP_SIZE_BYTES,
            true,
            &StorageBackend::Lmdb,
        )?;

        let local = store.router.local_store();
        match local.as_ref() {
            LocalStore::TieredLmdb { primary: _, legacy } => {
                assert_eq!(legacy.max_bytes(), None);
                assert!(legacy.put_sync(legacy_hash, &legacy_blob)?);
            }
            _ => panic!("expected tiered LMDB local store"),
        }

        assert!(store.blob_exists(&legacy_hash)?);
        assert_eq!(
            store.blob_size(&legacy_hash)?,
            Some(legacy_blob.len() as u64)
        );
        assert_eq!(store.router.writable_stats()?.total_bytes, 0);

        let pubkey = [1u8; 32];
        let hot_hash_hex = store.put_owned_blob(&hot_blob, &pubkey)?;
        let hot_hash = from_hex(&hot_hash_hex)?;
        assert_eq!(store.blob_size(&hot_hash)?, Some(hot_blob.len() as u64));
        assert!(store.blob_exists(&legacy_hash)?);
        assert!(!store.router.delete_local_only(&legacy_hash)?);
        assert!(store.blob_exists(&legacy_hash)?);

        let local = store.router.local_store();
        match local.as_ref() {
            LocalStore::TieredLmdb { primary, legacy } => {
                assert!(primary.exists(&hot_hash)?);
                assert!(!primary.exists(&legacy_hash)?);
                assert!(legacy.exists(&legacy_hash)?);
            }
            _ => panic!("expected tiered LMDB local store"),
        }

        let writable_stats = store.router.writable_stats()?;
        assert_eq!(writable_stats.count, 1);
        assert_eq!(writable_stats.total_bytes, hot_blob.len() as u64);

        drop(store);
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
    fn duplicate_blossom_writes_refresh_blob_last_accessed() -> Result<()> {
        let temp = TempDir::new()?;
        let store = HashtreeStore::with_options_and_backend(
            temp.path(),
            None,
            LMDB_BLOB_MIN_MAP_SIZE_BYTES,
            true,
            &StorageBackend::Lmdb,
        )?;

        let data = b"cached blossom duplicate";
        let hash = sha256(data);
        store.put_cached_blob(data)?;
        store.router.touch_accessed_sync(&hash, 1)?;
        store.put_cached_blob(data)?;
        assert!(store.blob_last_accessed_at(&hash)?.unwrap_or(0) > 1);

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
        store.router.touch_accessed_sync(&owned_hash, 1)?;
        store.put_owned_blob(owned, &owner)?;
        assert!(store.blob_last_accessed_at(&owned_hash)?.unwrap_or(0) > 1);
        let owned_blobs = store.list_blobs_by_pubkey(&owner)?;
        assert_eq!(owned_blobs.len(), 1);
        assert_eq!(owned_blobs[0].sha256, to_hex(&owned_hash));

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
        let owned_blobs = store.list_blobs_by_pubkey(&owner)?;
        assert_eq!(owned_blobs.len(), 3);

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
