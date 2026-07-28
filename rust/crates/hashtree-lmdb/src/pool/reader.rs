use super::member::open_member_reader;
use super::{
    decode_manifest, map_heed, validate_controlled_manifest, LocationRecord, PoolManifest,
    PoolMemberId, PoolStoreConfig, MANIFEST_KEY,
};
use crate::{managed_env::ManagedEnv, LmdbBlobReader};
use hashtree_core::store::StoreError;
use hashtree_core::{sha256, types::Hash};
use heed::types::Bytes;
use heed::{Database, EnvFlags, EnvOpenOptions};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

const TERMINAL_AUDIT_BATCH_ITEMS: usize = 4_096;
const TERMINAL_AUDIT_BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// Strictly read-only view of a PoolStore for exhaustive validation.
///
/// Unlike [`super::PoolStore`], this type opens both the catalog and member
/// environments with `MDB_RDONLY`. It never finalizes `Pending` locations,
/// updates access temperature, repairs locations, or records adaptive state.
/// A validator can therefore inspect an online or copied Pool without changing
/// any catalog or member bytes.
pub struct PoolStoreReader {
    env: ManagedEnv,
    manifest: Database<Bytes, Bytes>,
    opened_manifest_bytes: Vec<u8>,
    locations: Database<Bytes, Bytes>,
    by_member: Database<Bytes, heed::types::Unit>,
    temperature_state: Database<Bytes, Bytes>,
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

/// Exhaustive terminal-state proof for a controlled Pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolTerminalAudit {
    pub stored_locations: u64,
    pub stored_bytes: u64,
    pub catalog_sha256: Hash,
    pub payload_sha256: Hash,
    pub manifest_sha256: Hash,
}

/// Exact catalog-to-physical-member proof that does not reread blob payloads.
///
/// It rejects non-`Stored` catalog rows, missing or extra secondary-index
/// entries, active moves, missing physical objects, declared/physical size
/// mismatches, member orphans, and stale member indexes or counters. It must
/// be bound to a separate completed content audit when used as a release gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolPhysicalAudit {
    pub stored_locations: u64,
    pub stored_bytes: u64,
    pub catalog_sha256: Hash,
    pub physical_sha256: Hash,
    pub manifest_sha256: Hash,
}

struct TerminalStateAudit {
    physical: PoolPhysicalAudit,
    payload_sha256: Option<Hash>,
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
    pub(super) fn from_record(record: Option<LocationRecord>) -> Self {
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

        let mut member_runtime_paths = config
            .member_runtime_paths
            .iter()
            .map(|binding| (binding.id, binding.clone()))
            .collect::<HashMap<_, _>>();
        if member_runtime_paths.len() != config.member_runtime_paths.len() {
            return Err(StoreError::Other(
                "duplicate runtime path binding for controlled Pool reader".into(),
            ));
        }
        let controlled = !member_runtime_paths.is_empty()
            || config.catalog_lmdb_identity.is_some()
            || config.expected_manifest_sha256.is_some();
        if controlled
            && (member_runtime_paths.is_empty()
                || config.catalog_lmdb_identity.is_none()
                || config.expected_manifest_sha256.is_none())
        {
            return Err(StoreError::Other(
                "controlled Pool reader requires catalog identity, exact manifest SHA-256, and every member runtime binding".into(),
            ));
        }

        let mut options = EnvOpenOptions::new();
        options.max_dbs(super::CATALOG_DATABASES);
        unsafe {
            options.flags(
                super::super::env_flags_from_env() | EnvFlags::READ_ONLY | EnvFlags::NO_READ_AHEAD,
            );
        }
        let env = unsafe {
            match config.catalog_lmdb_identity {
                Some(identity) => ManagedEnv::open_pinned(&options, path, identity),
                None => ManagedEnv::open(&options, path),
            }
        }
        .map_err(map_heed)?;
        let rtxn = env.read_txn().map_err(map_heed)?;
        let manifest_db: Database<Bytes, Bytes> = env
            .open_database(&rtxn, Some("manifest"))
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest database is missing".into()))?;
        let locations = env
            .open_database(&rtxn, Some("locations"))
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool locations database is missing".into()))?;
        let by_member = env
            .open_database(&rtxn, Some("by_member"))
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool member index database is missing".into()))?;
        let temperature_state = env
            .open_database(&rtxn, Some("temperature_state"))
            .map_err(map_heed)?
            .ok_or_else(|| {
                StoreError::Other("pool temperature state database is missing".into())
            })?;
        let (manifest, manifest_sha256) = read_manifest(&manifest_db, &rtxn)?;
        let opened_manifest_bytes = manifest_db
            .get(&rtxn, MANIFEST_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?
            .to_vec();
        if let Some(expected) = config.expected_manifest_sha256 {
            validate_controlled_manifest(&opened_manifest_bytes, expected, &member_runtime_paths)?;
        }
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
            let mut runtime_config = member.config.clone();
            let pinned_identity = match member_runtime_paths.remove(&member.id) {
                Some(binding) => {
                    runtime_config.path = binding.runtime_path;
                    runtime_config.external_blob_dir = binding.runtime_external_path;
                    Some(binding.lmdb_identity)
                }
                None if controlled => {
                    return Err(StoreError::Other(format!(
                        "controlled Pool reader has no runtime binding for member {}",
                        member.id
                    )))
                }
                None => None,
            };
            match open_member_reader(member.id, &runtime_config, pinned_identity) {
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
            manifest: manifest_db,
            opened_manifest_bytes,
            locations,
            by_member,
            temperature_state,
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

    /// Exhaustively prove terminal catalog and physical member state.
    ///
    /// Every location must decode as `Stored`, reference a manifest member,
    /// have its exact secondary index entry, and resolve to bytes of the
    /// declared size and SHA-256. Each member's blob and metadata key sets must
    /// be identical and exactly match catalog ownership, so missing metadata
    /// and addressable member orphans are rejected even when counts coincide.
    /// Callers must keep all target Pool writers fenced for the full scan.
    pub fn validate_terminal_catalog_and_payloads(&self) -> Result<PoolTerminalAudit, StoreError> {
        let audit = self.validate_terminal_state(true)?;
        Ok(PoolTerminalAudit {
            stored_locations: audit.physical.stored_locations,
            stored_bytes: audit.physical.stored_bytes,
            catalog_sha256: audit.physical.catalog_sha256,
            payload_sha256: audit.payload_sha256.ok_or_else(|| {
                StoreError::Other("terminal payload audit produced no payload digest".into())
            })?,
            manifest_sha256: audit.physical.manifest_sha256,
        })
    }

    /// Exhaustively prove exact catalog/physical-member parity without
    /// rereading multi-terabyte payload bodies.
    pub fn validate_terminal_catalog_and_physical_state(
        &self,
    ) -> Result<PoolPhysicalAudit, StoreError> {
        Ok(self.validate_terminal_state(false)?.physical)
    }

    /// Scan one bounded terminal catalog page without reading payload bytes.
    ///
    /// Every row must already be `Stored`; callers can merge the returned
    /// hash/size stream with an independently certified content-evidence
    /// stream while target writers remain fenced.
    pub fn scan_terminal_catalog_entries_after(
        &self,
        after: Option<Hash>,
        limit: usize,
    ) -> Result<Vec<(Hash, u64)>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut entries = Vec::with_capacity(limit);
        let decode = |key: &[u8], encoded: &[u8]| -> Result<(Hash, u64), StoreError> {
            let hash: Hash = key
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool catalog hash key".into()))?;
            let size = match LocationRecord::decode(encoded)? {
                LocationRecord::Stored { size, .. } => size,
                LocationRecord::Pending { .. } => {
                    return Err(StoreError::Other(format!(
                        "terminal Pool content proof rejected pending location {}",
                        hashtree_core::to_hex(&hash)
                    )))
                }
                LocationRecord::Moving { .. } => {
                    return Err(StoreError::Other(format!(
                        "terminal Pool content proof rejected moving location {}",
                        hashtree_core::to_hex(&hash)
                    )))
                }
            };
            Ok((hash, size))
        };
        match after {
            Some(after) => {
                use std::ops::Bound;
                let range = (Bound::Excluded(after.as_slice()), Bound::<&[u8]>::Unbounded);
                for item in self.locations.range(&rtxn, &range).map_err(map_heed)? {
                    let (key, encoded) = item.map_err(map_heed)?;
                    entries.push(decode(key, encoded)?);
                    if entries.len() >= limit {
                        break;
                    }
                }
            }
            None => {
                for item in self.locations.iter(&rtxn).map_err(map_heed)? {
                    let (key, encoded) = item.map_err(map_heed)?;
                    entries.push(decode(key, encoded)?);
                    if entries.len() >= limit {
                        break;
                    }
                }
            }
        }
        Ok(entries)
    }

    fn validate_terminal_state(
        &self,
        verify_payloads: bool,
    ) -> Result<TerminalStateAudit, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let manifest = self
            .manifest
            .get(&rtxn, MANIFEST_KEY)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("pool manifest is missing".into()))?;
        if manifest != self.opened_manifest_bytes {
            return Err(StoreError::Other(format!(
                "pool manifest changed after terminal reader open: expected {}, found {}",
                hashtree_core::to_hex(&self.manifest_identity.sha256),
                hashtree_core::to_hex(&sha256(manifest))
            )));
        }
        decode_manifest(manifest)?;

        let mut catalog_digest = Sha256::new();
        catalog_digest.update(b"hashtree-pool-terminal-catalog/v2\0");
        catalog_digest.update((manifest.len() as u64).to_be_bytes());
        catalog_digest.update(manifest);
        let mut payload_digest = Sha256::new();
        payload_digest.update(b"hashtree-pool-terminal-payloads/v1\0");
        let mut expected_member_ownership = self
            .member_ids
            .iter()
            .map(|member| {
                let mut digest = Sha256::new();
                digest.update(b"hashtree-pool-member-ownership/v1\0");
                (*member, digest)
            })
            .collect::<HashMap<_, _>>();
        let mut stored_locations = 0u64;
        let mut stored_bytes = 0u64;
        let mut member_counts = HashMap::<PoolMemberId, u64>::new();
        let mut member_bytes = HashMap::<PoolMemberId, u64>::new();
        let mut batch = Vec::<(Hash, PoolMemberId, u64)>::with_capacity(TERMINAL_AUDIT_BATCH_ITEMS);
        let started = Instant::now();
        let mut last_progress = started;

        for item in self.locations.iter(&rtxn).map_err(map_heed)? {
            let (hash, encoded) = item.map_err(map_heed)?;
            let hash: Hash = hash
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool catalog hash key".into()))?;
            let location = LocationRecord::decode(encoded)?;
            let (member, size) = match location {
                LocationRecord::Stored { member, size } => (member, size),
                LocationRecord::Pending { .. } => {
                    return Err(StoreError::Other(format!(
                        "terminal Pool audit rejected pending location {}",
                        hashtree_core::to_hex(&hash)
                    )))
                }
                LocationRecord::Moving { .. } => {
                    return Err(StoreError::Other(format!(
                        "terminal Pool audit rejected moving location {}",
                        hashtree_core::to_hex(&hash)
                    )))
                }
            };
            if !self.members.contains_key(&member) {
                return Err(self.member_unavailable_error(member));
            }
            if self
                .by_member
                .get(&rtxn, &super::member_hash_key(member, hash))
                .map_err(map_heed)?
                .is_none()
            {
                return Err(StoreError::Other(format!(
                    "terminal Pool audit found no member index for {} on {member}",
                    hashtree_core::to_hex(&hash)
                )));
            }
            stored_locations = stored_locations
                .checked_add(1)
                .ok_or_else(|| StoreError::Other("terminal Pool location count overflow".into()))?;
            stored_bytes = stored_bytes.checked_add(size).ok_or_else(|| {
                StoreError::Other("terminal Pool declared byte total overflow".into())
            })?;
            let member_count = member_counts.entry(member).or_default();
            *member_count = member_count
                .checked_add(1)
                .ok_or_else(|| StoreError::Other("terminal Pool member count overflow".into()))?;
            let member_byte_total = member_bytes.entry(member).or_default();
            *member_byte_total = member_byte_total.checked_add(size).ok_or_else(|| {
                StoreError::Other("terminal Pool member byte total overflow".into())
            })?;
            let ownership = expected_member_ownership.get_mut(&member).ok_or_else(|| {
                StoreError::Other(format!(
                    "terminal Pool catalog references unavailable member {member}"
                ))
            })?;
            ownership.update(hash);
            ownership.update(size.to_be_bytes());
            catalog_digest.update(hash);
            catalog_digest.update((encoded.len() as u64).to_be_bytes());
            catalog_digest.update(encoded);
            if verify_payloads {
                batch.push((hash, member, size));
                if batch.len() >= TERMINAL_AUDIT_BATCH_ITEMS {
                    self.validate_terminal_payload_batch(&batch, &mut payload_digest)?;
                    batch.clear();
                }
            }
            if last_progress.elapsed() >= Duration::from_secs(10) {
                eprintln!(
                    "Pool terminal audit: verified {stored_locations} catalogued blobs / {stored_bytes} declared bytes in {} s",
                    started.elapsed().as_secs()
                );
                last_progress = Instant::now();
            }
        }
        if verify_payloads && !batch.is_empty() {
            self.validate_terminal_payload_batch(&batch, &mut payload_digest)?;
        }

        let indexed = self.by_member.len(&rtxn).map_err(map_heed)?;
        if indexed != stored_locations {
            return Err(StoreError::Other(format!(
                "terminal Pool member index count {indexed} differs from stored location count {stored_locations}"
            )));
        }
        catalog_digest.update(b"\0exact-by-member-index\0");
        catalog_digest.update(indexed.to_be_bytes());
        for (prefix, label) in [(b"m".as_slice(), "move"), (b"c".as_slice(), "move-cleanup")] {
            if self
                .temperature_state
                .prefix_iter(&rtxn, prefix)
                .map_err(map_heed)?
                .next()
                .transpose()
                .map_err(map_heed)?
                .is_some()
            {
                return Err(StoreError::Other(format!(
                    "terminal Pool audit found active {label} ownership"
                )));
            }
        }
        catalog_digest.update(b"\0no-active-move-ownership\0");
        rtxn.commit().map_err(map_heed)?;

        let mut physical_digest = Sha256::new();
        physical_digest.update(b"hashtree-pool-terminal-physical/v1\0");
        physical_digest.update(self.manifest_identity.sha256);
        for member in &self.member_ids {
            let reader = self
                .members
                .get(member)
                .ok_or_else(|| self.member_unavailable_error(*member))?;
            let expected = member_counts.get(member).copied().unwrap_or(0);
            let expected_bytes = member_bytes.get(member).copied().unwrap_or(0);
            let keyset = reader.validate_terminal_member_keyset()?;
            if keyset.blob_entries != expected
                || keyset.metadata_entries != expected
                || keyset.total_bytes != expected_bytes
            {
                return Err(StoreError::Other(format!(
                    "terminal Pool member {member} has {} blob and {} metadata records / {} exact metadata bytes, expected exactly {expected} catalog locations / {expected_bytes} catalog bytes",
                    keyset.blob_entries, keyset.metadata_entries, keyset.total_bytes
                )));
            }
            let expected_ownership = expected_member_ownership
                .remove(member)
                .expect("every manifest member has an ownership digest")
                .finalize();
            if keyset.ownership_sha256.as_slice() != expected_ownership.as_slice() {
                return Err(StoreError::Other(format!(
                    "terminal Pool member {member} physical hash/size ownership differs from its exact catalog ownership"
                )));
            }
            catalog_digest.update(member.as_bytes());
            catalog_digest.update(expected.to_be_bytes());
            catalog_digest.update(expected_bytes.to_be_bytes());
            catalog_digest.update(keyset.blob_entries.to_be_bytes());
            catalog_digest.update(keyset.metadata_entries.to_be_bytes());
            catalog_digest.update(keyset.total_bytes.to_be_bytes());
            catalog_digest.update(keyset.pinned_count.to_be_bytes());
            catalog_digest.update(keyset.pinned_bytes.to_be_bytes());
            catalog_digest.update(keyset.sha256);
            physical_digest.update(member.as_bytes());
            physical_digest.update(keyset.ownership_sha256);
            physical_digest.update(keyset.sha256);
        }

        let catalog_sha256 = catalog_digest.finalize().into();
        physical_digest.update(catalog_sha256);
        physical_digest.update(stored_locations.to_be_bytes());
        physical_digest.update(stored_bytes.to_be_bytes());
        Ok(TerminalStateAudit {
            physical: PoolPhysicalAudit {
                stored_locations,
                stored_bytes,
                catalog_sha256,
                physical_sha256: physical_digest.finalize().into(),
                manifest_sha256: self.manifest_identity.sha256,
            },
            payload_sha256: verify_payloads.then(|| payload_digest.finalize().into()),
        })
    }

    fn validate_terminal_payload_batch(
        &self,
        batch: &[(Hash, PoolMemberId, u64)],
        payload_digest: &mut Sha256,
    ) -> Result<(), StoreError> {
        let mut by_member = HashMap::<PoolMemberId, Vec<(Hash, u64)>>::new();
        for (hash, member, size) in batch {
            by_member.entry(*member).or_default().push((*hash, *size));
        }
        for (member, expected) in by_member {
            let reader = self
                .members
                .get(&member)
                .ok_or_else(|| self.member_unavailable_error(member))?;
            let mut offset = 0usize;
            while offset < expected.len() {
                let hashes = expected[offset..]
                    .iter()
                    .map(|(hash, _)| *hash)
                    .collect::<Vec<_>>();
                let found = reader.read_hashes_bounded(&hashes, TERMINAL_AUDIT_BATCH_BYTES)?;
                if found.is_empty() {
                    return Err(StoreError::Other(format!(
                        "terminal Pool member {member} omitted a requested payload"
                    )));
                }
                for ((found_hash, data), (expected_hash, expected_size)) in
                    found.iter().zip(&expected[offset..])
                {
                    if found_hash != expected_hash {
                        return Err(StoreError::Other(format!(
                            "terminal Pool member {member} returned payloads out of order"
                        )));
                    }
                    if data.len() as u64 != *expected_size || sha256(data) != *expected_hash {
                        return Err(StoreError::Other(format!(
                            "terminal Pool member {member} returned corrupt or size-mismatched bytes for {}",
                            hashtree_core::to_hex(expected_hash)
                        )));
                    }
                }
                offset = offset.checked_add(found.len()).ok_or_else(|| {
                    StoreError::Other("terminal Pool batch offset overflow".into())
                })?;
            }
        }
        for (hash, member, size) in batch {
            payload_digest.update(hash);
            payload_digest.update(member.as_bytes());
            payload_digest.update(size.to_be_bytes());
        }
        Ok(())
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
        let terminal_error = reader
            .validate_terminal_catalog_and_payloads()
            .expect_err("terminal audit must reject Pending and Moving");
        assert!(terminal_error.to_string().contains("pending location"));
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
        let audit = reader.validate_terminal_catalog_and_payloads()?;
        assert_eq!(audit.stored_locations, 1);
        assert_eq!(audit.stored_bytes, data.len() as u64);
        assert_eq!(audit.manifest_sha256, reader.manifest_identity().sha256);
        let physical = reader.validate_terminal_catalog_and_physical_state()?;
        assert_eq!(physical.stored_locations, 1);
        assert_eq!(physical.stored_bytes, data.len() as u64);
        assert_eq!(physical.catalog_sha256, audit.catalog_sha256);
        assert_eq!(physical.manifest_sha256, audit.manifest_sha256);
        Ok(())
    }

    #[test]
    fn physical_audit_rejects_equal_count_catalog_member_ownership_mismatch(
    ) -> Result<(), StoreError> {
        let temp = tempfile::tempdir().map_err(StoreError::Io)?;
        let catalog = temp.path().join("catalog");
        let member_path = temp.path().join("member");
        let mut config = PoolStoreConfig::default();
        config.temperature.enabled = false;
        let pool = PoolStore::open(&catalog, config.clone())?;
        let member = pool.add_member(PoolMemberConfig::new(member_path, 16 * 1024 * 1024))?;
        let physical_body = b"physical ownership A";
        let catalog_body = b"physical ownership B";
        assert_eq!(physical_body.len(), catalog_body.len());
        let physical_hash = sha256(physical_body);
        let catalog_hash = sha256(catalog_body);
        pool.put_sync(physical_hash, physical_body)?;
        let mut wtxn = pool.env.write_txn().map_err(map_heed)?;
        pool.set_location_txn(&mut wtxn, physical_hash, None)?;
        pool.set_location_txn(
            &mut wtxn,
            catalog_hash,
            Some(LocationRecord::Stored {
                member,
                size: catalog_body.len() as u64,
            }),
        )?;
        wtxn.commit().map_err(map_heed)?;
        pool.force_sync()?;
        drop(pool);

        let reader = PoolStoreReader::open(&catalog, config)?;
        let error = reader
            .validate_terminal_catalog_and_physical_state()
            .expect_err("equal-count ownership mismatch must fail physical audit");
        assert!(error.to_string().contains("physical hash/size ownership"));
        Ok(())
    }

    #[test]
    fn terminal_audit_rejects_missing_bodies_and_addressable_member_orphans(
    ) -> Result<(), StoreError> {
        let temp = tempfile::tempdir().map_err(StoreError::Io)?;
        let catalog = temp.path().join("catalog");
        let member_path = temp.path().join("member");
        let mut config = PoolStoreConfig::default();
        config.temperature.enabled = false;
        let pool = PoolStore::open(&catalog, config.clone())?;
        let member = pool.add_member(PoolMemberConfig::new(member_path, 16 * 1024 * 1024))?;
        let stored_data = b"terminal audit catalogued body";
        let stored_hash = sha256(stored_data);
        pool.put_sync(stored_hash, stored_data)?;
        pool.get_member(member)?.delete_sync(&stored_hash)?;
        pool.force_sync()?;
        drop(pool);

        let reader = PoolStoreReader::open(&catalog, config.clone())?;
        let missing = reader
            .validate_terminal_catalog_and_payloads()
            .expect_err("missing catalogued body must fail terminal audit");
        assert!(
            missing.to_string().contains("lost blob")
                || missing.to_string().contains("omitted")
                || missing.to_string().contains("missing")
        );
        drop(reader);

        let pool = PoolStore::open(&catalog, config.clone())?;
        pool.put_sync(stored_hash, stored_data)?;
        let orphan_data = b"terminal audit addressable member orphan";
        pool.get_member(member)?
            .put_sync(sha256(orphan_data), orphan_data)?;
        pool.force_sync()?;
        drop(pool);

        let reader = PoolStoreReader::open(&catalog, config)?;
        let orphan = reader
            .validate_terminal_catalog_and_payloads()
            .expect_err("addressable member orphan must fail terminal audit");
        assert!(orphan.to_string().contains("expected exactly"));
        Ok(())
    }

    #[test]
    fn terminal_audit_rejects_equal_count_member_keyset_mismatch() -> Result<(), StoreError> {
        let temp = tempfile::tempdir().map_err(StoreError::Io)?;
        let catalog = temp.path().join("catalog");
        let member_path = temp.path().join("member");
        let mut config = PoolStoreConfig::default();
        config.temperature.enabled = false;
        let pool = PoolStore::open(&catalog, config.clone())?;
        let member = pool.add_member(PoolMemberConfig::new(member_path, 16 * 1024 * 1024))?;
        let first_data = b"terminal member keyset first body";
        let second_data = b"terminal member keyset second body";
        let first_hash = sha256(first_data);
        let second_hash = sha256(second_data);
        let metadata_orphan_hash = sha256(b"metadata-only replacement key");
        pool.put_sync(first_hash, first_data)?;
        pool.put_sync(second_hash, second_data)?;

        let member_store = pool.get_member(member)?;
        let mut wtxn = member_store.env.write_txn().map_err(map_heed)?;
        let second_metadata = member_store
            .metadata
            .get(&wtxn, &second_hash)
            .map_err(map_heed)?
            .ok_or_else(|| StoreError::Other("test metadata row is missing".into()))?
            .to_vec();
        member_store
            .metadata
            .delete(&mut wtxn, &second_hash)
            .map_err(map_heed)?;
        member_store
            .metadata
            .put(&mut wtxn, &metadata_orphan_hash, &second_metadata)
            .map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)?;
        pool.force_sync()?;
        drop(member_store);
        drop(pool);

        let reader = PoolStoreReader::open(&catalog, config)?;
        let error = reader
            .validate_terminal_catalog_and_payloads()
            .expect_err("equal blob/metadata counts with different keys must fail terminal audit");
        assert!(error.to_string().contains("blobs/metadata key sets differ"));
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
