use crate::{LmdbBlobReader, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::{sha256, types::Hash};

/// Default upper bound for complete blob payloads retained by a migration.
///
/// A blob larger than this limit is migrated alone, so the actual peak is
/// bounded by the larger of this value and the largest individual blob.
pub const DEFAULT_POOL_MIGRATION_MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolMigrationBatch {
    /// Source hashes scanned while advancing the durable cursor.
    pub scanned: usize,
    /// Scanned hashes already committed in the target catalog.
    pub already_present: usize,
    /// Source payloads loaded and hash-verified in this pass.
    pub verified: usize,
    pub inserted: usize,
    pub inserted_bytes: u64,
    /// Number of byte-bounded writes used to commit this cursor page.
    pub write_batches: usize,
    /// Maximum bytes of complete blob payloads owned at once.
    pub peak_buffered_bytes: u64,
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
    migrate_lmdb_batch_with_max_buffer_bytes(
        source,
        target,
        after,
        limit,
        DEFAULT_POOL_MIGRATION_MAX_BUFFER_BYTES,
    )
}

/// Copy and verify one lexicographic cursor page using byte-bounded writes.
///
/// The item limit controls durable cursor frequency. `max_buffer_bytes`
/// independently limits how many complete blob payloads are retained for one
/// target write, preventing a page of large blobs from consuming memory in
/// proportion to `limit`. A single oversized blob is still migrated atomically
/// and therefore defines the unavoidable upper bound.
pub fn migrate_lmdb_batch_with_max_buffer_bytes(
    source: &LmdbBlobReader,
    target: &PoolStore,
    after: Option<Hash>,
    limit: usize,
    max_buffer_bytes: usize,
) -> Result<PoolMigrationBatch, StoreError> {
    if limit == 0 {
        return Ok(PoolMigrationBatch::default());
    }
    if max_buffer_bytes == 0 {
        return Err(StoreError::Other(
            "migration max buffer bytes must be non-zero".into(),
        ));
    }
    let hashes = source.scan_hashes_after(after, limit)?;
    if hashes.is_empty() {
        return Ok(PoolMigrationBatch {
            source_exhausted: true,
            ..PoolMigrationBatch::default()
        });
    }

    let mut batch = PoolMigrationBatch {
        scanned: hashes.len(),
        last_hash: hashes.last().copied(),
        ..PoolMigrationBatch::default()
    };
    let committed = target.committed_hashes_in_sorted_candidates(&hashes)?;
    let missing = hashes
        .iter()
        .zip(committed)
        .filter_map(|(hash, committed)| {
            if committed {
                batch.already_present = batch.already_present.saturating_add(1);
                None
            } else {
                Some(*hash)
            }
        })
        .collect::<Vec<_>>();

    let max_buffer_bytes = max_buffer_bytes as u64;
    let mut next = 0usize;
    while next < missing.len() {
        let items = source.read_hashes_bounded(&missing[next..], max_buffer_bytes)?;
        if items.is_empty() {
            return Err(StoreError::Other(
                "bounded source read made no migration progress".into(),
            ));
        }
        let mut buffered_bytes = 0u64;
        for (hash, data) in &items {
            if sha256(data) != *hash {
                return Err(StoreError::Other(format!(
                    "source returned corrupt bytes for {hash:?}"
                )));
            }
            buffered_bytes = buffered_bytes.saturating_add(data.len() as u64);
        }
        batch.verified = batch.verified.saturating_add(items.len());
        batch.peak_buffered_bytes = batch.peak_buffered_bytes.max(buffered_bytes);
        let report = target.put_many_report_sync(&items)?;
        batch.inserted = batch.inserted.saturating_add(report.inserted);
        batch.inserted_bytes = batch.inserted_bytes.saturating_add(report.inserted_bytes);
        batch.write_batches = batch.write_batches.saturating_add(1);
        next = next.saturating_add(items.len());
    }
    Ok(batch)
}
