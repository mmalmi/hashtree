use crate::{LmdbBlobReader, PoolCatalogLocation, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::{sha256, types::Hash};
use std::time::{Duration, Instant};

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
    /// Ordered content-address/length pairs for every source payload verified
    /// in this page. Final migration receipts stream these compact records
    /// into one source-content digest without rereading multi-terabyte bodies.
    pub verified_source_entries: Vec<(Hash, u64)>,
    /// Ordered hash/declared-size pairs for the complete scanned source page,
    /// including entries whose exact-size Stored target record let the
    /// reconciliation path skip loading source bytes.
    pub source_entries: Vec<(Hash, u64)>,
    pub inserted: usize,
    pub inserted_bytes: u64,
    /// Number of byte-bounded writes used to commit this cursor page.
    pub write_batches: usize,
    /// Maximum bytes of complete blob payloads owned at once.
    pub peak_buffered_bytes: u64,
    /// Time spent scanning source hashes for this cursor page.
    pub scan_micros: u64,
    /// Time spent probing the Pool catalog for already committed hashes.
    pub catalog_probe_micros: u64,
    /// Time spent loading source payloads.
    pub source_read_micros: u64,
    /// Number of source-group reads used for this page.
    pub source_read_groups: usize,
    /// Time spent hash-verifying source payloads.
    pub source_verify_micros: u64,
    /// Time spent committing payloads and locations to the Pool.
    pub target_write_micros: u64,
    pub last_hash: Option<Hash>,
    pub source_exhausted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LmdbSourceAuditBatch {
    pub scanned: usize,
    pub verified: usize,
    pub verified_bytes: u64,
    pub verified_source_entries: Vec<(Hash, u64)>,
    pub peak_buffered_bytes: u64,
    pub scan_micros: u64,
    pub source_read_micros: u64,
    pub source_verify_micros: u64,
    pub last_hash: Option<Hash>,
    pub source_exhausted: bool,
}

/// Read and content-hash one bounded source page without opening or mutating a
/// target. This is the expensive source proof used before final downtime.
pub fn audit_lmdb_source_batch_with_max_buffer_bytes(
    source: &LmdbBlobReader,
    after: Option<Hash>,
    limit: usize,
    max_buffer_bytes: usize,
) -> Result<LmdbSourceAuditBatch, StoreError> {
    if limit == 0 {
        return Ok(LmdbSourceAuditBatch::default());
    }
    if max_buffer_bytes == 0 {
        return Err(StoreError::Other(
            "source audit max buffer bytes must be non-zero".into(),
        ));
    }
    let scan_started = Instant::now();
    let hashes = source.scan_hashes_after(after, limit)?;
    let scan_micros = elapsed_micros(scan_started.elapsed());
    if hashes.is_empty() {
        return Ok(LmdbSourceAuditBatch {
            source_exhausted: true,
            scan_micros,
            ..LmdbSourceAuditBatch::default()
        });
    }
    let mut batch = LmdbSourceAuditBatch {
        scanned: hashes.len(),
        scan_micros,
        last_hash: hashes.last().copied(),
        ..LmdbSourceAuditBatch::default()
    };
    let mut next = 0usize;
    while next < hashes.len() {
        let source_read_started = Instant::now();
        let items = source.read_hashes_bounded(&hashes[next..], max_buffer_bytes as u64)?;
        batch.source_read_micros = batch
            .source_read_micros
            .saturating_add(elapsed_micros(source_read_started.elapsed()));
        if items.is_empty() {
            return Err(StoreError::Other(
                "bounded source audit read made no progress".into(),
            ));
        }
        let source_verify_started = Instant::now();
        let mut buffered_bytes = 0u64;
        for (offset, (hash, data)) in items.iter().enumerate() {
            let expected_hash = hashes.get(next + offset).ok_or_else(|| {
                StoreError::Other("bounded source read exceeded the requested hash set".into())
            })?;
            if hash != expected_hash {
                return Err(StoreError::Other(format!(
                    "bounded source read returned {:?}, expected {:?}",
                    hash, expected_hash
                )));
            }
            if sha256(data) != *hash {
                return Err(StoreError::Other(format!(
                    "source returned corrupt bytes for {hash:?}"
                )));
            }
            let len = data.len() as u64;
            buffered_bytes = buffered_bytes.saturating_add(len);
            batch.verified_bytes = batch.verified_bytes.saturating_add(len);
            batch.verified_source_entries.push((*hash, len));
        }
        batch.source_verify_micros = batch
            .source_verify_micros
            .saturating_add(elapsed_micros(source_verify_started.elapsed()));
        batch.verified = batch.verified.saturating_add(items.len());
        batch.peak_buffered_bytes = batch.peak_buffered_bytes.max(buffered_bytes);
        next = next.saturating_add(items.len());
    }
    Ok(batch)
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
    migrate_lmdb_batch_with_max_buffer_bytes_and_authorizer(
        source,
        target,
        after,
        limit,
        max_buffer_bytes,
        &mut |_, _| Ok(()),
    )
}

/// Copy and verify one lexicographic cursor page, authorizing each bounded
/// target-write group immediately before it can mutate the Pool.
///
/// The callback is deliberately invoked after source reads and verification,
/// and only when the group contains at least one blob not already committed in
/// the target. This keeps a root-owned external fence fresh at the mutation
/// boundary without holding that authorization across potentially slow source
/// I/O.
pub fn migrate_lmdb_batch_with_max_buffer_bytes_and_authorizer(
    source: &LmdbBlobReader,
    target: &PoolStore,
    after: Option<Hash>,
    limit: usize,
    max_buffer_bytes: usize,
    authorize_target_write: &mut dyn FnMut(Option<Hash>, usize) -> Result<(), StoreError>,
) -> Result<PoolMigrationBatch, StoreError> {
    if limit == 0 {
        return Ok(PoolMigrationBatch::default());
    }
    if max_buffer_bytes == 0 {
        return Err(StoreError::Other(
            "migration max buffer bytes must be non-zero".into(),
        ));
    }
    let scan_started = Instant::now();
    let hashes = source.scan_hashes_after(after, limit)?;
    let scan_micros = elapsed_micros(scan_started.elapsed());
    if hashes.is_empty() {
        return Ok(PoolMigrationBatch {
            source_exhausted: true,
            scan_micros,
            ..PoolMigrationBatch::default()
        });
    }

    let mut batch = migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer(
        source,
        target,
        &hashes,
        after,
        max_buffer_bytes,
        authorize_target_write,
    )?;
    batch.scan_micros = scan_micros;
    Ok(batch)
}

/// Verify and migrate an explicit sorted source hash set.
///
/// Online audit recovery uses this after consulting its durable verified set,
/// so a resumed or catch-up pass rereads only source bodies that have not
/// already been proven against the exact target authority.
pub fn migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer(
    source: &LmdbBlobReader,
    target: &PoolStore,
    hashes: &[Hash],
    checkpoint_cursor: Option<Hash>,
    max_buffer_bytes: usize,
    authorize_target_write: &mut dyn FnMut(Option<Hash>, usize) -> Result<(), StoreError>,
) -> Result<PoolMigrationBatch, StoreError> {
    if hashes.is_empty() {
        return Ok(PoolMigrationBatch::default());
    }
    if max_buffer_bytes == 0 {
        return Err(StoreError::Other(
            "migration max buffer bytes must be non-zero".into(),
        ));
    }
    if hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(StoreError::Other(
            "explicit migration hashes must be unique and strictly sorted".into(),
        ));
    }
    let mut batch = PoolMigrationBatch {
        scanned: hashes.len(),
        last_hash: hashes.last().copied(),
        ..PoolMigrationBatch::default()
    };
    let catalog_probe_started = Instant::now();
    let committed = target.committed_hashes_in_sorted_candidates(hashes)?;
    batch.catalog_probe_micros = elapsed_micros(catalog_probe_started.elapsed());
    let max_buffer_bytes = max_buffer_bytes as u64;
    let mut next = 0usize;
    while next < hashes.len() {
        let source_read_started = Instant::now();
        let items = source.read_hashes_bounded(&hashes[next..], max_buffer_bytes)?;
        batch.source_read_micros = batch
            .source_read_micros
            .saturating_add(elapsed_micros(source_read_started.elapsed()));
        if items.is_empty() {
            return Err(StoreError::Other(
                "bounded source read made no migration progress".into(),
            ));
        }
        let source_verify_started = Instant::now();
        let mut buffered_bytes = 0u64;
        for (hash, data) in &items {
            if sha256(data) != *hash {
                return Err(StoreError::Other(format!(
                    "source returned corrupt bytes for {hash:?}"
                )));
            }
            buffered_bytes = buffered_bytes.saturating_add(data.len() as u64);
            batch
                .verified_source_entries
                .push((*hash, data.len() as u64));
        }
        batch.source_verify_micros = batch
            .source_verify_micros
            .saturating_add(elapsed_micros(source_verify_started.elapsed()));
        batch.verified = batch.verified.saturating_add(items.len());
        batch.peak_buffered_bytes = batch.peak_buffered_bytes.max(buffered_bytes);
        let item_count = items.len();
        let mut writes = Vec::new();
        for (offset, (hash, data)) in items.into_iter().enumerate() {
            if committed[next + offset] {
                let target_data = target.get_sync(&hash)?.ok_or_else(|| {
                    StoreError::Other(format!(
                        "target catalog claims committed blob {hash:?}, but its bytes are unavailable"
                    ))
                })?;
                if target_data != data {
                    return Err(StoreError::Other(format!(
                        "target catalog claims committed blob {hash:?}, but its bytes differ from source"
                    )));
                }
                batch.already_present = batch.already_present.saturating_add(1);
            } else {
                writes.push((hash, data));
            }
        }
        let target_write_started = Instant::now();
        if !writes.is_empty() {
            authorize_target_write(checkpoint_cursor, writes.len())?;
        }
        let report = target.put_many_report_sync(&writes)?;
        batch.target_write_micros = batch
            .target_write_micros
            .saturating_add(elapsed_micros(target_write_started.elapsed()));
        batch.inserted = batch.inserted.saturating_add(report.inserted);
        batch.inserted_bytes = batch.inserted_bytes.saturating_add(report.inserted_bytes);
        if !writes.is_empty() {
            batch.write_batches = batch.write_batches.saturating_add(1);
        }
        next = next.saturating_add(item_count);
    }
    Ok(batch)
}

/// Reconcile one sorted source page using source keys and target metadata.
///
/// `Stored` target records supply their exact size without touching the source
/// value. `Missing` and `Pending` records load and hash-check only the needed
/// source bodies. `Moving` and every declared-size mismatch fail closed. The
/// release path separately proves every target body and the complete frozen
/// source key set.
pub fn reconcile_lmdb_source_batch_with_max_buffer_bytes_and_authorizer(
    source: &LmdbBlobReader,
    target: &PoolStore,
    after: Option<Hash>,
    limit: usize,
    max_buffer_bytes: usize,
    authorize_target_write: &mut dyn FnMut(Option<Hash>, usize) -> Result<(), StoreError>,
) -> Result<PoolMigrationBatch, StoreError> {
    if limit == 0 {
        return Ok(PoolMigrationBatch::default());
    }
    if max_buffer_bytes == 0 {
        return Err(StoreError::Other(
            "migration max buffer bytes must be non-zero".into(),
        ));
    }
    let scan_started = Instant::now();
    let hashes = source.scan_hashes_after(after, limit)?;
    let scan_micros = elapsed_micros(scan_started.elapsed());
    if hashes.is_empty() {
        return Ok(PoolMigrationBatch {
            source_exhausted: true,
            scan_micros,
            ..PoolMigrationBatch::default()
        });
    }
    let mut batch = reconcile_lmdb_source_hashes_with_max_buffer_bytes_and_authorizer(
        source,
        target,
        &hashes,
        max_buffer_bytes,
        authorize_target_write,
    )?;
    batch.scan_micros = scan_micros;
    Ok(batch)
}

/// Reconcile exact ordered source keys, deriving sizes from the target when
/// already `Stored` and reading source bodies only for `Missing`/`Pending`.
pub fn reconcile_lmdb_source_hashes_with_max_buffer_bytes_and_authorizer(
    source: &LmdbBlobReader,
    target: &PoolStore,
    hashes: &[Hash],
    max_buffer_bytes: usize,
    authorize_target_write: &mut dyn FnMut(Option<Hash>, usize) -> Result<(), StoreError>,
) -> Result<PoolMigrationBatch, StoreError> {
    if hashes.is_empty() {
        return Ok(PoolMigrationBatch::default());
    }
    if max_buffer_bytes == 0 {
        return Err(StoreError::Other(
            "migration max buffer bytes must be non-zero".into(),
        ));
    }
    if hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(StoreError::Other(
            "source reconciliation hashes must be unique and strictly sorted".into(),
        ));
    }

    let catalog_probe_started = Instant::now();
    let locations = target.catalog_locations_in_sorted_candidates(hashes)?;
    let catalog_probe_micros = elapsed_micros(catalog_probe_started.elapsed());
    if locations.len() != hashes.len() {
        return Err(StoreError::Other(
            "target catalog lookup returned the wrong result count".into(),
        ));
    }

    let mut source_entries = Vec::with_capacity(hashes.len());
    let mut needed = Vec::new();
    let mut already_present = 0usize;
    for (hash, location) in hashes.iter().copied().zip(locations) {
        match location {
            PoolCatalogLocation::Stored { size, .. } => {
                source_entries.push((hash, size));
                already_present = already_present.saturating_add(1);
            }
            PoolCatalogLocation::Pending { size, .. } => {
                needed.push((hash, Some(size)));
            }
            PoolCatalogLocation::Missing => needed.push((hash, None)),
            PoolCatalogLocation::Moving { size, .. } => {
                return Err(StoreError::Other(format!(
                    "target Moving location is non-terminal for {hash:?} (declared size {size})"
                )));
            }
        }
    }

    let mut batch = PoolMigrationBatch {
        scanned: hashes.len(),
        already_present,
        catalog_probe_micros,
        last_hash: hashes.last().copied(),
        ..PoolMigrationBatch::default()
    };
    let max_buffer_bytes = max_buffer_bytes as u64;
    let mut next = 0usize;
    while next < needed.len() {
        let source_read_started = Instant::now();
        let needed_hashes = needed[next..]
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<Vec<_>>();
        let items = source.read_hashes_bounded(&needed_hashes, max_buffer_bytes)?;
        batch.source_read_micros = batch
            .source_read_micros
            .saturating_add(elapsed_micros(source_read_started.elapsed()));
        if items.is_empty() {
            return Err(StoreError::Other(
                "bounded key-only source reconciliation read made no progress".into(),
            ));
        }
        batch.source_read_groups = batch.source_read_groups.saturating_add(1);

        let source_verify_started = Instant::now();
        let mut buffered_bytes = 0u64;
        let mut writes = Vec::with_capacity(items.len());
        for (offset, (hash, data)) in items.into_iter().enumerate() {
            let (expected_hash, pending_size) = needed.get(next + offset).ok_or_else(|| {
                StoreError::Other(
                    "bounded key-only source read exceeded the requested hash set".into(),
                )
            })?;
            let size = data.len() as u64;
            if hash != *expected_hash || sha256(&data) != *expected_hash {
                return Err(StoreError::Other(format!(
                    "source payload differs from its content-addressed key for {expected_hash:?}"
                )));
            }
            if pending_size.is_some_and(|expected_size| expected_size != size) {
                return Err(StoreError::Other(format!(
                    "target Pending size {} differs from source size {size} for {expected_hash:?}",
                    pending_size.expect("checked pending size")
                )));
            }
            buffered_bytes = buffered_bytes.saturating_add(size);
            source_entries.push((*expected_hash, size));
            batch.verified_source_entries.push((*expected_hash, size));
            writes.push((*expected_hash, data));
        }
        batch.source_verify_micros = batch
            .source_verify_micros
            .saturating_add(elapsed_micros(source_verify_started.elapsed()));
        batch.verified = batch.verified.saturating_add(writes.len());
        batch.peak_buffered_bytes = batch.peak_buffered_bytes.max(buffered_bytes);
        authorize_target_write(writes.last().map(|(hash, _)| *hash), writes.len())?;
        let target_write_started = Instant::now();
        let report = target.put_many_optimistic_report_sync(&writes)?;
        batch.target_write_micros = batch
            .target_write_micros
            .saturating_add(elapsed_micros(target_write_started.elapsed()));
        batch.inserted = batch.inserted.saturating_add(report.inserted);
        batch.inserted_bytes = batch.inserted_bytes.saturating_add(report.inserted_bytes);
        batch.write_batches = batch.write_batches.saturating_add(1);
        next = next.saturating_add(writes.len());
    }

    source_entries.sort_unstable_by_key(|(hash, _)| *hash);
    if source_entries.len() != hashes.len()
        || source_entries
            .iter()
            .zip(hashes)
            .any(|((entry_hash, _), expected_hash)| entry_hash != expected_hash)
    {
        return Err(StoreError::Other(
            "key-only source reconciliation did not cover the exact source page".into(),
        ));
    }
    batch.source_entries = source_entries;
    Ok(batch)
}

/// Reconcile an exact, receipt-proven sorted hash/size page.
///
/// This is the final-union counterpart to the source-cursor API above. It
/// never scans source keys or stats source blobs: the caller supplies the
/// immutable evidence records and this function loads only bodies absent from
/// an exact-size terminal `Stored` target location.
pub fn reconcile_lmdb_source_entries_with_max_buffer_bytes_and_authorizer(
    source: &LmdbBlobReader,
    target: &PoolStore,
    source_entries: &[(Hash, u64)],
    max_buffer_bytes: usize,
    authorize_target_write: &mut dyn FnMut(Option<Hash>, usize) -> Result<(), StoreError>,
) -> Result<PoolMigrationBatch, StoreError> {
    let union_entries = source_entries
        .iter()
        .map(|(hash, size)| (*hash, *size, 0usize))
        .collect::<Vec<_>>();
    reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer(
        &[source],
        target,
        &union_entries,
        max_buffer_bytes,
        authorize_target_write,
    )
}

/// Reconcile one globally sorted, receipt-proven union page.
///
/// The target catalog is probed exactly once for the whole page. Missing and
/// exact-size Pending bodies are then read in globally byte-bounded chunks,
/// grouped by source within each chunk, and committed in global hash order.
/// Thus interleaved manifests cost at most one read per participating source
/// per byte-bounded chunk rather than one read transaction per alternating
/// hash.
pub fn reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer(
    sources: &[&LmdbBlobReader],
    target: &PoolStore,
    source_entries: &[(Hash, u64, usize)],
    max_buffer_bytes: usize,
    authorize_target_write: &mut dyn FnMut(Option<Hash>, usize) -> Result<(), StoreError>,
) -> Result<PoolMigrationBatch, StoreError> {
    if source_entries.is_empty() {
        return Ok(PoolMigrationBatch::default());
    }
    if sources.is_empty() {
        return Err(StoreError::Other(
            "receipt-proven source union has no body sources".into(),
        ));
    }
    if max_buffer_bytes == 0 {
        return Err(StoreError::Other(
            "migration max buffer bytes must be non-zero".into(),
        ));
    }
    if source_entries
        .windows(2)
        .any(|entries| entries[0].0 >= entries[1].0)
    {
        return Err(StoreError::Other(
            "receipt-proven source entries must be strictly sorted".into(),
        ));
    }
    if source_entries
        .iter()
        .any(|(_, _, source_index)| *source_index >= sources.len())
    {
        return Err(StoreError::Other(
            "receipt-proven source union references an unknown body source".into(),
        ));
    }
    let hashes = source_entries
        .iter()
        .map(|(hash, _, _)| *hash)
        .collect::<Vec<_>>();
    let catalog_probe_started = Instant::now();
    let locations = target.catalog_locations_in_sorted_candidates(&hashes)?;
    let catalog_probe_micros = elapsed_micros(catalog_probe_started.elapsed());
    if locations.len() != hashes.len() {
        return Err(StoreError::Other(
            "target catalog lookup returned the wrong result count".into(),
        ));
    }

    let mut needed = Vec::new();
    let mut already_present = 0usize;
    for ((hash, expected_size, source_index), location) in
        source_entries.iter().copied().zip(locations)
    {
        match location {
            PoolCatalogLocation::Stored { size, .. } if size == expected_size => {
                already_present = already_present.saturating_add(1);
            }
            PoolCatalogLocation::Stored { size, .. } => {
                return Err(StoreError::Other(format!(
                    "target Stored size {size} differs from source size {expected_size} for {hash:?}"
                )));
            }
            PoolCatalogLocation::Pending { size, .. } if size == expected_size => {
                needed.push((hash, expected_size, source_index));
            }
            PoolCatalogLocation::Missing => needed.push((hash, expected_size, source_index)),
            PoolCatalogLocation::Pending { size, .. } => {
                return Err(StoreError::Other(format!(
                    "target Pending size {size} differs from source size {expected_size} for {hash:?}"
                )));
            }
            PoolCatalogLocation::Moving { size, .. } => {
                return Err(StoreError::Other(format!(
                    "target Moving location is non-terminal for {hash:?} (declared size {size})"
                )));
            }
        }
    }

    let mut batch = PoolMigrationBatch {
        scanned: hashes.len(),
        already_present,
        source_entries: source_entries
            .iter()
            .map(|(hash, size, _)| (*hash, *size))
            .collect(),
        catalog_probe_micros,
        last_hash: hashes.last().copied(),
        ..PoolMigrationBatch::default()
    };
    let max_buffer_bytes = max_buffer_bytes as u64;
    let mut next = 0usize;
    while next < needed.len() {
        let mut end = next;
        let mut chunk_bytes = 0u64;
        while end < needed.len() {
            let size = needed[end].1;
            if end > next && chunk_bytes.saturating_add(size) > max_buffer_bytes {
                break;
            }
            chunk_bytes = chunk_bytes.saturating_add(size);
            end += 1;
            if chunk_bytes >= max_buffer_bytes {
                break;
            }
        }
        let mut bodies = std::collections::HashMap::with_capacity(end - next);
        let source_read_started = Instant::now();
        for (source_index, source) in sources.iter().enumerate() {
            let source_hashes = needed[next..end]
                .iter()
                .filter_map(|(hash, _, candidate_source)| {
                    (*candidate_source == source_index).then_some(*hash)
                })
                .collect::<Vec<_>>();
            if source_hashes.is_empty() {
                continue;
            }
            let items = source.read_hashes_bounded(&source_hashes, max_buffer_bytes)?;
            if items.len() != source_hashes.len() {
                return Err(StoreError::Other(
                    "bounded source-union read returned an incomplete body group".into(),
                ));
            }
            batch.source_read_groups = batch.source_read_groups.saturating_add(1);
            for (hash, data) in items {
                if bodies.insert(hash, data).is_some() {
                    return Err(StoreError::Other(
                        "source-union body group returned a duplicate hash".into(),
                    ));
                }
            }
        }
        batch.source_read_micros = batch
            .source_read_micros
            .saturating_add(elapsed_micros(source_read_started.elapsed()));
        if bodies.len() != end - next {
            return Err(StoreError::Other(
                "source-union body reads did not cover the exact needed chunk".into(),
            ));
        }
        let source_verify_started = Instant::now();
        let mut items = Vec::with_capacity(end - next);
        for (expected_hash, expected_size, _) in &needed[next..end] {
            let data = bodies.remove(expected_hash).ok_or_else(|| {
                StoreError::Other(format!("source-union body group omitted {expected_hash:?}"))
            })?;
            if data.len() as u64 != *expected_size || sha256(&data) != *expected_hash {
                return Err(StoreError::Other(format!(
                    "source payload differs from its receipt-bound hash/size for {expected_hash:?}"
                )));
            }
            batch
                .verified_source_entries
                .push((*expected_hash, *expected_size));
            items.push((*expected_hash, data));
        }
        batch.source_verify_micros = batch
            .source_verify_micros
            .saturating_add(elapsed_micros(source_verify_started.elapsed()));
        batch.verified = batch.verified.saturating_add(items.len());
        batch.peak_buffered_bytes = batch.peak_buffered_bytes.max(chunk_bytes);
        authorize_target_write(items.last().map(|(hash, _)| *hash), items.len())?;
        let target_write_started = Instant::now();
        let report = target.put_many_optimistic_report_sync(&items)?;
        batch.target_write_micros = batch
            .target_write_micros
            .saturating_add(elapsed_micros(target_write_started.elapsed()));
        batch.inserted = batch.inserted.saturating_add(report.inserted);
        batch.inserted_bytes = batch.inserted_bytes.saturating_add(report.inserted_bytes);
        batch.write_batches = batch.write_batches.saturating_add(1);
        next = end;
    }
    Ok(batch)
}

fn elapsed_micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}
