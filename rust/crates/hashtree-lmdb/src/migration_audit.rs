use crate::{managed_env::ManagedEnv, map_heed_error};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;
use heed::types::Bytes;
use heed::{Database, EnvOpenOptions};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const AUDIT_MAP_SIZE: usize = 16 * 1024 * 1024 * 1024;
const AUDIT_DATABASE_COUNT: u32 = 2;
const BINDING_KEY: &[u8] = b"authority-binding-v1";

/// Exact summary of a durable online migration verified set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMigrationAuditSummary {
    pub entries: u64,
    pub bytes: u64,
    pub content_sha256: Hash,
}

/// Durable set of source bodies already hash-verified against a target Pool.
///
/// The set is committed and force-synced before the external scan cursor may
/// advance. A crash can therefore leave the set ahead of the cursor, which is
/// safe: replay consults the set and skips only bodies whose exact hash/size
/// proof is already durable.
pub struct PoolMigrationAuditStore {
    path: PathBuf,
    env: ManagedEnv,
    verified: Database<Bytes, Bytes>,
    metadata: Database<Bytes, Bytes>,
    binding: Hash,
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
        let verified = env
            .create_database(&mut wtxn, Some("verified"))
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
            verified,
            metadata,
            binding,
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
    pub fn contains_exact_sorted(
        &self,
        candidates: &[(Hash, u64)],
    ) -> Result<Vec<bool>, StoreError> {
        require_sorted_candidates(candidates)?;
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        candidates
            .iter()
            .map(|(hash, size)| {
                let Some(encoded) = self.verified.get(&rtxn, hash).map_err(map_heed_error)? else {
                    return Ok(false);
                };
                let actual = decode_size(encoded)?;
                if actual != *size {
                    return Err(StoreError::Other(format!(
                        "online migration audit size for {} changed from {actual} to {size}",
                        hashtree_core::to_hex(hash)
                    )));
                }
                Ok(true)
            })
            .collect()
    }

    /// Commit exact verified entries and synchronously force them durable.
    pub fn record_verified(&self, entries: &[(Hash, u64)]) -> Result<(), StoreError> {
        require_sorted_candidates(entries)?;
        if entries.is_empty() {
            return Ok(());
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed_error)?;
        for (hash, size) in entries {
            if let Some(encoded) = self.verified.get(&wtxn, hash).map_err(map_heed_error)? {
                let actual = decode_size(encoded)?;
                if actual != *size {
                    return Err(StoreError::Other(format!(
                        "online migration audit size for {} changed from {actual} to {size}",
                        hashtree_core::to_hex(hash)
                    )));
                }
                continue;
            }
            self.verified
                .put(&mut wtxn, hash, &size.to_be_bytes())
                .map_err(map_heed_error)?;
        }
        wtxn.commit().map_err(map_heed_error)?;
        self.env.force_sync().map_err(map_heed_error)
    }

    /// Stream every verified hash/size record in canonical hash order.
    pub fn for_each_verified_batch(
        &self,
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
        for item in self.verified.iter(&rtxn).map_err(map_heed_error)? {
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
            let covered = self.contains_exact_sorted(&entries)?;
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
                .record_verified(&entries)
                .expect("record verified entries");
        }
        let audit = PoolMigrationAuditStore::open(&path, binding).expect("reopen audit");
        assert_eq!(
            audit
                .contains_exact_sorted(&entries)
                .expect("query exact entries"),
            vec![true, true]
        );
        assert!(audit
            .contains_exact_sorted(&[(first, 18)])
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
            .record_verified(&[(first, first_body.len() as u64)])
            .expect("record first source");
        assert_eq!(
            audit
                .first_unverified_source(&reader, 1)
                .expect("scan coverage"),
            Some((second, second_body.len() as u64))
        );
        audit
            .record_verified(&[(second, second_body.len() as u64)])
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
            .record_verified(&first.verified_source_entries)
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
                .contains_exact_sorted(&entries)
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
            .record_verified(&resumed.verified_source_entries)
            .expect("record resumed page");
        assert_eq!(
            audit
                .contains_exact_sorted(&entries)
                .expect("query complete coverage"),
            vec![true, true, true]
        );
        assert_eq!(pool.stats().expect("target stats").count, 3);
    }
}
