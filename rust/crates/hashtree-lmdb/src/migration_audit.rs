use crate::{managed_env::ManagedEnv, map_heed_error};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;
use heed::types::Bytes;
use heed::{Database, EnvFlags, EnvOpenOptions};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const AUDIT_MAP_SIZE: usize = 16 * 1024 * 1024 * 1024;
// Reserve one extra named DB so opening a pre-provenance v3 store can fail on
// its authority binding instead of failing earlier with MDB_DBS_FULL.
const AUDIT_DATABASE_COUNT: u32 = 4;
const BINDING_KEY: &[u8] = b"authority-binding-v1";
const TARGET_CURSOR_KEY: &[u8] = b"target-catalog-cursor-v1";
const TARGET_FENCE_BINDING_KEY: &[u8] = b"target-fence-binding-v1";

/// Exact summary of a durable online migration verified set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMigrationAuditSummary {
    pub entries: u64,
    pub bytes: u64,
    pub content_sha256: Hash,
}

/// Durable, provenance-separated source and target body proofs.
///
/// A source proof means the source body and exact target body were both
/// hash-verified. A target proof means only the target body was hash-verified;
/// it must never satisfy the stopped source-boundary check. The root
/// controller owns the writable instance. Workers open it read-only.
pub struct PoolMigrationAuditStore {
    path: PathBuf,
    env: ManagedEnv,
    source_verified: Database<Bytes, Bytes>,
    target_verified: Database<Bytes, Bytes>,
    metadata: Database<Bytes, Bytes>,
    binding: Hash,
    writable: bool,
}

impl PoolMigrationAuditStore {
    pub fn open(path: &Path, binding: Hash) -> Result<Self, StoreError> {
        std::fs::create_dir_all(path).map_err(StoreError::Io)?;
        let mut options = EnvOpenOptions::new();
        options
            .map_size(AUDIT_MAP_SIZE)
            .max_dbs(AUDIT_DATABASE_COUNT);
        let env = unsafe { ManagedEnv::open(&options, path) }.map_err(map_heed_error)?;
        let mut wtxn = env.write_txn().map_err(map_heed_error)?;
        let source_verified = env
            .create_database(&mut wtxn, Some("source_verified"))
            .map_err(map_heed_error)?;
        let target_verified = env
            .create_database(&mut wtxn, Some("target_verified"))
            .map_err(map_heed_error)?;
        let metadata = env
            .create_database(&mut wtxn, Some("metadata"))
            .map_err(map_heed_error)?;
        match metadata.get(&wtxn, BINDING_KEY).map_err(map_heed_error)? {
            Some(actual) if actual != binding => {
                return Err(StoreError::Other(
                    "online migration audit store belongs to a different authority binding".into(),
                ))
            }
            Some(_) => {}
            None => metadata
                .put(&mut wtxn, BINDING_KEY, binding.as_slice())
                .map_err(map_heed_error)?,
        }
        wtxn.commit().map_err(map_heed_error)?;
        env.force_sync().map_err(map_heed_error)?;
        Ok(Self {
            path: path.to_path_buf(),
            env,
            source_verified,
            target_verified,
            metadata,
            binding,
            writable: true,
        })
    }

    pub fn open_read_only(path: &Path, binding: Hash) -> Result<Self, StoreError> {
        let mut options = EnvOpenOptions::new();
        options.max_dbs(AUDIT_DATABASE_COUNT);
        unsafe {
            options.flags(EnvFlags::READ_ONLY | EnvFlags::NO_READ_AHEAD);
        }
        let env = unsafe { ManagedEnv::open(&options, path) }.map_err(map_heed_error)?;
        let rtxn = env.read_txn().map_err(map_heed_error)?;
        let source_verified = env
            .open_database(&rtxn, Some("source_verified"))
            .map_err(map_heed_error)?
            .ok_or_else(|| {
                StoreError::Other("online migration source audit database is missing".into())
            })?;
        let target_verified = env
            .open_database(&rtxn, Some("target_verified"))
            .map_err(map_heed_error)?
            .ok_or_else(|| {
                StoreError::Other("online migration target audit database is missing".into())
            })?;
        let metadata = env
            .open_database(&rtxn, Some("metadata"))
            .map_err(map_heed_error)?
            .ok_or_else(|| {
                StoreError::Other("online migration audit metadata database is missing".into())
            })?;
        let actual = metadata
            .get(&rtxn, BINDING_KEY)
            .map_err(map_heed_error)?
            .ok_or_else(|| {
                StoreError::Other("online migration audit lost its authority binding".into())
            })?;
        if actual != binding {
            return Err(StoreError::Other(
                "online migration audit store belongs to a different authority binding".into(),
            ));
        }
        rtxn.commit().map_err(map_heed_error)?;
        Ok(Self {
            path: path.to_path_buf(),
            env,
            source_verified,
            target_verified,
            metadata,
            binding,
            writable: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn binding(&self) -> Hash {
        self.binding
    }

    /// Return whether each sorted candidate already has an exact durable
    /// hash/size proof. A conflicting size is corruption, never a cache miss.
    pub fn contains_source_exact_sorted(
        &self,
        candidates: &[(Hash, u64)],
    ) -> Result<Vec<bool>, StoreError> {
        self.contains_exact_sorted_in(self.source_verified, candidates, "source")
    }

    pub fn contains_target_exact_sorted(
        &self,
        candidates: &[(Hash, u64)],
    ) -> Result<Vec<bool>, StoreError> {
        self.contains_exact_sorted_in(self.target_verified, candidates, "target")
    }

    /// Root-commit source proofs after the broker independently verifies both
    /// bodies. The same entries are also durable target proofs; ordinary
    /// Pool writers cannot replace content at an existing hash.
    pub fn record_verified_source(&self, entries: &[(Hash, u64)]) -> Result<(), StoreError> {
        self.require_writable()?;
        require_sorted_candidates(entries)?;
        if entries.is_empty() {
            return Ok(());
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed_error)?;
        self.put_verified_txn(&mut wtxn, self.source_verified, entries, "source")?;
        self.put_verified_txn(&mut wtxn, self.target_verified, entries, "target")?;
        wtxn.commit().map_err(map_heed_error)?;
        self.env.force_sync().map_err(map_heed_error)
    }

    /// Start or resume the target-fenced certification epoch. Existing
    /// hash/size body proofs remain valid under Pool's content-addressed,
    /// non-replacing write contract. The raw catalog cursor is reset on the
    /// first transition so every final catalog row is revisited under the
    /// exact held fence.
    pub fn begin_target_fenced_epoch(&self, fence_binding: Hash) -> Result<(), StoreError> {
        self.require_writable()?;
        let mut wtxn = self.env.write_txn().map_err(map_heed_error)?;
        match self
            .metadata
            .get(&wtxn, TARGET_FENCE_BINDING_KEY)
            .map_err(map_heed_error)?
        {
            Some(actual) if actual == fence_binding => {}
            Some(_) => {
                return Err(StoreError::Other(
                    "online migration target fence authority changed; use a new rollout".into(),
                ))
            }
            None => {
                self.metadata
                    .delete(&mut wtxn, TARGET_CURSOR_KEY)
                    .map_err(map_heed_error)?;
                self.metadata
                    .put(
                        &mut wtxn,
                        TARGET_FENCE_BINDING_KEY,
                        fence_binding.as_slice(),
                    )
                    .map_err(map_heed_error)?;
            }
        }
        wtxn.commit().map_err(map_heed_error)?;
        self.env.force_sync().map_err(map_heed_error)
    }

    pub fn target_fence_binding(&self) -> Result<Option<Hash>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        self.metadata
            .get(&rtxn, TARGET_FENCE_BINDING_KEY)
            .map_err(map_heed_error)?
            .map(|bytes| {
                bytes.try_into().map_err(|_| {
                    StoreError::Other(
                        "online migration audit target fence binding has an invalid length".into(),
                    )
                })
            })
            .transpose()
    }

    /// Commit exact target content proofs and their raw catalog cursor in one
    /// force-synced transaction. The cursor can therefore never become
    /// durable ahead of the proofs that allowed it to advance.
    pub fn record_verified_target_page(
        &self,
        entries: &[(Hash, u64)],
        cursor: Hash,
    ) -> Result<(), StoreError> {
        self.require_writable()?;
        require_sorted_candidates(entries)?;
        let mut wtxn = self.env.write_txn().map_err(map_heed_error)?;
        self.put_verified_txn(&mut wtxn, self.target_verified, entries, "target")?;
        self.metadata
            .put(&mut wtxn, TARGET_CURSOR_KEY, cursor.as_slice())
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        self.env.force_sync().map_err(map_heed_error)
    }

    pub fn target_cursor(&self) -> Result<Option<Hash>, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        self.metadata
            .get(&rtxn, TARGET_CURSOR_KEY)
            .map_err(map_heed_error)?
            .map(|bytes| {
                bytes.try_into().map_err(|_| {
                    StoreError::Other(
                        "online migration audit target cursor has an invalid length".into(),
                    )
                })
            })
            .transpose()
    }

    pub fn reset_target_cursor(&self) -> Result<(), StoreError> {
        self.require_writable()?;
        let mut wtxn = self.env.write_txn().map_err(map_heed_error)?;
        self.metadata
            .delete(&mut wtxn, TARGET_CURSOR_KEY)
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        self.env.force_sync().map_err(map_heed_error)
    }

    /// Stream every verified hash/size record in canonical hash order.
    pub fn for_each_source_verified_batch(
        &self,
        batch_size: usize,
        visit: impl FnMut(&[(Hash, u64)]) -> Result<(), StoreError>,
    ) -> Result<PoolMigrationAuditSummary, StoreError> {
        self.for_each_verified_batch_in(self.source_verified, batch_size, visit)
    }

    pub fn for_each_target_verified_batch(
        &self,
        batch_size: usize,
        visit: impl FnMut(&[(Hash, u64)]) -> Result<(), StoreError>,
    ) -> Result<PoolMigrationAuditSummary, StoreError> {
        self.for_each_verified_batch_in(self.target_verified, batch_size, visit)
    }

    fn for_each_verified_batch_in(
        &self,
        database: Database<Bytes, Bytes>,
        batch_size: usize,
        mut visit: impl FnMut(&[(Hash, u64)]) -> Result<(), StoreError>,
    ) -> Result<PoolMigrationAuditSummary, StoreError> {
        if batch_size == 0 {
            return Err(StoreError::Other(
                "online migration audit export batch size must be non-zero".into(),
            ));
        }
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        let mut hasher = Sha256::new();
        hasher.update(b"hashtree-pool-migration-source-content/v3\0");
        let mut entries = 0u64;
        let mut bytes = 0u64;
        let mut batch = Vec::with_capacity(batch_size);
        for item in database.iter(&rtxn).map_err(map_heed_error)? {
            let (key, encoded) = item.map_err(map_heed_error)?;
            let hash: Hash = key.try_into().map_err(|_| {
                StoreError::Other("online migration audit contains an invalid hash key".into())
            })?;
            let size = decode_size(encoded)?;
            batch.push((hash, size));
            if batch.len() == batch_size {
                visit(&batch)?;
                batch.clear();
            }
            hasher.update(hash);
            hasher.update(size.to_be_bytes());
            entries = entries.checked_add(1).ok_or_else(|| {
                StoreError::Other("online migration audit entry count overflow".into())
            })?;
            bytes = bytes.checked_add(size).ok_or_else(|| {
                StoreError::Other("online migration audit byte count overflow".into())
            })?;
        }
        if !batch.is_empty() {
            visit(&batch)?;
        }
        rtxn.commit().map_err(map_heed_error)?;
        Ok(PoolMigrationAuditSummary {
            entries,
            bytes,
            content_sha256: hasher.finalize().into(),
        })
    }

    /// Find the first source hash/size pair not covered by this durable set.
    ///
    /// The scan reads only source catalog metadata (and external file lengths
    /// for legacy rows), never payload bodies.
    pub fn first_unverified_source(
        &self,
        source: &crate::LmdbBlobReader,
        page_size: usize,
    ) -> Result<Option<(Hash, u64)>, StoreError> {
        if page_size == 0 {
            return Err(StoreError::Other(
                "online migration audit coverage page size must be non-zero".into(),
            ));
        }
        let mut cursor = None;
        loop {
            let hashes = source.scan_hashes_after(cursor, page_size)?;
            if hashes.is_empty() {
                return Ok(None);
            }
            let sizes = source.sizes_for_sorted_hashes(&hashes)?;
            let entries = hashes.iter().copied().zip(sizes).collect::<Vec<_>>();
            let covered = self.contains_source_exact_sorted(&entries)?;
            if let Some((entry, _)) = entries.iter().zip(covered).find(|(_, present)| !present) {
                return Ok(Some(*entry));
            }
            cursor = hashes.last().copied();
        }
    }

    pub fn validate_binding(&self) -> Result<(), StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        let actual = self
            .metadata
            .get(&rtxn, BINDING_KEY)
            .map_err(map_heed_error)?
            .ok_or_else(|| {
                StoreError::Other("online migration audit lost its authority binding".into())
            })?;
        if actual != self.binding {
            return Err(StoreError::Other(
                "online migration audit authority binding changed".into(),
            ));
        }
        rtxn.commit().map_err(map_heed_error)
    }

    fn contains_exact_sorted_in(
        &self,
        database: Database<Bytes, Bytes>,
        candidates: &[(Hash, u64)],
        proof_kind: &str,
    ) -> Result<Vec<bool>, StoreError> {
        require_sorted_candidates(candidates)?;
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        candidates
            .iter()
            .map(|(hash, size)| {
                let Some(encoded) = database.get(&rtxn, hash).map_err(map_heed_error)? else {
                    return Ok(false);
                };
                let actual = decode_size(encoded)?;
                if actual != *size {
                    return Err(StoreError::Other(format!(
                        "online migration {proof_kind} audit size for {} changed from {actual} to {size}",
                        hashtree_core::to_hex(hash)
                    )));
                }
                Ok(true)
            })
            .collect()
    }

    fn put_verified_txn(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        database: Database<Bytes, Bytes>,
        entries: &[(Hash, u64)],
        proof_kind: &str,
    ) -> Result<(), StoreError> {
        for (hash, size) in entries {
            if let Some(encoded) = database.get(wtxn, hash).map_err(map_heed_error)? {
                let actual = decode_size(encoded)?;
                if actual != *size {
                    return Err(StoreError::Other(format!(
                        "online migration {proof_kind} audit size for {} changed from {actual} to {size}",
                        hashtree_core::to_hex(hash)
                    )));
                }
                continue;
            }
            database
                .put(wtxn, hash, &size.to_be_bytes())
                .map_err(map_heed_error)?;
        }
        Ok(())
    }

    fn require_writable(&self) -> Result<(), StoreError> {
        if !self.writable {
            return Err(StoreError::Other(
                "online migration audit store is read-only".into(),
            ));
        }
        Ok(())
    }
}

fn require_sorted_candidates(candidates: &[(Hash, u64)]) -> Result<(), StoreError> {
    if candidates.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(StoreError::Other(
            "online migration audit candidates must be unique and strictly sorted".into(),
        ));
    }
    Ok(())
}

fn decode_size(encoded: &[u8]) -> Result<u64, StoreError> {
    let bytes: [u8; 8] = encoded.try_into().map_err(|_| {
        StoreError::Other("online migration audit contains an invalid size value".into())
    })?;
    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer, LmdbBlobReader, LmdbBlobStore,
        PoolMemberConfig, PoolStore, PoolStoreConfig,
    };
    use hashtree_core::sha256;

    #[test]
    fn verified_set_is_durable_exact_and_authority_bound() {
        let temp = tempfile::tempdir().expect("temporary audit root");
        let path = temp.path().join("online-audit");
        let binding = sha256(b"rollout/source/pool authority");
        let first = sha256(b"first verified body");
        let second = sha256(b"second verified body");
        let mut entries = vec![(first, 19), (second, 20)];
        entries.sort_unstable_by_key(|(hash, _)| *hash);
        {
            let audit = PoolMigrationAuditStore::open(&path, binding).expect("create audit");
            audit
                .record_verified_source(&entries)
                .expect("record verified entries");
        }
        let audit = PoolMigrationAuditStore::open(&path, binding).expect("reopen audit");
        assert_eq!(
            audit
                .contains_source_exact_sorted(&entries)
                .expect("query exact entries"),
            vec![true, true]
        );
        assert_eq!(
            audit
                .contains_target_exact_sorted(&entries)
                .expect("query target provenance"),
            vec![true, true]
        );
        assert!(audit
            .contains_source_exact_sorted(&[(first, 18)])
            .expect_err("conflicting size must fail")
            .to_string()
            .contains("changed"));
        drop(audit);
        let error = match PoolMigrationAuditStore::open(&path, sha256(b"different authority")) {
            Ok(_) => panic!("different authority must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("different authority"));
    }

    #[test]
    fn coverage_scan_reads_catalog_sizes_and_finds_only_unverified_source() {
        let temp = tempfile::tempdir().expect("temporary migration root");
        let source_path = temp.path().join("source");
        let source = LmdbBlobStore::new(&source_path).expect("create source");
        let first_body = b"first source body";
        let second_body = b"second source body";
        let first = sha256(first_body);
        let second = sha256(second_body);
        source
            .put_sync(first, first_body)
            .expect("put first source");
        source
            .put_sync(second, second_body)
            .expect("put second source");
        drop(source);
        let reader = LmdbBlobReader::open(&source_path, None).expect("open source reader");

        let audit = PoolMigrationAuditStore::open(
            &temp.path().join("online-audit"),
            sha256(b"coverage authority"),
        )
        .expect("create online audit");
        audit
            .record_verified_source(&[(first, first_body.len() as u64)])
            .expect("record first source");
        assert_eq!(
            audit
                .first_unverified_source(&reader, 1)
                .expect("scan coverage"),
            Some((second, second_body.len() as u64))
        );
        audit
            .record_verified_source(&[(second, second_body.len() as u64)])
            .expect("record second source");
        assert_eq!(
            audit
                .first_unverified_source(&reader, 1)
                .expect("scan complete coverage"),
            None
        );
    }

    #[test]
    fn durable_verified_set_resumes_real_explicit_pool_migration() {
        let temp = tempfile::tempdir().expect("temporary migration root");
        let source_path = temp.path().join("source");
        let source = LmdbBlobStore::new(&source_path).expect("create source");
        let bodies = [
            b"online migration body zero".as_slice(),
            b"online migration body one".as_slice(),
            b"online migration body two".as_slice(),
        ];
        for body in bodies {
            source
                .put_sync(sha256(body), body)
                .expect("write source body");
        }
        source.force_sync().expect("sync source");
        drop(source);
        let source = LmdbBlobReader::open(&source_path, None).expect("open source reader");
        let hashes = source.scan_hashes_after(None, 16).expect("scan source");
        let sizes = source
            .sizes_for_sorted_hashes(&hashes)
            .expect("read source sizes");

        let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
            .expect("open target Pool");
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("member"),
            64 * 1024 * 1024,
        ))
        .expect("add target member");
        let mut authorize = |_: Option<Hash>, _: usize| Ok(());
        let first = migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer(
            &source,
            &pool,
            &hashes[..2],
            None,
            usize::MAX,
            &mut authorize,
        )
        .expect("migrate first explicit page");
        pool.force_sync().expect("sync target");

        let audit_path = temp.path().join("online-audit");
        let binding = sha256(b"resumable real migration authority");
        let audit = PoolMigrationAuditStore::open(&audit_path, binding).expect("create audit");
        audit
            .record_verified_source(&first.verified_source_entries)
            .expect("record first verified page");
        drop(audit);

        let audit = PoolMigrationAuditStore::open(&audit_path, binding).expect("reopen audit");
        let entries = hashes
            .iter()
            .copied()
            .zip(sizes.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(
            audit
                .contains_source_exact_sorted(&entries)
                .expect("query resumed coverage"),
            vec![true, true, false]
        );
        let resumed = migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer(
            &source,
            &pool,
            &hashes[2..],
            hashes.get(1).copied(),
            usize::MAX,
            &mut authorize,
        )
        .expect("resume only missing explicit body");
        pool.force_sync().expect("sync resumed target");
        audit
            .record_verified_source(&resumed.verified_source_entries)
            .expect("record resumed page");
        assert_eq!(
            audit
                .contains_source_exact_sorted(&entries)
                .expect("query complete coverage"),
            vec![true, true, true]
        );
        assert_eq!(pool.stats().expect("target stats").count, 3);
    }

    #[test]
    fn target_fence_rescans_catalog_but_retains_root_verified_content_proofs() {
        let temp = tempfile::tempdir().expect("temporary audit root");
        let path = temp.path().join("online-audit");
        let binding = sha256(b"root ledger authority");
        let pre_fence = sha256(b"pre-fence target");
        let fenced = sha256(b"fenced target");
        let root = PoolMigrationAuditStore::open(&path, binding).expect("create root ledger");
        root.record_verified_source(&[(pre_fence, 16)])
            .expect("record source proof before target fence");
        assert_eq!(
            root.contains_target_exact_sorted(&[(pre_fence, 16)])
                .expect("query pre-fence target proof"),
            vec![true]
        );
        root.record_verified_target_page(&[(pre_fence, 16)], pre_fence)
            .expect("record pre-fence target page");
        assert_eq!(
            root.target_cursor().expect("read pre-fence cursor"),
            Some(pre_fence)
        );

        let fence = sha256(b"exact persistent mask authorities");
        root.begin_target_fenced_epoch(fence)
            .expect("begin target proof epoch");
        assert_eq!(
            root.target_cursor().expect("read reset target cursor"),
            None
        );
        root.record_verified_target_page(&[(fenced, 13)], fenced)
            .expect("record root target proof");
        drop(root);
        let worker =
            PoolMigrationAuditStore::open_read_only(&path, binding).expect("open worker ledger");
        assert_eq!(
            worker.target_fence_binding().expect("read fence"),
            Some(fence)
        );
        let mut target_entries = vec![(pre_fence, 16), (fenced, 13)];
        target_entries.sort_unstable_by_key(|(hash, _)| *hash);
        assert_eq!(
            worker
                .contains_target_exact_sorted(&target_entries)
                .expect("read exact target provenance"),
            vec![true, true]
        );
        assert!(worker
            .record_verified_source(&[(pre_fence, 16)])
            .expect_err("worker ledger writes must fail")
            .to_string()
            .contains("read-only"));
        drop(worker);
        let root = PoolMigrationAuditStore::open(&path, binding).expect("reopen root ledger");
        assert!(root
            .begin_target_fenced_epoch(sha256(b"changed masks"))
            .expect_err("fence authority change must fail closed")
            .to_string()
            .contains("changed"));
    }
}
