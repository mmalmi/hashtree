use crate::{
    ExternalBlobOptions, LmdbBlobStore, PoolMemberConfig, PoolStore, PoolStoreConfig,
    ReadOnlyPoolStore,
};
use async_trait::async_trait;
use hashtree_core::store::{Store, StoreError};
use hashtree_core::types::Hash;
use std::fs;
use std::path::Path;
pub const LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME: &str = "blob-files-v1";
pub const SHARED_BLOB_MIN_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024;
pub const SHARED_BLOB_POOL_DIR_NAME: &str = "blob-pool-v1";
pub const POOL_AUDIT_READ_ONLY_ENV: &str = "HTREE_POOL_AUDIT_READ_ONLY";
pub const POOL_AUDIT_READ_ONLY_ERROR: &str = "PoolStore is in audit-serving read-only mode";
const DEFAULT_POOL_MEMBER_HEADROOM_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// One canonical application-owned LMDB blob store.
pub enum ConfiguredLmdbBlobStore {
    Single(LmdbBlobStore),
    Pool(Box<PoolStore>),
    ReadOnlyPool(Box<ReadOnlyPoolStore>),
}

impl ConfiguredLmdbBlobStore {
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        match self {
            Self::Single(store) => store.put_sync(hash, data),
            Self::Pool(store) => store.put_sync(hash, data),
            Self::ReadOnlyPool(_) => Err(pool_audit_read_only_error()),
        }
    }

    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        match self {
            Self::Single(store) => store.put_many_sync(items),
            Self::Pool(store) => store.put_many_sync(items),
            Self::ReadOnlyPool(_) => Err(pool_audit_read_only_error()),
        }
    }

    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Self::Single(store) => store.get_sync(hash),
            Self::Pool(store) => store.get_sync(hash),
            Self::ReadOnlyPool(store) => store.get_sync(hash),
        }
    }

    pub fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Self::Single(store) => store.get_range_sync(hash, start, end_inclusive),
            Self::Pool(store) => store.get_range_sync(hash, start, end_inclusive),
            Self::ReadOnlyPool(store) => store.get_range_sync(hash, start, end_inclusive),
        }
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        match self {
            Self::Single(store) => store.blob_size_sync(hash),
            Self::Pool(store) => store.blob_size_sync(hash),
            Self::ReadOnlyPool(store) => store.blob_size_sync(hash),
        }
    }

    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            Self::Single(store) => store.exists(hash),
            Self::Pool(store) => store.exists(hash),
            Self::ReadOnlyPool(store) => store.get_sync(hash).map(|data| data.is_some()),
        }
    }

    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            Self::Single(store) => store.delete_sync(hash),
            Self::Pool(store) => store.delete_sync(hash),
            Self::ReadOnlyPool(_) => Err(pool_audit_read_only_error()),
        }
    }

    pub fn delete_many_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        match self {
            Self::Single(store) => store.delete_many_sync(hashes),
            Self::Pool(store) => store.delete_many_sync(hashes),
            Self::ReadOnlyPool(_) => Err(pool_audit_read_only_error()),
        }
    }

    pub fn is_audit_read_only(&self) -> bool {
        matches!(self, Self::ReadOnlyPool(_))
    }
}

#[async_trait]
impl Store for ConfiguredLmdbBlobStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_sync(&items)
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

    async fn stats(&self) -> hashtree_core::store::StoreStats {
        match self {
            Self::Single(store) => {
                let Ok(stats) = store.stats() else {
                    return hashtree_core::store::StoreStats::default();
                };
                hashtree_core::store::StoreStats {
                    count: stats.count as u64,
                    bytes: stats.total_bytes,
                    pinned_count: stats.pinned_count as u64,
                    pinned_bytes: stats.pinned_bytes,
                }
            }
            Self::Pool(store) => store.stats().unwrap_or_default(),
            Self::ReadOnlyPool(_) => hashtree_core::store::StoreStats::default(),
        }
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        match self {
            Self::Single(store) => store.pin(hash).await,
            Self::Pool(store) => store.pin_sync(hash),
            Self::ReadOnlyPool(_) => Err(pool_audit_read_only_error()),
        }
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        match self {
            Self::Single(store) => store.unpin(hash).await,
            Self::Pool(store) => store.unpin_sync(hash),
            Self::ReadOnlyPool(_) => Err(pool_audit_read_only_error()),
        }
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        match self {
            Self::Single(store) => store.pin_count(hash),
            Self::Pool(store) => store.pin_count_sync(hash).unwrap_or(0),
            Self::ReadOnlyPool(_) => 0,
        }
    }
}

/// Open one existing unbounded LMDB environment for a canonical blob path.
pub fn open_configured_lmdb_blob_store<P: AsRef<Path>>(
    path: P,
    map_size_bytes: Option<u64>,
) -> Result<ConfiguredLmdbBlobStore, StoreError> {
    open_unbounded_lmdb_blob_store(path.as_ref(), map_size_bytes)
        .map(ConfiguredLmdbBlobStore::Single)
}

/// Open Hashtree's canonical same-user LMDB blob layout for a data directory.
///
/// `storage_budget_bytes` is converted to the same raw-blob map size used by
/// `HashtreeStore`. The raw environments remain unbounded; application-owned
/// storage metadata retains quota, pin, garbage-collection, and write policy.
pub fn open_shared_lmdb_blob_store<P: AsRef<Path>>(
    data_dir: P,
    storage_budget_bytes: u64,
) -> Result<ConfiguredLmdbBlobStore, StoreError> {
    let audit_read_only = pool_audit_read_only_enabled()?;
    open_shared_lmdb_blob_store_with_audit_mode(
        data_dir.as_ref(),
        storage_budget_bytes,
        audit_read_only,
    )
}

fn open_shared_lmdb_blob_store_with_audit_mode(
    data_dir: &Path,
    storage_budget_bytes: u64,
    audit_read_only: bool,
) -> Result<ConfiguredLmdbBlobStore, StoreError> {
    let blob_path = data_dir.join("blobs");
    let map_size_bytes = storage_budget_bytes.max(SHARED_BLOB_MIN_MAP_SIZE_BYTES);
    let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
    if audit_read_only {
        if !pool_path.join("data.mdb").is_file() {
            return Err(StoreError::Other(format!(
                "{POOL_AUDIT_READ_ONLY_ENV}=1 requires an existing PoolStore at {}",
                pool_path.display()
            )));
        }
        let pool = ReadOnlyPoolStore::open(&pool_path)?;
        return Ok(ConfiguredLmdbBlobStore::ReadOnlyPool(Box::new(pool)));
    }
    if !pool_path.join("data.mdb").exists() && blob_path.join("data.mdb").exists() {
        return open_configured_lmdb_blob_store(&blob_path, Some(map_size_bytes));
    }

    let pool = PoolStore::open(&pool_path, PoolStoreConfig::default())?;
    // Placement capacity is a member guardrail, not the application's quota.
    // Keep bounded staging room so Hashtree can finish and index a raw tree
    // before application-owned retention decides what to evict.
    let capacity = storage_budget_bytes.saturating_add(DEFAULT_POOL_MEMBER_HEADROOM_BYTES);
    let external = configured_external_blob_options(&blob_path);
    let member = PoolMemberConfig::new(blob_path, capacity)
        .with_map_size_bytes(map_size_bytes)
        .with_external_blobs(
            external.base_path,
            external.min_bytes as u64,
            external.sync,
            external.pack_target_bytes.map(|bytes| bytes as u64),
        );
    pool.ensure_initial_member(member)?;
    Ok(ConfiguredLmdbBlobStore::Pool(Box::new(pool)))
}

pub fn pool_audit_read_only_enabled() -> Result<bool, StoreError> {
    let value = std::env::var(POOL_AUDIT_READ_ONLY_ENV);
    match value {
        Ok(value) => parse_pool_audit_read_only(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_pool_audit_read_only(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(StoreError::Other(format!(
            "{POOL_AUDIT_READ_ONLY_ENV} must be exactly 0 or 1"
        ))),
    }
}

fn parse_pool_audit_read_only(value: Option<&str>) -> Result<bool, StoreError> {
    match value {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(StoreError::Other(format!(
            "{POOL_AUDIT_READ_ONLY_ENV} must be exactly 0 or 1"
        ))),
    }
}

fn pool_audit_read_only_error() -> StoreError {
    StoreError::Other(POOL_AUDIT_READ_ONLY_ERROR.into())
}

fn open_unbounded_lmdb_blob_store(
    path: &Path,
    map_size_bytes: Option<u64>,
) -> Result<LmdbBlobStore, StoreError> {
    fs::create_dir_all(path).map_err(StoreError::Io)?;
    remove_stale_fs_blob_shards(path)?;
    let external_blobs = Some(configured_external_blob_options(path));
    match map_size_bytes {
        Some(map_size_bytes) => {
            let map_size = usize::try_from(map_size_bytes)
                .map_err(|_| StoreError::Other("LMDB map size exceeds usize".to_string()))?;
            LmdbBlobStore::with_map_size_and_external_blob_options(path, map_size, external_blobs)
        }
        None => LmdbBlobStore::with_external_blob_options(path, external_blobs),
    }
}

fn configured_external_blob_options(store_path: &Path) -> ExternalBlobOptions {
    ExternalBlobOptions::from_env(store_path)
        .unwrap_or_else(|| local_add_external_blob_reopen_options(store_path))
}

fn local_add_external_blob_reopen_options(store_path: &Path) -> ExternalBlobOptions {
    ExternalBlobOptions {
        base_path: store_path.with_file_name(LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME),
        min_bytes: usize::MAX,
        sync: true,
        pack_target_bytes: None,
    }
}

fn remove_stale_fs_blob_shards(path: &Path) -> Result<(), StoreError> {
    let entries = fs::read_dir(path).map_err(StoreError::Io)?;
    for entry in entries {
        let entry = entry.map_err(StoreError::Io)?;
        let entry_path = entry.path();
        if entry_path.is_dir() && is_fs_blob_shard_dir(&entry_path) {
            fs::remove_dir_all(&entry_path).map_err(StoreError::Io)?;
        }
    }
    Ok(())
}

fn is_fs_blob_shard_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.len() == 2 && name.as_bytes().iter().all(u8::is_ascii_hexdigit))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::sha256;
    use tempfile::TempDir;

    #[test]
    fn audit_read_only_environment_is_strict() {
        assert!(!parse_pool_audit_read_only(None).expect("unset"));
        assert!(!parse_pool_audit_read_only(Some("0")).expect("disabled"));
        assert!(parse_pool_audit_read_only(Some("1")).expect("enabled"));
        for invalid in ["", "true", "yes", " 1", "1 "] {
            assert!(
                parse_pool_audit_read_only(Some(invalid)).is_err(),
                "{invalid:?} must fail closed"
            );
        }
    }

    #[test]
    fn shared_pool_audit_mode_serves_reads_and_rejects_writes() {
        let temp = TempDir::new().expect("temp dir");
        let data = b"shared audit-serving bytes";
        let hash = sha256(data);
        let writable = open_shared_lmdb_blob_store_with_audit_mode(temp.path(), 1024 * 1024, false)
            .expect("open writable shared pool");
        writable.put_sync(hash, data).expect("seed shared pool");
        drop(writable);

        let audit = open_shared_lmdb_blob_store_with_audit_mode(temp.path(), 1024 * 1024, true)
            .expect("open audit-serving shared pool");
        assert_eq!(audit.get_sync(&hash).expect("read"), Some(data.to_vec()));
        let error = audit
            .put_sync(hash, data)
            .expect_err("audit-serving shared pool must reject writes");
        assert!(error
            .to_string()
            .contains(crate::POOL_AUDIT_READ_ONLY_ERROR));
    }
}
