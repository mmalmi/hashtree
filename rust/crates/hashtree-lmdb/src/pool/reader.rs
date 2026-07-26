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

/// One result from a bounded, strictly read-only Pool batch.
pub struct PoolReadBatchItem {
    pub hash: Hash,
    pub member_candidates: Vec<PoolMemberId>,
    pub declared_size: Option<u64>,
    pub data: Option<Vec<u8>>,
    pub error: Option<String>,
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

    /// Resolve candidate member order for many hashes with one catalog
    /// transaction. Moving records prefer the target and retain the source as
    /// a read fallback; pending and stored records contain one candidate.
    pub fn blob_member_candidates(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<Vec<PoolMemberId>>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        hashes
            .iter()
            .map(|hash| {
                let location = self
                    .locations
                    .get(&rtxn, hash)
                    .map_err(map_heed)?
                    .map(LocationRecord::decode)
                    .transpose()?;
                Ok(match location {
                    None => Vec::new(),
                    Some(LocationRecord::Pending { member, .. })
                    | Some(LocationRecord::Stored { member, .. }) => vec![member],
                    Some(LocationRecord::Moving { source, target, .. }) => {
                        vec![target, source]
                    }
                })
            })
            .collect()
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

    /// Resolve a bounded prefix with one catalog transaction and coalesced
    /// member reads. The returned prefix always contains at least one item
    /// when `hashes` is non-empty and `byte_limit` is non-zero.
    pub fn read_hashes_bounded(
        &self,
        hashes: &[Hash],
        byte_limit: u64,
    ) -> Result<Vec<PoolReadBatchItem>, StoreError> {
        if hashes.is_empty() || byte_limit == 0 {
            return Ok(Vec::new());
        }

        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut items = Vec::with_capacity(hashes.len());
        let mut selected_bytes = 0u64;
        for hash in hashes {
            let location = self
                .locations
                .get(&rtxn, hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            let declared_size = location.as_ref().map(|location| (*location).size());
            let expected_size = declared_size.unwrap_or(0);
            if !items.is_empty() && selected_bytes.saturating_add(expected_size) > byte_limit {
                break;
            }
            let member_candidates = match location {
                None => Vec::new(),
                Some(LocationRecord::Pending { member, .. })
                | Some(LocationRecord::Stored { member, .. }) => vec![member],
                Some(LocationRecord::Moving { source, target, .. }) => vec![target, source],
            };
            items.push(PoolReadBatchItem {
                hash: *hash,
                member_candidates,
                declared_size,
                data: None,
                error: None,
            });
            selected_bytes = selected_bytes.saturating_add(expected_size);
        }
        drop(rtxn);

        for candidate_index in 0..2 {
            let mut reads_by_member = HashMap::<PoolMemberId, Vec<(usize, Hash)>>::new();
            for (item_index, item) in items.iter().enumerate() {
                if item.data.is_none() {
                    if let Some(member) = item.member_candidates.get(candidate_index) {
                        reads_by_member
                            .entry(*member)
                            .or_default()
                            .push((item_index, item.hash));
                    }
                }
            }
            for (member_id, requested) in reads_by_member {
                let Some(member) = self.members.get(&member_id) else {
                    for (item_index, _) in requested {
                        items[item_index].error.get_or_insert_with(|| {
                            format!("pool member {member_id} is unavailable to reader")
                        });
                    }
                    continue;
                };
                let hashes = requested.iter().map(|(_, hash)| *hash).collect::<Vec<_>>();
                let present = member.existing_hashes_in_sorted_candidates(&hashes)?;
                let present_requests = requested
                    .into_iter()
                    .zip(present)
                    .filter_map(|(request, is_present)| is_present.then_some(request))
                    .collect::<Vec<_>>();
                if present_requests.is_empty() {
                    continue;
                }
                let present_hashes = present_requests
                    .iter()
                    .map(|(_, hash)| *hash)
                    .collect::<Vec<_>>();
                let bodies = member.read_hashes_bounded(&present_hashes, u64::MAX)?;
                if bodies.len() != present_requests.len() {
                    return Err(StoreError::Other(format!(
                        "pool member {member_id} bounded reader returned {} of {} requested blobs",
                        bodies.len(),
                        present_requests.len()
                    )));
                }
                for ((item_index, expected_hash), (actual_hash, data)) in
                    present_requests.into_iter().zip(bodies)
                {
                    if actual_hash != expected_hash {
                        return Err(StoreError::Other(format!(
                            "pool member {member_id} returned hashes out of order"
                        )));
                    }
                    if sha256(&data) == expected_hash {
                        items[item_index].data = Some(data);
                    } else {
                        items[item_index].error.get_or_insert_with(|| {
                            format!("pool member {member_id} returned corrupt bytes")
                        });
                    }
                }
            }
        }
        for item in &mut items {
            if item.member_candidates.is_empty() {
                item.error
                    .get_or_insert_with(|| "Pool catalog entry is missing".into());
            } else if item.data.is_none() {
                item.error
                    .get_or_insert_with(|| "Pool payload is missing from candidate members".into());
            } else {
                item.error = None;
            }
        }
        Ok(items)
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
        let batch = reader.read_hashes_bounded(
            &[pending_hash, moving_hash],
            pending_data.len() as u64 + moving_data.len() as u64,
        )?;
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].hash, pending_hash);
        assert_eq!(batch[0].data.as_deref(), Some(pending_data.as_slice()));
        assert!(batch[0].error.is_none());
        assert_eq!(batch[1].hash, moving_hash);
        assert_eq!(batch[1].data.as_deref(), Some(moving_data.as_slice()));
        assert!(batch[1].error.is_none());
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
