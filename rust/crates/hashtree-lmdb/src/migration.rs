use crate::{LmdbBlobReader, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::{sha256, types::Hash};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolMigrationBatch {
    pub verified: usize,
    pub inserted: usize,
    pub inserted_bytes: u64,
    pub last_hash: Option<Hash>,
    pub source_exhausted: bool,
}

/// Copy and verify one bounded lexicographic page from an existing LMDB store.
///
/// The caller persists `last_hash` only after this returns successfully. Replaying
/// a page after process death is safe because pool writes are hash-verified and
/// idempotent.
pub fn migrate_lmdb_batch(
    source: &LmdbBlobReader,
    target: &PoolStore,
    after: Option<Hash>,
    limit: usize,
) -> Result<PoolMigrationBatch, StoreError> {
    if limit == 0 {
        return Ok(PoolMigrationBatch::default());
    }
    let hashes = source.scan_hashes_after(after, limit)?;
    if hashes.is_empty() {
        return Ok(PoolMigrationBatch {
            source_exhausted: true,
            ..PoolMigrationBatch::default()
        });
    }
    let mut items = Vec::with_capacity(hashes.len());
    for hash in &hashes {
        let data = source
            .get_sync(hash)?
            .ok_or_else(|| StoreError::Other(format!("source lost blob {hash:?} during scan")))?;
        if sha256(&data) != *hash {
            return Err(StoreError::Other(format!(
                "source returned corrupt bytes for {hash:?}"
            )));
        }
        items.push((*hash, data));
    }
    let report = target.put_many_report_sync(&items)?;
    Ok(PoolMigrationBatch {
        verified: items.len(),
        inserted: report.inserted,
        inserted_bytes: report.inserted_bytes,
        last_hash: hashes.last().copied(),
        source_exhausted: false,
    })
}
