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
    manifest_identity: PoolManifestIdentity,
    member_ids: Vec<PoolMemberId>,
    members: HashMap<PoolMemberId, LmdbBlobReader>,
    member_errors: HashMap<PoolMemberId, String>,
}

/// One result from a bounded, strictly read-only Pool batch.
pub struct PoolReadBatchItem {
    pub hash: Hash,
    pub member_candidates: Vec<PoolMemberId>,
    pub declared_size: Option<u64>,
    pub data: Option<Vec<u8>>,
    pub error: Option<String>,
}

/// Exact, read-only Pool catalog state for one blob.
///
/// Release auditors must distinguish a terminal `Stored` location from
/// `Pending` and `Moving`: physical bytes on a destination member are not a
/// completed residency proof until the catalog commits that destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolCatalogLocation {
    Missing,
    Pending {
        member: PoolMemberId,
        size: u64,
    },
    Stored {
        member: PoolMemberId,
        size: u64,
    },
    Moving {
        source: PoolMemberId,
        target: PoolMemberId,
        size: u64,
    },
}

/// Identity of the complete Pool manifest observed by a read-only reader.
///
/// `sha256` covers the exact stored manifest bytes, including generation,
/// member ordering, member states, and every member configuration field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolManifestIdentity {
    pub generation: u64,
    pub sha256: Hash,
    pub member_ids: Vec<PoolMemberId>,
}

impl PoolCatalogLocation {
    fn from_record(record: Option<LocationRecord>) -> Self {
        match record {
            None => Self::Missing,
            Some(LocationRecord::Pending { member, size }) => Self::Pending { member, size },
            Some(LocationRecord::Stored { member, size }) => Self::Stored { member, size },
            Some(LocationRecord::Moving {
                source,
                target,
                size,
            }) => Self::Moving {
                source,
                target,
                size,
            },
        }
    }
}

impl PoolStoreReader {
    pub fn open<P: AsRef<Path>>(path: P, config: PoolStoreConfig) -> Result<Self, StoreError> {
        Self::open_inner(path.as_ref(), config, false)
    }

    /// Open a strict read-only validator while retaining unavailable manifest
    /// members as explicit per-read errors.
    ///
    /// This is intentionally audit-only behavior. Ordinary readers must use
    /// [`Self::open`], which fails immediately when any configured member
    /// cannot be opened.
    pub fn open_with_unavailable_members_for_audit<P: AsRef<Path>>(
        path: P,
        config: PoolStoreConfig,
    ) -> Result<Self, StoreError> {
        Self::open_inner(path.as_ref(), config, true)
    }

    fn open_inner(
        path: &Path,
        config: PoolStoreConfig,
        retain_unavailable_members: bool,
    ) -> Result<Self, StoreError> {
        if config.temperature.enabled {
            return Err(StoreError::Other(
                "read-only Pool validation requires temperature tracking to be disabled".into(),
            ));
        }

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
        let (manifest, manifest_sha256) = read_manifest(&manifest_db, &rtxn)?;
        rtxn.commit().map_err(map_heed)?;

        let mut member_ids = manifest
            .members
            .iter()
            .map(|member| member.id)
            .collect::<Vec<_>>();
        member_ids.sort_unstable();
        let manifest_identity = PoolManifestIdentity {
            generation: manifest.generation,
            sha256: manifest_sha256,
            member_ids: member_ids.clone(),
        };
        let mut members = HashMap::with_capacity(manifest.members.len());
        let mut member_errors = HashMap::new();
        for member in manifest.members {
            match open_member_reader(member.id, &member.config) {
                Ok(reader) => {
                    members.insert(member.id, reader);
                }
                Err(error) => {
                    if !retain_unavailable_members {
                        return Err(error);
                    }
                    member_errors.insert(member.id, error.to_string());
                }
            }
        }
        Ok(Self {
            env,
            locations,
            manifest_identity,
            member_ids,
            members,
            member_errors,
        })
    }

    pub fn blob_location(&self, hash: &Hash) -> Result<Option<PoolMemberId>, StoreError> {
        Ok(self
            .read_location(hash)?
            .map(LocationRecord::preferred_member))
    }

    /// Return the exact member identities declared by the Pool manifest.
    ///
    /// Validators use this to pin the complete target-member set instead of
    /// accidentally accepting a newly added or removed member.
    pub fn member_ids(&self) -> Vec<PoolMemberId> {
        self.member_ids.clone()
    }

    /// Return the complete manifest identity captured by this reader.
    pub fn manifest_identity(&self) -> PoolManifestIdentity {
        self.manifest_identity.clone()
    }

    /// Mark which sorted candidate hashes physically exist on one exact Pool
    /// member. No catalog fallback or mutation is performed.
    pub fn member_existing_hashes_in_sorted_candidates(
        &self,
        member: PoolMemberId,
        hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        self.members
            .get(&member)
            .ok_or_else(|| self.member_unavailable_error(member))?
            .existing_hashes_in_sorted_candidates(hashes)
    }

    /// Read a bounded prefix of sorted, physically present hashes from one
    /// exact Pool member without checking their content hashes.
    ///
    /// Callers must pass only hashes reported present by
    /// [`Self::member_existing_hashes_in_sorted_candidates`] and must verify
    /// every returned body against its requested hash.
    pub fn read_member_hashes_bounded_unverified(
        &self,
        member: PoolMemberId,
        hashes: &[Hash],
        byte_limit: u64,
    ) -> Result<Vec<(Hash, Vec<u8>)>, StoreError> {
        self.members
            .get(&member)
            .ok_or_else(|| self.member_unavailable_error(member))?
            .read_hashes_bounded(hashes, byte_limit)
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

    /// Return exact catalog state for many hashes in one read transaction.
    ///
    /// This deliberately exposes `Pending` and `Moving` rather than reducing
    /// them to read candidates, so a release audit cannot mistake an
    /// in-progress write or relocation for terminal residency.
    pub fn blob_catalog_locations(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<PoolCatalogLocation>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        hashes
            .iter()
            .map(|hash| {
                let record = self
                    .locations
                    .get(&rtxn, hash)
                    .map_err(map_heed)?
                    .map(LocationRecord::decode)
                    .transpose()?;
                Ok(PoolCatalogLocation::from_record(record))
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
                first_error.get_or_insert_with(|| self.member_unavailable_error(id));
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
                    let error = self.member_unavailable_error(member_id).to_string();
                    for (item_index, _) in requested {
                        items[item_index].error.get_or_insert_with(|| error.clone());
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

    fn member_unavailable_error(&self, member: PoolMemberId) -> StoreError {
        match self.member_errors.get(&member) {
            Some(error) => StoreError::Other(format!(
                "pool member {member} is unavailable to reader: {error}"
            )),
            None if self.member_ids.contains(&member) => {
                StoreError::Other(format!("pool member {member} is unavailable to reader"))
            }
            None => StoreError::Other(format!("unknown pool member {member}")),
        }
    }
}

fn read_manifest(
    database: &Database<Bytes, Bytes>,
    txn: &heed::RoTxn<'_>,
) -> Result<(PoolManifest, Hash), StoreError> {
    let bytes = database
        .get(txn, MANIFEST_KEY)
        .map_err(map_heed)?
        .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
    Ok((decode_manifest(bytes)?, sha256(bytes)))
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
            reader.blob_catalog_locations(&[pending_hash, moving_hash])?,
            vec![
                PoolCatalogLocation::Pending {
                    member: first,
                    size: pending_data.len() as u64,
                },
                PoolCatalogLocation::Moving {
                    source: first,
                    target: second,
                    size: moving_data.len() as u64,
                },
            ]
        );
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

    #[test]
    fn reader_can_probe_one_exact_member_without_catalog_fallback() -> Result<(), StoreError> {
        let temp = tempfile::tempdir().map_err(StoreError::Io)?;
        let catalog = temp.path().join("catalog");
        let first_path = temp.path().join("first");
        let second_path = temp.path().join("second");
        let mut config = PoolStoreConfig::default();
        config.temperature.enabled = false;
        let pool = PoolStore::open(&catalog, config.clone())?;
        let first = pool.add_member(PoolMemberConfig::new(first_path, 1024 * 1024))?;
        let second = pool.add_member(PoolMemberConfig::new(second_path, 1024 * 1024))?;

        let data = b"exact member validation payload";
        let hash = sha256(data);
        pool.get_member(first)?.put_sync(hash, data)?;
        let mut wtxn = pool.env.write_txn().map_err(map_heed)?;
        pool.set_location_txn(
            &mut wtxn,
            hash,
            Some(LocationRecord::Stored {
                member: first,
                size: data.len() as u64,
            }),
        )?;
        wtxn.commit().map_err(map_heed)?;
        pool.force_sync()?;
        drop(pool);

        let reader = PoolStoreReader::open(&catalog, config)?;
        assert_eq!(
            reader.blob_catalog_locations(&[hash])?,
            vec![PoolCatalogLocation::Stored {
                member: first,
                size: data.len() as u64,
            }]
        );
        let mut expected_members = vec![first, second];
        expected_members.sort_unstable();
        assert_eq!(reader.member_ids(), expected_members);
        assert_eq!(
            reader.member_existing_hashes_in_sorted_candidates(first, &[hash])?,
            vec![true]
        );
        assert_eq!(
            reader.member_existing_hashes_in_sorted_candidates(second, &[hash])?,
            vec![false]
        );
        assert_eq!(
            reader.read_member_hashes_bounded_unverified(first, &[hash], data.len() as u64)?,
            vec![(hash, data.to_vec())]
        );
        Ok(())
    }

    #[test]
    fn reader_preserves_manifest_identity_when_a_member_is_unavailable() -> Result<(), StoreError> {
        let temp = tempfile::tempdir().map_err(StoreError::Io)?;
        let catalog = temp.path().join("catalog");
        let member_path = temp.path().join("member");
        let unavailable_path = temp.path().join("member-unavailable");
        let mut config = PoolStoreConfig::default();
        config.temperature.enabled = false;
        let pool = PoolStore::open(&catalog, config.clone())?;
        let member = pool.add_member(PoolMemberConfig::new(member_path.clone(), 1024 * 1024))?;
        drop(pool);
        fs::rename(&member_path, &unavailable_path).map_err(StoreError::Io)?;

        let default_error = match PoolStoreReader::open(&catalog, config.clone()) {
            Ok(_) => {
                return Err(StoreError::Other(
                    "default reader accepted unavailable member".into(),
                ))
            }
            Err(error) => error,
        };
        assert!(default_error.to_string().contains("member"));

        let reader = PoolStoreReader::open_with_unavailable_members_for_audit(&catalog, config)?;
        assert_eq!(reader.member_ids(), vec![member]);
        let error = reader
            .member_existing_hashes_in_sorted_candidates(member, &[sha256(b"missing")])
            .expect_err("unavailable member must remain an explicit read error");
        assert!(error.to_string().contains("unavailable to reader"));
        Ok(())
    }
}
