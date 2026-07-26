use super::member::open_member_reader;
use super::{
    decode_manifest, map_heed, LocationRecord, PoolManifest, PoolMemberId, PoolStoreConfig,
    MANIFEST_KEY,
};
use crate::{managed_env::ManagedEnv, LmdbBlobReader};
use hashtree_core::store::StoreError;
use hashtree_core::{sha256, types::Hash};
use heed::types::Bytes;
use heed::{Database, EnvFlags, EnvOpenOptions};
use std::collections::HashMap;
use std::path::Path;

/// Strictly read-only view of a PoolStore for exhaustive validation.
///
/// Unlike [`super::PoolStore`], this type opens both the catalog and member
/// environments with `MDB_RDONLY`. It never finalizes `Pending` locations,
/// updates access temperature, repairs locations, or records adaptive state.
/// A validator can therefore inspect an online or copied Pool without changing
/// any catalog or member bytes.
pub struct PoolStoreReader {
    env: ManagedEnv,
    locations: Database<Bytes, Bytes>,
    members: HashMap<PoolMemberId, LmdbBlobReader>,
}

impl PoolStoreReader {
    pub fn open<P: AsRef<Path>>(path: P, config: PoolStoreConfig) -> Result<Self, StoreError> {
        if config.temperature.enabled {
            return Err(StoreError::Other(
                "read-only Pool validation requires temperature tracking to be disabled".into(),
            ));
        }

        let path = path.as_ref();
        let mut options = EnvOpenOptions::new();
        options.max_dbs(super::CATALOG_DATABASES);
        unsafe {
            options.flags(
                super::super::env_flags_from_env() | EnvFlags::READ_ONLY | EnvFlags::NO_READ_AHEAD,
            );
        }
        let env = unsafe { ManagedEnv::open(&options, path) }.map_err(map_heed)?;
        let rtxn = env.read_txn().map_err(map_heed)?;
        let manifest_db: Database<Bytes, Bytes> = env
            .open_database(&rtxn, Some("manifest"))
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest database is missing".into()))?;
        let locations = env
            .open_database(&rtxn, Some("locations"))
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool locations database is missing".into()))?;
        let manifest = read_manifest(&manifest_db, &rtxn)?;
        rtxn.commit().map_err(map_heed)?;

        let mut members = HashMap::with_capacity(manifest.members.len());
        for member in manifest.members {
            members.insert(member.id, open_member_reader(member.id, &member.config)?);
        }
        Ok(Self {
            env,
            locations,
            members,
        })
    }

    pub fn blob_location(&self, hash: &Hash) -> Result<Option<PoolMemberId>, StoreError> {
        Ok(self
            .read_location(hash)?
            .map(LocationRecord::preferred_member))
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        Ok(self.read_location(hash)?.map(LocationRecord::size))
    }

    /// Read and hash-check bytes without finalizing or otherwise touching the
    /// catalog. Moving records try the target first and then the source.
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(location) = self.read_location(hash)? else {
            return Ok(None);
        };
        let ids = match location {
            LocationRecord::Pending { member, .. } | LocationRecord::Stored { member, .. } => {
                ([member, member], 1)
            }
            LocationRecord::Moving { source, target, .. } => ([target, source], 2),
        };

        let mut first_error = None;
        for id in ids.0.into_iter().take(ids.1) {
            let Some(member) = self.members.get(&id) else {
                first_error.get_or_insert_with(|| {
                    StoreError::Other(format!("pool member {id} is unavailable to reader"))
                });
                continue;
            };
            match member.get_sync(hash) {
                Ok(Some(data)) if sha256(&data) == *hash => return Ok(Some(data)),
                Ok(Some(_)) => {
                    first_error.get_or_insert_with(|| {
                        StoreError::Other(format!("pool member {id} returned corrupt bytes"))
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }

    fn read_location(&self, hash: &Hash) -> Result<Option<LocationRecord>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        self.locations
            .get(&rtxn, hash)
            .map_err(map_heed)?
            .map(LocationRecord::decode)
            .transpose()
    }
}

fn read_manifest(
    database: &Database<Bytes, Bytes>,
    txn: &heed::RoTxn<'_>,
) -> Result<PoolManifest, StoreError> {
    let bytes = database
        .get(txn, MANIFEST_KEY)
        .map_err(map_heed)?
        .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
    decode_manifest(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PoolMemberConfig, PoolStore};
    use std::fs;

    #[test]
    fn reader_preserves_pending_and_moving_catalog_bytes() -> Result<(), StoreError> {
        let temp = tempfile::tempdir().map_err(StoreError::Io)?;
        let catalog = temp.path().join("catalog");
        let first_path = temp.path().join("first");
        let second_path = temp.path().join("second");
        let mut writable_config = PoolStoreConfig::default();
        writable_config.temperature.enabled = false;
        let pool = PoolStore::open(&catalog, writable_config)?;
        let first = pool.add_member(PoolMemberConfig::new(first_path, 1024 * 1024))?;
        let second = pool.add_member(PoolMemberConfig::new(second_path, 1024 * 1024))?;

        let pending_data = b"pending validation payload";
        let pending_hash = sha256(pending_data);
        pool.get_member(first)?
            .put_sync(pending_hash, pending_data)?;
        let moving_data = b"moving validation payload";
        let moving_hash = sha256(moving_data);
        pool.get_member(first)?.put_sync(moving_hash, moving_data)?;
        pool.get_member(second)?
            .put_sync(moving_hash, moving_data)?;
        let mut wtxn = pool.env.write_txn().map_err(map_heed)?;
        pool.set_location_txn(
            &mut wtxn,
            pending_hash,
            Some(LocationRecord::Pending {
                member: first,
                size: pending_data.len() as u64,
            }),
        )?;
        pool.set_location_txn(
            &mut wtxn,
            moving_hash,
            Some(LocationRecord::Moving {
                source: first,
                target: second,
                size: moving_data.len() as u64,
            }),
        )?;
        wtxn.commit().map_err(map_heed)?;
        pool.force_sync()?;
        drop(pool);

        let catalog_path = catalog.join("data.mdb");
        let before = fs::read(&catalog_path).map_err(StoreError::Io)?;
        let mut reader_config = PoolStoreConfig::default();
        reader_config.temperature.enabled = false;
        let reader = PoolStoreReader::open(&catalog, reader_config)?;
        assert_eq!(
            reader.get_sync(&pending_hash)?.as_deref(),
            Some(pending_data.as_slice())
        );
        assert_eq!(
            reader.get_sync(&moving_hash)?.as_deref(),
            Some(moving_data.as_slice())
        );
        drop(reader);
        let after = fs::read(&catalog_path).map_err(StoreError::Io)?;
        assert_eq!(after, before);

        let reopened = PoolStore::open(&catalog, {
            let mut config = PoolStoreConfig::default();
            config.temperature.enabled = false;
            config
        })?;
        assert!(matches!(
            reopened.read_location(&pending_hash)?,
            Some(LocationRecord::Pending { .. })
        ));
        assert!(matches!(
            reopened.read_location(&moving_hash)?,
            Some(LocationRecord::Moving { .. })
        ));
        Ok(())
    }
}
