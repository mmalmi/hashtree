use super::member::verify_member_path;
use super::{
    decode_manifest, map_heed, LocationRecord, PoolManifest, PoolMemberConfig, PoolMemberId,
    CATALOG_DATABASES, CATALOG_MAX_READERS, EXTERNAL_MARKER_NAME, MANIFEST_KEY, MEMBER_MARKER_NAME,
};
use crate::{managed_env::ManagedEnv, ExternalBlobOptions, LmdbBlobReader};
use async_trait::async_trait;
use hashtree_core::store::{slice_blob_range, Store, StoreError, StoreStats};
use hashtree_core::{sha256, to_hex, types::Hash};
use heed::types::Bytes;
use heed::{Database, EnvFlags, EnvOpenOptions};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

struct ReadOnlyPoolStoreInner {
    // Keep the catalog environment alive for the named database handle.
    _env: ManagedEnv,
    manifest: Database<Bytes, Bytes>,
    opened_manifest_bytes: Vec<u8>,
    opened_manifest_sha256: String,
    locations: Database<Bytes, Bytes>,
    members: HashMap<PoolMemberId, Arc<LmdbBlobReader>>,
}

/// Strictly read-only view of an existing PoolStore catalog and its members.
///
/// Unlike [`super::PoolStore::open`], this constructor never creates a path,
/// opens a write transaction, repairs pending records, records temperature
/// samples, or starts a worker. It is intended for live-data verification.
/// LMDB can still update reader bookkeeping in `lock.mdb`; `data.mdb` remains
/// read-only.
#[derive(Clone)]
pub struct ReadOnlyPoolStore {
    inner: Arc<ReadOnlyPoolStoreInner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyPoolCatalogAudit {
    pub stored_locations: u64,
    pub sha256: String,
    pub manifest_sha256: String,
}

fn require_opened_manifest(
    current: &[u8],
    opened: &[u8],
    opened_sha256: &str,
) -> Result<(), StoreError> {
    if current != opened {
        return Err(StoreError::Other(format!(
            "pool manifest changed after read-only members were opened: expected {opened_sha256}, found {}",
            to_hex(&sha256(current))
        )));
    }
    Ok(())
}

impl ReadOnlyPoolStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if !path.join("data.mdb").is_file() {
            return Err(StoreError::Other(format!(
                "read-only pool catalog is missing at {}",
                path.display()
            )));
        }

        let mut options = EnvOpenOptions::new();
        options
            .max_dbs(CATALOG_DATABASES)
            .max_readers(CATALOG_MAX_READERS);
        unsafe {
            options.flags(EnvFlags::READ_ONLY | EnvFlags::NO_READ_AHEAD);
        }
        let env = unsafe { ManagedEnv::open(&options, path) }.map_err(|error| {
            StoreError::Other(format!(
                "open read-only pool catalog {}: {error}",
                path.display()
            ))
        })?;
        let rtxn = env.read_txn().map_err(map_heed)?;
        let manifest_db = env
            .open_database::<Bytes, Bytes>(&rtxn, Some("manifest"))
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest database is missing".into()))?;
        let locations = env
            .open_database::<Bytes, Bytes>(&rtxn, Some("locations"))
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool locations database is missing".into()))?;
        let manifest_bytes = manifest_db
            .get(&rtxn, MANIFEST_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
        let opened_manifest_bytes = manifest_bytes.to_vec();
        let opened_manifest_sha256 = to_hex(&sha256(&opened_manifest_bytes));
        let manifest = decode_manifest(&opened_manifest_bytes)?;
        // Publish DBI handles before later transactions use them.
        rtxn.commit().map_err(map_heed)?;

        let members = open_members(&manifest)?;
        Ok(Self {
            inner: Arc::new(ReadOnlyPoolStoreInner {
                _env: env,
                manifest: manifest_db,
                opened_manifest_bytes,
                opened_manifest_sha256,
                locations,
                members,
            }),
        })
    }

    fn read_location(&self, hash: &Hash) -> Result<Option<LocationRecord>, StoreError> {
        let rtxn = self.inner._env.read_txn().map_err(map_heed)?;
        self.inner
            .locations
            .get(&rtxn, hash)
            .map_err(map_heed)?
            .map(LocationRecord::decode)
            .transpose()
    }

    /// Prove that the entire catalog contains only committed locations.
    ///
    /// Terminal verification must not silently read through an interrupted
    /// pending write or member move, even when one of its copies happens to
    /// contain valid bytes.
    pub fn validate_committed_catalog(&self) -> Result<ReadOnlyPoolCatalogAudit, StoreError> {
        let rtxn = self.inner._env.read_txn().map_err(map_heed)?;
        let manifest = self
            .inner
            .manifest
            .get(&rtxn, MANIFEST_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
        require_opened_manifest(
            manifest,
            &self.inner.opened_manifest_bytes,
            &self.inner.opened_manifest_sha256,
        )?;
        // Decode again so corruption cannot be hidden by byte equality alone.
        decode_manifest(manifest)?;
        let mut digest = Sha256::new();
        digest.update(b"hashtree-read-only-pool-catalog-v1\0");
        digest.update((manifest.len() as u64).to_be_bytes());
        digest.update(manifest);
        let mut stored = 0u64;
        for item in self.inner.locations.iter(&rtxn).map_err(map_heed)? {
            let (hash, encoded) = item.map_err(map_heed)?;
            let hash: Hash = hash
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool catalog hash key".into()))?;
            let location = LocationRecord::decode(encoded)?;
            match location {
                LocationRecord::Stored { member, .. } => {
                    if !self.inner.members.contains_key(&member) {
                        return Err(StoreError::Other(format!(
                            "pool catalog location {} references unknown member {member}",
                            to_hex(&hash)
                        )));
                    }
                    stored = stored.checked_add(1).ok_or_else(|| {
                        StoreError::Other("pool catalog location count overflow".into())
                    })?;
                }
                LocationRecord::Pending { .. } => {
                    return Err(StoreError::Other(format!(
                        "pool catalog contains pending location {}",
                        to_hex(&hash)
                    )));
                }
                LocationRecord::Moving { .. } => {
                    return Err(StoreError::Other(format!(
                        "pool catalog contains moving location {}",
                        to_hex(&hash)
                    )));
                }
            }
            digest.update(hash);
            digest.update((encoded.len() as u64).to_be_bytes());
            digest.update(encoded);
        }
        let digest: Hash = digest.finalize().into();
        Ok(ReadOnlyPoolCatalogAudit {
            stored_locations: stored,
            sha256: to_hex(&digest),
            manifest_sha256: self.inner.opened_manifest_sha256.clone(),
        })
    }

    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(location) = self.read_location(hash)? else {
            return Ok(None);
        };
        let id = match location {
            LocationRecord::Stored { member, .. } => member,
            LocationRecord::Pending { .. } => {
                return Err(StoreError::Other(format!(
                    "read-only terminal verification rejected pending pool location {}",
                    to_hex(hash)
                )));
            }
            LocationRecord::Moving { .. } => {
                return Err(StoreError::Other(format!(
                    "read-only terminal verification rejected moving pool location {}",
                    to_hex(hash)
                )));
            }
        };
        let member = self.inner.members.get(&id).ok_or_else(|| {
            StoreError::Other(format!("pool location references unknown member {id}"))
        })?;
        match member.get_sync(hash)? {
            Some(data) if sha256(&data) == *hash && data.len() as u64 == location.size() => {
                Ok(Some(data))
            }
            Some(_) => Err(StoreError::Other(format!(
                "read-only pool member {id} returned corrupt or size-mismatched bytes"
            ))),
            None => Err(StoreError::Other(format!(
                "pool catalog location {} references missing bytes on member {id}",
                to_hex(hash)
            ))),
        }
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let Some(location) = self.read_location(hash)? else {
            return Ok(None);
        };
        match location {
            LocationRecord::Stored { member, size } => {
                let member_store = self.inner.members.get(&member).ok_or_else(|| {
                    StoreError::Other(format!("pool location references unknown member {member}"))
                })?;
                match member_store.blob_size_sync(hash)? {
                    Some(member_size) if member_size == size => Ok(Some(size)),
                    Some(member_size) => Err(StoreError::Other(format!(
                        "pool catalog size {size} for {} differs from member {member} size {member_size}",
                        to_hex(hash)
                    ))),
                    None => Err(StoreError::Other(format!(
                        "pool catalog location {} references missing bytes on member {member}",
                        to_hex(hash)
                    ))),
                }
            }
            LocationRecord::Pending { .. } => Err(StoreError::Other(format!(
                "read-only terminal verification rejected pending pool location {}",
                to_hex(hash)
            ))),
            LocationRecord::Moving { .. } => Err(StoreError::Other(format!(
                "read-only terminal verification rejected moving pool location {}",
                to_hex(hash)
            ))),
        }
    }

    pub fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)?
            .map(|data| slice_blob_range(&data, start, end_inclusive))
            .transpose()
    }
}

fn open_members(
    manifest: &PoolManifest,
) -> Result<HashMap<PoolMemberId, Arc<LmdbBlobReader>>, StoreError> {
    let mut stores = HashMap::with_capacity(manifest.members.len());
    for member in &manifest.members {
        let config = &member.config;
        verify_member_path(&config.path, MEMBER_MARKER_NAME, member.id)?;
        let external = external_options(member.id, config)?;
        if stores
            .insert(
                member.id,
                Arc::new(LmdbBlobReader::open(&config.path, external)?),
            )
            .is_some()
        {
            return Err(StoreError::Other(format!(
                "pool manifest repeats member {}",
                member.id
            )));
        }
    }
    Ok(stores)
}

fn external_options(
    id: PoolMemberId,
    config: &PoolMemberConfig,
) -> Result<Option<ExternalBlobOptions>, StoreError> {
    match (
        config.external_blob_dir.as_ref(),
        config.external_blob_min_bytes,
    ) {
        (Some(path), Some(min_bytes)) => {
            verify_member_path(path, EXTERNAL_MARKER_NAME, id)?;
            Ok(Some(ExternalBlobOptions {
                base_path: path.clone(),
                min_bytes: usize::try_from(min_bytes).map_err(|_| {
                    StoreError::Other("pool external blob threshold exceeds usize".into())
                })?,
                sync: config.external_blob_sync,
                pack_target_bytes: config
                    .external_pack_target_bytes
                    .map(usize::try_from)
                    .transpose()
                    .map_err(|_| {
                        StoreError::Other("pool external pack target exceeds usize".into())
                    })?,
            }))
        }
        (None, None) => Ok(None),
        _ => Err(StoreError::Other(
            "invalid pool external blob configuration".into(),
        )),
    }
}

fn read_only_error() -> StoreError {
    StoreError::Other("read-only PoolStore does not permit mutation".into())
}

#[async_trait]
impl Store for ReadOnlyPoolStore {
    async fn put(&self, _hash: Hash, _data: Vec<u8>) -> Result<bool, StoreError> {
        Err(read_only_error())
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
        self.get_sync(hash)?
            .map(|data| slice_blob_range(&data, start, end_inclusive))
            .transpose()
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.blob_size_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.get_sync(hash)?.is_some())
    }

    async fn delete(&self, _hash: &Hash) -> Result<bool, StoreError> {
        Err(read_only_error())
    }

    async fn stats(&self) -> StoreStats {
        StoreStats::default()
    }

    async fn pin(&self, _hash: &Hash) -> Result<(), StoreError> {
        Err(read_only_error())
    }

    async fn unpin(&self, _hash: &Hash) -> Result<(), StoreError> {
        Err(read_only_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PoolStore, PoolStoreConfig};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    const MUTATE_CATALOG_ENV: &str = "HASHTREE_READ_ONLY_MUTATE_CATALOG";
    const MUTATE_MEMBER_ENV: &str = "HASHTREE_READ_ONLY_MUTATE_MEMBER";

    fn file_sha256(path: &Path) -> Hash {
        sha256(&fs::read(path).expect("read LMDB data file"))
    }

    fn file_modified(path: &Path) -> std::time::SystemTime {
        fs::metadata(path)
            .expect("read LMDB data file metadata")
            .modified()
            .expect("read LMDB data file mtime")
    }

    fn pool_config() -> PoolStoreConfig {
        let mut config = PoolStoreConfig::default();
        config.temperature.enabled = false;
        config
    }

    #[tokio::test]
    async fn reads_real_pool_without_mutating_data_files() {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let member = temp.path().join("member");
        let pool = PoolStore::open(&catalog, pool_config()).expect("open pool");
        pool.add_member(PoolMemberConfig::new(member.clone(), 1024 * 1024))
            .expect("add member");
        let data = b"read-only terminal audit bytes";
        let hash = sha256(data);
        pool.put_sync(hash, data).expect("put blob");
        pool.force_sync().expect("sync pool");
        drop(pool);

        let catalog_data = catalog.join("data.mdb");
        let member_data = member.join("data.mdb");
        let catalog_before = file_sha256(&catalog_data);
        let member_before = file_sha256(&member_data);
        let catalog_mtime_before = file_modified(&catalog_data);
        let member_mtime_before = file_modified(&member_data);
        let reader = ReadOnlyPoolStore::open(&catalog).expect("open read-only pool");
        let audit = reader
            .validate_committed_catalog()
            .expect("validate catalog");
        assert_eq!(audit.stored_locations, 1);
        assert_eq!(audit.manifest_sha256.len(), 64);
        assert_eq!(
            reader.get_sync(&hash).expect("get blob"),
            Some(data.to_vec())
        );
        assert_eq!(
            reader.blob_size_sync(&hash).expect("blob size"),
            Some(data.len() as u64)
        );
        assert_eq!(
            reader.get_range(&hash, 5, 8).await.expect("blob range"),
            Some(data[5..=8].to_vec())
        );
        assert!(reader.has(&hash).await.expect("has blob"));
        assert!(reader.put(hash, data.to_vec()).await.is_err());
        assert!(reader.delete(&hash).await.is_err());
        assert!(reader.pin(&hash).await.is_err());
        assert!(reader.unpin(&hash).await.is_err());
        drop(reader);

        assert_eq!(file_sha256(&catalog_data), catalog_before);
        assert_eq!(file_sha256(&member_data), member_before);
        assert_eq!(file_modified(&catalog_data), catalog_mtime_before);
        assert_eq!(file_modified(&member_data), member_mtime_before);
    }

    #[test]
    fn rejects_pending_and_moving_catalog_records() {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let pool = PoolStore::open(&catalog, pool_config()).expect("open pool");
        let source = pool
            .add_member(PoolMemberConfig::new(
                temp.path().join("source"),
                1024 * 1024,
            ))
            .expect("add source");
        let target = pool
            .add_member(PoolMemberConfig::new(
                temp.path().join("target"),
                1024 * 1024,
            ))
            .expect("add target");
        let pending_hash = sha256(b"pending audit record");
        let moving_hash = sha256(b"moving audit record");
        let mut wtxn = pool.env.write_txn().expect("catalog write txn");
        pool.set_location_txn(
            &mut wtxn,
            pending_hash,
            Some(LocationRecord::Pending {
                member: source,
                size: 20,
            }),
        )
        .expect("write pending location");
        pool.set_location_txn(
            &mut wtxn,
            moving_hash,
            Some(LocationRecord::Moving {
                source,
                target,
                size: 19,
            }),
        )
        .expect("write moving location");
        wtxn.commit().expect("commit interrupted records");
        pool.force_sync().expect("sync catalog");
        drop(pool);

        let reader = ReadOnlyPoolStore::open(&catalog).expect("open read-only pool");
        let error = reader
            .validate_committed_catalog()
            .expect_err("catalog with interrupted records must fail");
        assert!(
            error.to_string().contains("pending") || error.to_string().contains("moving"),
            "unexpected catalog error: {error}"
        );
        assert!(reader
            .get_sync(&pending_hash)
            .expect_err("pending read must fail")
            .to_string()
            .contains("pending"));
        assert!(reader
            .get_sync(&moving_hash)
            .expect_err("moving read must fail")
            .to_string()
            .contains("moving"));
        assert!(reader
            .blob_size_sync(&pending_hash)
            .expect_err("pending size must fail")
            .to_string()
            .contains("pending"));
        assert!(reader
            .blob_size_sync(&moving_hash)
            .expect_err("moving size must fail")
            .to_string()
            .contains("moving"));
    }

    #[test]
    fn rejects_stored_location_with_missing_member_bytes() {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let member_path = temp.path().join("member");
        let pool = PoolStore::open(&catalog, pool_config()).expect("open pool");
        let member = pool
            .add_member(PoolMemberConfig::new(member_path, 1024 * 1024))
            .expect("add member");
        let data = b"catalogued but physically missing";
        let hash = sha256(data);
        pool.put_sync(hash, data).expect("put blob");
        let member_store = pool.get_member(member).expect("open member");
        assert!(member_store
            .delete_sync(&hash)
            .expect("delete only member bytes"));
        pool.force_sync().expect("sync corrupt physical state");
        drop(member_store);
        drop(pool);

        let reader = ReadOnlyPoolStore::open(&catalog).expect("open read-only pool");
        let get_error = reader
            .get_sync(&hash)
            .expect_err("stored location without bytes must fail closed");
        assert!(get_error.to_string().contains(&to_hex(&hash)));
        assert!(get_error.to_string().contains(&member.to_string()));
        assert!(get_error.to_string().contains("missing bytes"));
        let size_error = reader
            .blob_size_sync(&hash)
            .expect_err("size must verify physical bytes");
        assert!(size_error.to_string().contains("missing bytes"));
    }

    #[test]
    fn rejects_catalog_location_for_unknown_member() {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let pool = PoolStore::open(&catalog, pool_config()).expect("open pool");
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("member"),
            1024 * 1024,
        ))
        .expect("add member");
        let hash = sha256(b"unknown member audit record");
        let unknown = PoolMemberId::new();
        let mut wtxn = pool.env.write_txn().expect("catalog write txn");
        pool.set_location_txn(
            &mut wtxn,
            hash,
            Some(LocationRecord::Stored {
                member: unknown,
                size: 27,
            }),
        )
        .expect("write unknown member location");
        wtxn.commit().expect("commit unknown member record");
        pool.force_sync().expect("sync catalog");
        drop(pool);

        let reader = ReadOnlyPoolStore::open(&catalog).expect("open read-only pool");
        assert!(reader
            .validate_committed_catalog()
            .expect_err("unknown catalog member must fail")
            .to_string()
            .contains("unknown member"));
        assert!(reader
            .get_sync(&hash)
            .expect_err("unknown member read must fail")
            .to_string()
            .contains("unknown member"));
        assert!(reader
            .blob_size_sync(&hash)
            .expect_err("unknown member size must fail")
            .to_string()
            .contains("unknown member"));
    }

    #[test]
    fn rejects_manifest_snapshot_that_differs_from_opened_members() {
        let opened = br#"{"version":1,"members":[]}"#;
        let opened_sha256 = to_hex(&sha256(opened));
        require_opened_manifest(opened, opened, &opened_sha256).unwrap();

        let changed = br#"{"version":1,"members":[{"id":"changed"}]}"#;
        let error = require_opened_manifest(changed, opened, &opened_sha256)
            .expect_err("changed manifest snapshot must fail");
        assert!(error.to_string().contains(&opened_sha256));
        assert!(error.to_string().contains(&to_hex(&sha256(changed))));
    }

    #[test]
    #[ignore = "subprocess entry point for read-only PoolStore manifest mutation"]
    fn read_only_manifest_mutator_helper() {
        let Some(catalog) = std::env::var_os(MUTATE_CATALOG_ENV) else {
            return;
        };
        let member = std::env::var_os(MUTATE_MEMBER_ENV).expect("mutator member path");
        let pool = PoolStore::open(catalog, pool_config()).expect("open catalog writer");
        pool.add_member(PoolMemberConfig::new(member.into(), 1024 * 1024))
            .expect("mutate pool manifest");
        pool.force_sync().expect("sync mutated manifest");
    }

    #[test]
    fn live_catalog_audit_rejects_manifest_changed_after_reader_open() {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let initial_member = temp.path().join("initial-member");
        let added_member = temp.path().join("added-member");
        let pool = PoolStore::open(&catalog, pool_config()).expect("open pool");
        pool.add_member(PoolMemberConfig::new(initial_member, 1024 * 1024))
            .expect("add initial member");
        pool.force_sync().expect("sync initial manifest");
        drop(pool);

        let reader = ReadOnlyPoolStore::open(&catalog).expect("open pinned read-only pool");
        reader
            .validate_committed_catalog()
            .expect("initial catalog matches opened manifest");

        let output = Command::new(std::env::current_exe().expect("test binary"))
            .arg("--ignored")
            .arg("--exact")
            .arg("pool::read_only::tests::read_only_manifest_mutator_helper")
            .env(MUTATE_CATALOG_ENV, &catalog)
            .env(MUTATE_MEMBER_ENV, &added_member)
            .env("RUST_TEST_THREADS", "1")
            .output()
            .expect("run manifest mutator helper");
        assert!(
            output.status.success(),
            "manifest mutator failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let error = reader
            .validate_committed_catalog()
            .expect_err("changed live manifest must differ from opened member snapshot");
        assert!(
            error
                .to_string()
                .contains("manifest changed after read-only members were opened"),
            "unexpected live manifest mismatch: {error}"
        );
    }
}
