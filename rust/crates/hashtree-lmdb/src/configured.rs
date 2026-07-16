use crate::{ExternalBlobOptions, LmdbBlobStore};
use async_trait::async_trait;
use hashtree_core::store::{Store, StoreError};
use hashtree_core::types::Hash;
use std::fs;
use std::path::{Path, PathBuf};

const LMDB_HOT_BLOB_DIR_ENV: &str = "HTREE_LMDB_HOT_BLOB_DIR";
const LMDB_HOT_BLOB_LEGACY_DIR_ENV: &str = "HTREE_LMDB_HOT_BLOB_LEGACY_DIR";
const LMDB_HOT_EXTERNAL_BLOB_DIR_ENV: &str = "HTREE_LMDB_HOT_EXTERNAL_BLOB_DIR";
const LMDB_LEGACY_EXTERNAL_BLOB_DIR_ENV: &str = "HTREE_LMDB_LEGACY_EXTERNAL_BLOB_DIR";
pub const LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME: &str = "blob-files-v1";
pub const SHARED_BLOB_MIN_MAP_SIZE_BYTES: u64 = 16 * 1024 * 1024;

/// One canonical LMDB blob layout selected from Hashtree's environment.
///
/// A hot tier, when configured, owns selection between its primary environment
/// and the legacy environment. Callers should expose this whole value as one
/// read route instead of registering each tier separately.
pub enum ConfiguredLmdbBlobStore {
    Single(LmdbBlobStore),
    Tiered {
        primary: Box<LmdbBlobStore>,
        legacy: Box<LmdbBlobStore>,
    },
}

impl ConfiguredLmdbBlobStore {
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        match self {
            Self::Single(store) => store.put_sync(hash, data),
            Self::Tiered { primary, .. } => primary.put_sync(hash, data),
        }
    }

    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        match self {
            Self::Single(store) => store.put_many_sync(items),
            Self::Tiered { primary, .. } => primary.put_many_sync(items),
        }
    }

    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Self::Single(store) => store.get_sync(hash),
            Self::Tiered { primary, legacy } => {
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
            Self::Single(store) => store.get_range_sync(hash, start, end_inclusive),
            Self::Tiered { primary, legacy } => {
                if let Some(data) = primary.get_range_sync(hash, start, end_inclusive)? {
                    return Ok(Some(data));
                }
                legacy.get_range_sync(hash, start, end_inclusive)
            }
        }
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        match self {
            Self::Single(store) => store.blob_size_sync(hash),
            Self::Tiered { primary, legacy } => {
                if let Some(size) = primary.blob_size_sync(hash)? {
                    return Ok(Some(size));
                }
                legacy.blob_size_sync(hash)
            }
        }
    }

    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            Self::Single(store) => store.exists(hash),
            Self::Tiered { primary, legacy } => Ok(primary.exists(hash)? || legacy.exists(hash)?),
        }
    }

    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        match self {
            Self::Single(store) => store.delete_sync(hash),
            Self::Tiered { primary, legacy } => {
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
}

/// Open the unbounded LMDB blob environment(s) for a canonical blob path.
///
/// This owns all environment-based tier and external-marker selection. Every
/// process sharing the path must use this helper under the same environment so
/// marker paths and map options cannot drift between ad-hoc constructors.
pub fn open_configured_lmdb_blob_store<P: AsRef<Path>>(
    legacy_path: P,
    map_size_bytes: Option<u64>,
) -> Result<ConfiguredLmdbBlobStore, StoreError> {
    let legacy_path = legacy_path.as_ref();
    if let Some(hot_path) = lmdb_hot_blob_dir_for(legacy_path) {
        if hot_path != legacy_path {
            let primary = open_unbounded_lmdb_blob_store(
                &hot_path,
                map_size_bytes,
                Some(LMDB_HOT_EXTERNAL_BLOB_DIR_ENV),
            )?;
            let legacy = open_unbounded_lmdb_blob_store(
                legacy_path,
                map_size_bytes,
                Some(LMDB_LEGACY_EXTERNAL_BLOB_DIR_ENV),
            )?;
            return Ok(ConfiguredLmdbBlobStore::Tiered {
                primary: Box::new(primary),
                legacy: Box::new(legacy),
            });
        }
    }

    open_unbounded_lmdb_blob_store(legacy_path, map_size_bytes, None)
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
    open_configured_lmdb_blob_store(
        data_dir.as_ref().join("blobs"),
        Some(storage_budget_bytes.max(SHARED_BLOB_MIN_MAP_SIZE_BYTES)),
    )
}

fn open_unbounded_lmdb_blob_store(
    path: &Path,
    map_size_bytes: Option<u64>,
    external_dir_env: Option<&str>,
) -> Result<LmdbBlobStore, StoreError> {
    fs::create_dir_all(path).map_err(StoreError::Io)?;
    remove_stale_fs_blob_shards(path)?;
    let external_blobs = Some(configured_external_blob_options(path, external_dir_env));
    match map_size_bytes {
        Some(map_size_bytes) => {
            let map_size = usize::try_from(map_size_bytes)
                .map_err(|_| StoreError::Other("LMDB map size exceeds usize".to_string()))?;
            LmdbBlobStore::with_map_size_and_external_blob_options(path, map_size, external_blobs)
        }
        None => LmdbBlobStore::with_external_blob_options(path, external_blobs),
    }
}

fn configured_external_blob_options(
    store_path: &Path,
    override_dir_env: Option<&str>,
) -> ExternalBlobOptions {
    let options = ExternalBlobOptions::from_env(store_path)
        .unwrap_or_else(|| local_add_external_blob_reopen_options(store_path));
    let override_path = override_dir_env
        .and_then(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    match override_path {
        Some(path) => options.with_base_path(path),
        None => options,
    }
}

fn local_add_external_blob_reopen_options(store_path: &Path) -> ExternalBlobOptions {
    ExternalBlobOptions {
        base_path: store_path.with_file_name(LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME),
        min_bytes: usize::MAX,
        sync: true,
        pack_target_bytes: None,
    }
}

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

fn paths_refer_to_same_location(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
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
