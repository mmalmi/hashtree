use super::*;
use hashtree_core::MemoryStore;
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStoreOptions};
use heed::EnvFlags;
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
use std::path::{Path, PathBuf};

fn event(id: &str, created_at: u64, kind: u32) -> StoredNostrEvent {
    StoredNostrEvent {
        id: id.to_string(),
        pubkey: "11".repeat(32),
        created_at,
        kind,
        tags: vec![vec!["t".to_string(), "Rust".to_string()]],
        content: id.to_string(),
        sig: "22".repeat(64),
    }
}

fn raw_database_contents(
    spool: &BulkProjectionSpool,
    database: &Database<Bytes, Bytes>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let rtxn = spool.env.read_txn().unwrap();
    database
        .iter(&rtxn)
        .unwrap()
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn open_read_only_spool(source: &Path, read_ahead: bool) -> BulkProjectionSpool {
    let mut options = EnvOpenOptions::new();
    options
        .map_size(BULK_PROJECTION_MAP_SIZE)
        .max_dbs(3)
        .max_readers(32);
    unsafe {
        let mut flags = EnvFlags::READ_ONLY;
        if !read_ahead {
            flags |= EnvFlags::NO_READ_AHEAD;
        }
        options.flags(flags);
    }
    let env = unsafe { options.open(source) }
        .unwrap_or_else(|error| panic!("open {} read-only: {error}", source.display()));
    let rtxn = env
        .read_txn()
        .unwrap_or_else(|error| panic!("open {} database txn: {error}", source.display()));
    let open_database = |name| {
        env.open_database(&rtxn, Some(name))
            .unwrap_or_else(|error| panic!("open {name} in {}: {error}", source.display()))
            .unwrap_or_else(|| panic!("missing {name} in {}", source.display()))
    };
    let events = open_database("events");
    let slots = open_database("slots");
    let entries = open_database("entries");
    // LMDB requires a read-only transaction that opened named databases to
    // be committed before those DBI handles are used by later transactions.
    // Dropping it aborts the DBI publication and the next cursor returns
    // EINVAL in a multi-process environment.
    rtxn.commit()
        .unwrap_or_else(|error| panic!("publish {} database handles: {error}", source.display()));
    BulkProjectionSpool {
        env,
        entries,
        events,
        slots,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BatchedCompareReport {
    count: u64,
    shared_sha256: String,
    batches: u64,
    peak_live_payload_bytes: usize,
    peak_owned_capacity_bytes: usize,
}

#[derive(Clone, Copy)]
struct BufferedRecord {
    key_start: usize,
    key_len: usize,
    value_start: usize,
    value_len: usize,
}

struct ContiguousRecordBatch {
    arena: Vec<u8>,
    records: Vec<BufferedRecord>,
}

impl ContiguousRecordBatch {
    fn new(max_rows: usize, max_bytes: usize) -> Self {
        Self {
            arena: Vec::with_capacity(max_bytes),
            records: Vec::with_capacity(max_rows),
        }
    }

    fn clear(&mut self) {
        self.arena.clear();
        self.records.clear();
    }

    fn push(&mut self, key: &[u8], value: &[u8]) {
        let key_start = self.arena.len();
        self.arena.extend_from_slice(key);
        let value_start = self.arena.len();
        self.arena.extend_from_slice(value);
        self.records.push(BufferedRecord {
            key_start,
            key_len: key.len(),
            value_start,
            value_len: value.len(),
        });
    }

    fn record(&self, index: usize) -> (&[u8], &[u8]) {
        let record = self.records[index];
        (
            &self.arena[record.key_start..record.key_start + record.key_len],
            &self.arena[record.value_start..record.value_start + record.value_len],
        )
    }

    fn payload_bytes(&self) -> usize {
        self.arena.len()
    }

    fn owned_capacity_bytes(&self) -> usize {
        self.arena.capacity().saturating_add(
            self.records
                .capacity()
                .saturating_mul(std::mem::size_of::<BufferedRecord>()),
        )
    }
}

#[derive(Clone, Copy)]
struct BatchedCompareProgress {
    left_payload_bytes: usize,
    pending_left_payload_bytes: usize,
    owned_capacity_bytes: usize,
    peak_live_payload_bytes: usize,
}

fn compare_borrowed_record_batches<'left, 'right, L, R, F>(
    mut left: L,
    mut right: R,
    name: &str,
    max_rows: usize,
    max_bytes: usize,
    mut progress: F,
) -> std::result::Result<BatchedCompareReport, String>
where
    L: Iterator<Item = std::result::Result<(&'left [u8], &'left [u8]), String>>,
    R: Iterator<Item = std::result::Result<(&'right [u8], &'right [u8]), String>>,
    F: FnMut(u64, u64, BatchedCompareProgress),
{
    use sha2::{Digest, Sha256};

    if max_rows == 0 || max_bytes == 0 {
        return Err("batch row and byte limits must be non-zero".to_string());
    }
    // The left side is copied into one reusable arena, so this loop performs
    // O(batches) allocations instead of two allocations for every database
    // record. The right side remains borrowed and is compared one row at a
    // time; a malformed large right value therefore cannot be accumulated
    // across an entire batch.
    let mut left_batch = ContiguousRecordBatch::new(max_rows, max_bytes);
    let mut pending_left = None;
    let mut digest = Sha256::new();
    digest.update(name.as_bytes());
    digest.update([0]);
    let mut count = 0u64;
    let mut batches = 0u64;
    let mut peak_live_payload_bytes = 0usize;
    let mut peak_owned_capacity_bytes = left_batch.owned_capacity_bytes();
    loop {
        left_batch.clear();
        while left_batch.records.len() < max_rows {
            let record = match pending_left.take() {
                Some(record) => record,
                None => match left.next() {
                    Some(Ok(record)) => record,
                    Some(Err(error)) => {
                        return Err(format!("read first {name} entry: {error}"));
                    }
                    None => break,
                },
            };
            let record_bytes = record.0.len().saturating_add(record.1.len());
            if !left_batch.records.is_empty()
                && left_batch.payload_bytes().saturating_add(record_bytes) > max_bytes
            {
                pending_left = Some(record);
                break;
            }
            left_batch.push(record.0, record.1);
            peak_owned_capacity_bytes =
                peak_owned_capacity_bytes.max(left_batch.owned_capacity_bytes());
            // A single record larger than the byte limit is permitted and
            // fully accounted. Do not fetch and retain another record while
            // that oversized batch is compared.
            if left_batch.payload_bytes() >= max_bytes {
                break;
            }
        }
        if left_batch.records.is_empty() {
            return match right.next() {
                None => Ok(BatchedCompareReport {
                    count,
                    shared_sha256: hex::encode(digest.finalize()),
                    batches,
                    peak_live_payload_bytes,
                    peak_owned_capacity_bytes,
                }),
                Some(Ok((key, _))) => Err(format!(
                    "{name} first spool ended at row {count}; second_key_prefix={}",
                    hex::encode(&key[..key.len().min(32)]),
                )),
                Some(Err(error)) => Err(format!("read extra second {name} entry: {error}")),
            };
        }

        let pending_left_payload_bytes = pending_left
            .as_ref()
            .map_or(0, |(key, value)| key.len().saturating_add(value.len()));
        let left_payload_bytes = left_batch.payload_bytes();
        peak_live_payload_bytes = peak_live_payload_bytes
            .max(left_payload_bytes.saturating_add(pending_left_payload_bytes));
        for index in 0..left_batch.records.len() {
            let Some(item) = right.next() else {
                return Err(format!("{name} second spool ended at row {count}"));
            };
            let (right_key, right_value) =
                item.map_err(|error| format!("read second {name} entry: {error}"))?;
            peak_live_payload_bytes = peak_live_payload_bytes.max(
                left_payload_bytes
                    .saturating_add(pending_left_payload_bytes)
                    .saturating_add(right_key.len())
                    .saturating_add(right_value.len()),
            );
            let (left_key, left_value) = left_batch.record(index);
            if left_key != right_key {
                return Err(format!(
                    "{name} key mismatch at row {count}: left_prefix={} right_prefix={}",
                    hex::encode(&left_key[..left_key.len().min(32)]),
                    hex::encode(&right_key[..right_key.len().min(32)]),
                ));
            }
            if left_value != right_value {
                return Err(format!(
                    "{name} value mismatch at row {count}, key_prefix={}",
                    hex::encode(&left_key[..left_key.len().min(32)]),
                ));
            }
            digest.update((left_key.len() as u64).to_be_bytes());
            digest.update(left_key);
            digest.update((left_value.len() as u64).to_be_bytes());
            digest.update(left_value);
            count = count.saturating_add(1);
        }
        batches = batches.saturating_add(1);
        progress(
            batches,
            count,
            BatchedCompareProgress {
                left_payload_bytes,
                pending_left_payload_bytes,
                owned_capacity_bytes: left_batch.owned_capacity_bytes(),
                peak_live_payload_bytes,
            },
        );
    }
}

#[test]
fn read_only_spool_opener_publishes_named_database_handles() {
    const CHILD_PATH_ENV: &str = "HTREE_TEST_READ_ONLY_SPOOL_CHILD_PATH";
    const CHILD_MODE_ENV: &str = "HTREE_TEST_READ_ONLY_SPOOL_CHILD_MODE";
    if let (Some(child_path), Some(child_mode)) = (
        std::env::var_os(CHILD_PATH_ENV),
        std::env::var_os(CHILD_MODE_ENV),
    ) {
        match child_mode.to_str().unwrap() {
            "write" => {
                let spool = BulkProjectionSpool::open(Path::new(&child_path)).unwrap();
                let mut wtxn = spool.env.write_txn().unwrap();
                spool
                    .events
                    .put(&mut wtxn, b"event-key", b"event-value")
                    .unwrap();
                spool
                    .slots
                    .put(&mut wtxn, b"slot-key", b"slot-value")
                    .unwrap();
                spool
                    .entries
                    .put(&mut wtxn, b"entry-key", b"entry-value")
                    .unwrap();
                wtxn.commit().unwrap();
            }
            "read" => {
                let read_only = open_read_only_spool(Path::new(&child_path), true);
                let rtxn = read_only.env.read_txn().unwrap();
                for (database, expected_key, expected_value) in [
                    (
                        read_only.events,
                        b"event-key".as_slice(),
                        b"event-value".as_slice(),
                    ),
                    (
                        read_only.slots,
                        b"slot-key".as_slice(),
                        b"slot-value".as_slice(),
                    ),
                    (
                        read_only.entries,
                        b"entry-key".as_slice(),
                        b"entry-value".as_slice(),
                    ),
                ] {
                    assert_eq!(
                        database.get(&rtxn, expected_key).unwrap(),
                        Some(expected_value)
                    );
                    let mut cursor = database.iter(&rtxn).unwrap();
                    assert_eq!(
                        cursor.next().unwrap().unwrap(),
                        (expected_key, expected_value)
                    );
                    assert!(cursor.next().is_none());
                }
            }
            mode => panic!("unknown read-only spool child mode {mode}"),
        }
        return;
    }

    // Keep both LMDB open phases in isolated processes. Besides matching the
    // production multi-process sequence, this avoids introducing another
    // process-global heed environment into a parallel unit-test process.
    let temp = tempfile::tempdir().unwrap();
    for mode in ["write", "read"] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::nostr_index::bulk_projection::tests::read_only_spool_opener_publishes_named_database_handles",
                "--nocapture",
            ])
            .env(CHILD_PATH_ENV, temp.path())
            .env(CHILD_MODE_ENV, mode)
            .status()
            .unwrap();
        assert!(status.success(), "read-only spool {mode} child failed");
    }
}

#[test]
fn batched_compare_preserves_digest_bounds_mismatches_errors_and_end_of_stream() {
    use sha2::{Digest, Sha256};

    let oversized = vec![b'x'; 80];
    let records = vec![
        (b"a".to_vec(), b"one".to_vec()),
        (b"b".to_vec(), oversized.clone()),
        (b"c".to_vec(), b"three".to_vec()),
    ];
    let mut progress_samples = Vec::new();
    let report = compare_borrowed_record_batches(
        records
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        records
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        "entries",
        2,
        16,
        |batch, count, progress| progress_samples.push((batch, count, progress)),
    )
    .unwrap();
    let mut expected = Sha256::new();
    expected.update(b"entries");
    expected.update([0]);
    for (key, value) in &records {
        expected.update((key.len() as u64).to_be_bytes());
        expected.update(key);
        expected.update((value.len() as u64).to_be_bytes());
        expected.update(value);
    }
    assert_eq!(report.count, records.len() as u64);
    assert_eq!(report.shared_sha256, hex::encode(expected.finalize()));
    assert_eq!(report.batches, 3);
    assert_eq!(progress_samples[0].0, 1);
    assert_eq!(progress_samples[0].1, 1);
    assert_eq!(progress_samples[0].2.left_payload_bytes, 4);
    assert_eq!(
        progress_samples[0].2.pending_left_payload_bytes,
        b"b".len() + oversized.len(),
        "the deferred left record must be included while the current batch is compared"
    );
    assert!(
        progress_samples[0].2.peak_live_payload_bytes
            >= progress_samples[0].2.left_payload_bytes
                + progress_samples[0].2.pending_left_payload_bytes
                + 4,
        "live-byte accounting must include the batch, pending left row, and streaming right row"
    );
    assert!(
        report.peak_live_payload_bytes >= 2 * (b"b".len() + oversized.len()),
        "oversized single record and streaming peer must be included in live-byte accounting"
    );
    assert!(
        report.peak_owned_capacity_bytes
            >= b"b".len() + oversized.len() + std::mem::size_of::<BufferedRecord>(),
        "oversized single record and offset metadata must be included in owned capacity"
    );

    let different_key = [(b"d".to_vec(), b"one".to_vec())];
    assert!(compare_borrowed_record_batches(
        records[..1]
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        different_key
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("key mismatch"));
    let different_value = [(b"a".to_vec(), b"two".to_vec())];
    assert!(compare_borrowed_record_batches(
        records[..1]
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        different_value
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("value mismatch"));
    assert!(compare_borrowed_record_batches(
        records[..1]
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        std::iter::empty(),
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("second spool ended"));
    assert!(compare_borrowed_record_batches(
        std::iter::empty(),
        records[..1]
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("first spool ended"));

    let left_error = std::iter::once(Err::<(&[u8], &[u8]), _>("left boom".to_string()));
    assert!(compare_borrowed_record_batches(
        left_error,
        std::iter::empty(),
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("read first entries entry: left boom"));
    let late_left_error = records[..1]
        .iter()
        .map(|(key, value)| Ok((key.as_slice(), value.as_slice())))
        .chain(std::iter::once(Err("late left boom".to_string())));
    assert!(compare_borrowed_record_batches(
        late_left_error,
        records[..1]
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("read first entries entry: late left boom"));
    let right_error = std::iter::once(Err::<(&[u8], &[u8]), _>("right boom".to_string()));
    assert!(compare_borrowed_record_batches(
        records[..1]
            .iter()
            .map(|(key, value)| Ok((key.as_slice(), value.as_slice()))),
        right_error,
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("read second entries entry: right boom"));
    let extra_right_error =
        std::iter::once(Err::<(&[u8], &[u8]), _>("extra right boom".to_string()));
    assert!(compare_borrowed_record_batches(
        std::iter::empty(),
        extra_right_error,
        "entries",
        2,
        16,
        |_, _, _| {},
    )
    .unwrap_err()
    .contains("read extra second entries entry: extra right boom"));
}

fn terminal_test_policy() -> IndexedNostrCrawlPolicy {
    IndexedNostrCrawlPolicy {
        base_root: None,
        author_allowlist_sha256: "aa".repeat(32),
        author_count: 100,
        relays: vec!["wss://relay.example".to_string()],
        require_all_relays: false,
        max_events_seen: None,
        max_authors: 100,
        max_follow_distance: Some(0),
        max_live_bytes: 1_000_000,
        author_batch_size: 1,
        checkpoint_authors: 1,
        per_author_event_limit: 256,
        per_author_kind_event_limit: None,
        per_author_live_bytes: Some(64 * 1024 * 1024),
        fetch_timeout_millis: 30_000,
        relay_event_max_bytes: Some(1024 * 1024),
        global_relay_scan: false,
        full_author_history: true,
        negentropy_only: false,
        relay_page_size: 1_000,
        max_relay_pages: 67,
        kinds: Some(vec![0, 1]),
    }
}

#[test]
fn terminal_gate_requires_the_exact_frozen_stage_boundary() {
    let policy = terminal_test_policy();
    let stage = StagedNostrCrawlState {
        version: 1,
        author_allowlist_source: Some("http://127.0.0.1/stage".to_string()),
        policy: policy.clone(),
        next_author: 42,
        events_seen: 1_000,
        events_selected: 900,
        live_bytes_selected: 123_456,
    };
    let mut bulk = BulkProjectionState {
        version: BULK_PROJECTION_VERSION,
        author_allowlist_source: Some("http://127.0.0.1/project".to_string()),
        policy,
        next_author: stage.next_author,
        segment_event_offset: 0,
        events_seen: stage.events_seen,
        events_selected: stage.events_selected,
        live_bytes_selected: stage.live_bytes_selected,
        built_roots: BTreeMap::new(),
        complete_root: None,
    };
    validate_terminal_stage_state(&bulk, &stage).unwrap();

    bulk.segment_event_offset = 1;
    assert!(validate_terminal_stage_state(&bulk, &stage)
        .unwrap_err()
        .to_string()
        .contains("inside a staged segment"));
    bulk.segment_event_offset = 0;
    bulk.events_selected += 1;
    assert!(validate_terminal_stage_state(&bulk, &stage)
        .unwrap_err()
        .to_string()
        .contains("counters differ"));
}

#[tokio::test]
async fn spool_replaces_old_events_and_bulk_builds_final_indexes_once() {
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let mut old = event(&"01".repeat(32), 10, 30_000);
    let mut new = event(&"02".repeat(32), 20, 30_000);
    let long_identifier = "d".repeat(1_024);
    old.tags
        .push(vec!["d".to_string(), long_identifier.clone()]);
    new.tags.push(vec!["d".to_string(), long_identifier]);
    let note = event(&"03".repeat(32), 15, 1);
    let old_cid = Cid::public([1; 32]);
    let new_cid = Cid::public([2; 32]);
    let note_cid = Cid::public([3; 32]);

    spool.apply(vec![(old, old_cid)]).unwrap();
    let report = spool
        .apply(vec![
            (note.clone(), note_cid.clone()),
            (new.clone(), new_cid.clone()),
        ])
        .unwrap();
    assert_eq!(report.replaced, 1);

    let store = Arc::new(MemoryStore::new());
    let by_id = spool
        .build_index_root(NostrEventIndex::ById, Arc::clone(&store), 8)
        .await
        .unwrap()
        .unwrap();
    let replaceable = spool
        .build_index_root(
            NostrEventIndex::ParameterizedReplaceable,
            Arc::clone(&store),
            8,
        )
        .await
        .unwrap()
        .unwrap();
    let btree = BTree::new(store, BTreeOptions { order: Some(8) });

    assert_eq!(
        btree.count_stored_links(Some(&by_id)).await.unwrap(),
        Some(2)
    );
    assert_eq!(
        btree.get_link(Some(&by_id), &new.id).await.unwrap(),
        Some(new_cid)
    );
    assert_eq!(
        btree.get_link(Some(&by_id), &note.id).await.unwrap(),
        Some(note_cid)
    );
    let (_, slot) = nostr_replaceable_slot(&new).unwrap();
    assert_eq!(
        btree.get_link(Some(&replaceable), &slot).await.unwrap(),
        Some(Cid::public([2; 32]))
    );
}

#[test]
fn rejected_spool_edge_marker_fails_without_changing_cursor_state() {
    let temp = tempfile::tempdir().unwrap();
    let bulk_dir = temp.path().join(INDEX_DIR).join(BULK_PROJECTION_DIR);
    std::fs::create_dir_all(&bulk_dir).unwrap();
    let state_path = bulk_dir.join(BULK_PROJECTION_STATE_FILE);
    let original_state = br#"{"version":2,"next_author":1234,"segment_event_offset":0}"#;
    std::fs::write(&state_path, original_state).unwrap();
    std::fs::write(
        bulk_dir.join(REJECTED_SPOOL_EDGE_STATE_FILE),
        b"rejected fast-forward state",
    )
    .unwrap();

    let error = reject_spool_edge_state_marker(temp.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("rejected bulk projection fast-forward marker"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(state_path).unwrap(), original_state);
}

#[test]
fn duplicate_spool_apply_requires_the_same_payload_and_cid() {
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let original = event(&"01".repeat(32), 10, 1);
    let cid = Cid::public([1; 32]);
    spool.apply(vec![(original.clone(), cid.clone())]).unwrap();

    let duplicate = spool.apply(vec![(original.clone(), cid.clone())]).unwrap();
    assert_eq!(duplicate.skipped, 1);

    let mut mismatched = original.clone();
    mismatched.content = "different".to_string();
    assert!(spool
        .apply(vec![(mismatched, cid)])
        .unwrap_err()
        .to_string()
        .contains("payload differs from duplicate spool record"));
    assert!(spool
        .apply(vec![(original, Cid::public([2; 32]))])
        .unwrap_err()
        .to_string()
        .contains("CID differs from duplicate spool record"));
}

#[test]
fn deferred_sorted_edges_match_ordered_long_key_mutations() {
    let baseline_dir = tempfile::tempdir().unwrap();
    let baseline = BulkProjectionSpool::open(baseline_dir.path()).unwrap();
    let optimized_dir = tempfile::tempdir().unwrap();
    let optimized = BulkProjectionSpool::open(optimized_dir.path()).unwrap();
    let shared_prefix = "a".repeat(ENTRY_CHUNK_SIZE);
    let first_key = format!("{shared_prefix}:first");
    let second_key = format!("{shared_prefix}:second");
    let first_cid = Cid::public([1; 32]);
    let replacement_cid = Cid::public([2; 32]);
    let second_cid = Cid::public([3; 32]);

    for spool in [&baseline, &optimized] {
        let mut wtxn = spool.env.write_txn().unwrap();
        spool
            .put_entry(&mut wtxn, NostrEventIndex::ById, &first_key, &first_cid)
            .unwrap();
        spool
            .put_entry(&mut wtxn, NostrEventIndex::ById, &second_key, &second_cid)
            .unwrap();
        wtxn.commit().unwrap();
    }

    let mut baseline_txn = baseline.env.write_txn().unwrap();
    baseline
        .remove_entry(&mut baseline_txn, NostrEventIndex::ById, &first_key)
        .unwrap();
    baseline
        .put_entry(
            &mut baseline_txn,
            NostrEventIndex::ById,
            &first_key,
            &replacement_cid,
        )
        .unwrap();
    baseline
        .remove_entry(&mut baseline_txn, NostrEventIndex::ById, &second_key)
        .unwrap();
    baseline_txn.commit().unwrap();

    let mut mutations = SpoolEdgeMutations::new();
    BulkProjectionSpool::defer_remove_entry(&mut mutations, NostrEventIndex::ById, &first_key);
    BulkProjectionSpool::defer_put_entry(
        &mut mutations,
        NostrEventIndex::ById,
        &first_key,
        &replacement_cid,
    );
    BulkProjectionSpool::defer_remove_entry(&mut mutations, NostrEventIndex::ById, &second_key);
    let mut optimized_txn = optimized.env.write_txn().unwrap();
    optimized
        .apply_deferred_edge_mutations(&mut optimized_txn, mutations)
        .unwrap();
    optimized_txn.commit().unwrap();

    assert_eq!(
        raw_database_contents(&optimized, &optimized.entries),
        raw_database_contents(&baseline, &baseline.entries)
    );
}

#[test]
fn deferred_edges_exhaustively_match_short_immediate_sequences() {
    #[derive(Debug, Clone, Copy)]
    enum Mutation {
        EnsureChildren,
        SetA,
        SetB,
        RemoveCid,
    }

    let baseline_dir = tempfile::tempdir().unwrap();
    let baseline = BulkProjectionSpool::open(baseline_dir.path()).unwrap();
    let optimized_dir = tempfile::tempdir().unwrap();
    let optimized = BulkProjectionSpool::open(optimized_dir.path()).unwrap();
    let cid_a = Cid::public([1; 32]);
    let cid_b = Cid::public([2; 32]);
    let child_cid = Cid::public([3; 32]);
    let alphabet = [
        Mutation::EnsureChildren,
        Mutation::SetA,
        Mutation::SetB,
        Mutation::RemoveCid,
    ];
    let mut case_number = 0usize;

    for initial in 0..4 {
        for sequence_len in 0..=3u32 {
            for encoded_sequence in 0..4usize.pow(sequence_len) {
                case_number += 1;
                let mut encoded = encoded_sequence;
                let mut sequence = Vec::with_capacity(sequence_len as usize);
                for _ in 0..sequence_len {
                    sequence.push(alphabet[encoded % alphabet.len()]);
                    encoded /= alphabet.len();
                }
                let label = format!("case-{case_number}-initial-{initial}-");
                let logical_key = format!(
                    "{label}{}",
                    "x".repeat(ENTRY_CHUNK_SIZE.saturating_sub(label.len()))
                );
                assert_eq!(logical_key.len(), ENTRY_CHUNK_SIZE);
                let child_key = format!("{logical_key}:child");

                for spool in [&baseline, &optimized] {
                    let mut wtxn = spool.env.write_txn().unwrap();
                    if initial == 2 || initial == 3 {
                        spool
                            .put_entry(&mut wtxn, NostrEventIndex::ById, &child_key, &child_cid)
                            .unwrap();
                    }
                    if initial == 1 || initial == 3 {
                        spool
                            .put_entry(&mut wtxn, NostrEventIndex::ById, &logical_key, &cid_a)
                            .unwrap();
                    }
                    wtxn.commit().unwrap();
                }

                let mut baseline_txn = baseline.env.write_txn().unwrap();
                for mutation in &sequence {
                    match mutation {
                        Mutation::EnsureChildren => baseline
                            .put_entry(
                                &mut baseline_txn,
                                NostrEventIndex::ById,
                                &child_key,
                                &child_cid,
                            )
                            .unwrap(),
                        Mutation::SetA => baseline
                            .put_entry(
                                &mut baseline_txn,
                                NostrEventIndex::ById,
                                &logical_key,
                                &cid_a,
                            )
                            .unwrap(),
                        Mutation::SetB => baseline
                            .put_entry(
                                &mut baseline_txn,
                                NostrEventIndex::ById,
                                &logical_key,
                                &cid_b,
                            )
                            .unwrap(),
                        Mutation::RemoveCid => baseline
                            .remove_entry(&mut baseline_txn, NostrEventIndex::ById, &logical_key)
                            .unwrap(),
                    }
                }
                baseline_txn.commit().unwrap();

                let mut mutations = SpoolEdgeMutations::new();
                for mutation in &sequence {
                    match mutation {
                        Mutation::EnsureChildren => BulkProjectionSpool::defer_put_entry(
                            &mut mutations,
                            NostrEventIndex::ById,
                            &child_key,
                            &child_cid,
                        ),
                        Mutation::SetA => BulkProjectionSpool::defer_put_entry(
                            &mut mutations,
                            NostrEventIndex::ById,
                            &logical_key,
                            &cid_a,
                        ),
                        Mutation::SetB => BulkProjectionSpool::defer_put_entry(
                            &mut mutations,
                            NostrEventIndex::ById,
                            &logical_key,
                            &cid_b,
                        ),
                        Mutation::RemoveCid => BulkProjectionSpool::defer_remove_entry(
                            &mut mutations,
                            NostrEventIndex::ById,
                            &logical_key,
                        ),
                    }
                }
                let mut optimized_txn = optimized.env.write_txn().unwrap();
                optimized
                    .apply_deferred_edge_mutations(&mut optimized_txn, mutations)
                    .unwrap();
                optimized_txn.commit().unwrap();

                let physical_key =
                    entry_edge_key(NostrEventIndex::ById, &[0; 32], logical_key.as_bytes());
                let baseline_txn = baseline.env.read_txn().unwrap();
                let optimized_txn = optimized.env.read_txn().unwrap();
                assert_eq!(
                    optimized
                        .entries
                        .get(&optimized_txn, &physical_key)
                        .unwrap(),
                    baseline.entries.get(&baseline_txn, &physical_key).unwrap(),
                    "initial={initial} sequence={sequence:?}"
                );
            }
        }
    }
}

#[test]
fn verified_plan_reprobes_duplicate_ids_that_can_change_presence_in_batch() {
    let duplicate = event(&"01".repeat(32), 10, 1);
    let cid = Cid::public([1; 32]);
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let plan = spool
        .plan_replay_batch(
            vec![duplicate.clone(), duplicate.clone()],
            &[cid.clone(), cid.clone()],
        )
        .unwrap();
    assert_eq!(
        plan.record_proofs,
        vec![SpoolRecordProof::Unknown, SpoolRecordProof::Unknown]
    );
    let report = spool
        .apply_verified_plan(
            plan.events
                .into_iter()
                .zip(plan.record_proofs)
                .map(|((event, planned), proof)| (event, planned.unwrap_or(cid.clone()), proof))
                .collect(),
        )
        .unwrap();
    assert_eq!(report.inserted, 1);
    assert_eq!(report.skipped, 1);
    let rtxn = spool.env.read_txn().unwrap();
    assert_eq!(spool.events.len(&rtxn).unwrap(), 1);
}

#[tokio::test]
async fn exact_spool_replay_reuses_cids_and_preserves_crash_replay_events() {
    let keys = Keys::generate();
    let profile = EventBuilder::new(Kind::Metadata, "profile")
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&keys)
        .unwrap();
    let note = EventBuilder::new(Kind::TextNote, "hello")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&keys)
        .unwrap();
    let original_events = [&profile, &note]
        .into_iter()
        .map(stored_event_from_nostr_sdk_event)
        .collect::<Vec<_>>();

    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store);
    let original_cids = event_store
        .store_event_blobs(original_events.clone())
        .await
        .unwrap();
    let mut event_cids = original_events
        .into_iter()
        .zip(original_cids)
        .collect::<Vec<_>>();
    event_cids.sort_unstable_by(|(left, _), (right, _)| right.id.cmp(&left.id));
    let (events, cids): (Vec<_>, Vec<_>) = event_cids.into_iter().unzip();
    assert!(events[0].id > events[1].id);
    spool
        .apply(events.clone().into_iter().zip(cids.clone()).collect())
        .unwrap();

    let replay = spool.plan_replay_batch(events.clone(), &cids).unwrap();
    assert_eq!(replay.reused_records, events.len());
    assert!(replay.missing_positions.is_empty());
    assert_eq!(
        replay
            .events
            .into_iter()
            .map(|(event, cid)| (event, cid.expect("reused target CID")))
            .collect::<Vec<_>>(),
        events.into_iter().zip(cids).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn exact_spool_replay_falls_back_for_missing_events_and_rejects_mismatch() {
    let keys = Keys::generate();
    let profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "profile")
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let note = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, "hello")
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap(),
    );

    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store);
    let cids = event_store
        .store_event_blobs([profile.clone(), note.clone()])
        .await
        .unwrap();
    spool
        .apply(vec![(profile.clone(), cids[0].clone())])
        .unwrap();

    let partial = spool
        .plan_replay_batch(
            vec![profile.clone(), note.clone()],
            &[cids[0].clone(), cids[1].clone()],
        )
        .unwrap();
    assert_eq!(partial.reused_records, 1);
    assert_eq!(partial.missing_positions, vec![1]);
    assert_eq!(partial.events[1].0, note);
    assert_eq!(partial.events[0].1, Some(cids[0].clone()));
    assert_eq!(partial.events[1].1, None);

    let mut mismatched_payload = profile.clone();
    mismatched_payload.content = "different".to_string();
    let payload_error = spool
        .plan_replay_batch(vec![mismatched_payload], &[cids[0].clone()])
        .unwrap_err();
    assert!(payload_error
        .to_string()
        .contains("payload differs from durable spool record"));

    let cid_error = spool
        .plan_replay_batch(vec![profile], &[Cid::public([9; 32])])
        .unwrap_err();
    assert!(cid_error
        .to_string()
        .contains("CID differs from durable spool record"));
}

#[tokio::test]
async fn mixed_replay_reuses_only_fully_readable_matching_durable_candidates() {
    let keys = Keys::generate();
    let profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "profile")
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let note = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, "hello")
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap(),
    );

    let durable = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(Arc::clone(&durable));
    let cids = event_store
        .store_event_blobs([profile.clone(), note.clone()])
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    spool
        .apply(vec![(profile.clone(), cids[0].clone())])
        .unwrap();

    let mut plan = spool
        .plan_replay_batch(
            vec![profile.clone(), note.clone()],
            &[cids[0].clone(), cids[1].clone()],
        )
        .unwrap();
    assert_eq!(
        plan.reuse_durable_candidates(&event_store, &cids)
            .await
            .unwrap(),
        1
    );
    assert!(plan.missing_positions.is_empty());
    assert_eq!(plan.events[1].1, Some(cids[1].clone()));
    let report = spool
        .apply(
            plan.events
                .into_iter()
                .map(|(event, cid)| (event, cid.unwrap()))
                .collect(),
        )
        .unwrap();
    assert_eq!(report.inserted, 1);
    assert_eq!(report.skipped, 1);

    let mut mismatched = note;
    mismatched.content = "different".to_string();
    let mismatch_temp = tempfile::tempdir().unwrap();
    let mismatch_spool = BulkProjectionSpool::open(mismatch_temp.path()).unwrap();
    let mut mismatch_plan = mismatch_spool
        .plan_replay_batch(vec![mismatched.clone()], &[cids[1].clone()])
        .unwrap();
    let mismatch_error = mismatch_plan
        .reuse_durable_candidates(&event_store, &[cids[1].clone()])
        .await
        .unwrap_err();
    assert!(mismatch_error
        .to_string()
        .contains("payload differs from durable target blob"));

    let missing_cid = Cid::public([9; 32]);
    let mut missing_plan = mismatch_spool
        .plan_replay_batch(vec![mismatched], std::slice::from_ref(&missing_cid))
        .unwrap();
    assert_eq!(
        missing_plan
            .reuse_durable_candidates(&event_store, &[missing_cid])
            .await
            .unwrap(),
        0
    );
    assert_eq!(missing_plan.missing_positions, vec![0]);
}

#[tokio::test]
async fn mixed_replay_stores_only_missing_events_and_preserves_replaceable_order() {
    let keys = Keys::generate();
    let old_profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "old")
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let existing_note = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, "existing")
            .custom_created_at(Timestamp::from_secs(15))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let new_profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "new")
            .custom_created_at(Timestamp::from_secs(30))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let losing_profile = stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::Metadata, "loser")
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap(),
    );
    let replay_events = vec![
        new_profile.clone(),
        old_profile.clone(),
        existing_note.clone(),
        losing_profile.clone(),
    ];

    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(Arc::clone(&store));
    let initial_events = vec![old_profile, existing_note.clone()];
    let initial_cids = event_store
        .store_event_blobs(initial_events.clone())
        .await
        .unwrap();
    let replay_cids = event_store
        .store_event_blobs(replay_events.clone())
        .await
        .unwrap();

    let baseline_dir = tempfile::tempdir().unwrap();
    let baseline = BulkProjectionSpool::open(baseline_dir.path()).unwrap();
    baseline
        .apply(
            initial_events
                .clone()
                .into_iter()
                .zip(initial_cids.clone())
                .collect(),
        )
        .unwrap();
    let baseline_report = baseline
        .apply(
            replay_events
                .clone()
                .into_iter()
                .zip(replay_cids.clone())
                .collect(),
        )
        .unwrap();

    let optimized_dir = tempfile::tempdir().unwrap();
    let optimized = BulkProjectionSpool::open(optimized_dir.path()).unwrap();
    optimized
        .apply(initial_events.into_iter().zip(initial_cids).collect())
        .unwrap();
    let plan = optimized
        .plan_replay_batch(replay_events, &replay_cids)
        .unwrap();
    assert_eq!(plan.reused_records, 2);
    assert_eq!(plan.missing_positions, vec![0, 3]);
    assert_eq!(
        plan.record_proofs,
        vec![
            SpoolRecordProof::Missing,
            SpoolRecordProof::Unknown,
            SpoolRecordProof::Present,
            SpoolRecordProof::Missing,
        ]
    );
    assert_eq!(plan.events[0].0, new_profile);
    assert_eq!(plan.events[3].0, losing_profile);
    let missing_cids = event_store
        .store_event_blobs(plan.events.iter().filter_map(|(event, cid)| {
            if cid.is_none() {
                Some(event.clone())
            } else {
                None
            }
        }))
        .await
        .unwrap();
    let mut missing_cids = missing_cids.into_iter();
    let planned_events = plan
        .events
        .into_iter()
        .zip(plan.record_proofs)
        .map(|((event, existing_cid), proof)| {
            (
                event,
                existing_cid.unwrap_or_else(|| missing_cids.next().unwrap()),
                proof,
            )
        })
        .collect();
    assert!(missing_cids.next().is_none());
    let optimized_report = optimized.apply_verified_plan(planned_events).unwrap();

    assert_eq!(optimized_report.inserted, baseline_report.inserted);
    assert_eq!(optimized_report.replaced, baseline_report.replaced);
    assert_eq!(optimized_report.skipped, baseline_report.skipped);
    assert_eq!(
        optimized_report.retained_events,
        baseline_report.retained_events
    );
    assert_eq!(
        raw_database_contents(&optimized, &optimized.events),
        raw_database_contents(&baseline, &baseline.events)
    );
    assert_eq!(
        raw_database_contents(&optimized, &optimized.slots),
        raw_database_contents(&baseline, &baseline.slots)
    );
    assert_eq!(
        raw_database_contents(&optimized, &optimized.entries),
        raw_database_contents(&baseline, &baseline.entries)
    );
    for index in NostrEventIndex::ALL {
        assert_eq!(
            optimized
                .build_index_root(index, Arc::clone(&store), 8)
                .await
                .unwrap(),
            baseline
                .build_index_root_unbuffered(index, Arc::clone(&store), 8)
                .await
                .unwrap()
        );
    }
    assert_eq!(
        optimized_report.retained_events,
        vec![new_profile, existing_note]
    );
}

#[tokio::test]
#[ignore = "requires an explicit real staged store and isolated LMDB target"]
async fn benchmark_real_bulk_projection_phase() {
    use hashtree_cli::HashtreeStore;
    use hashtree_config::StorageBackend;
    use hashtree_lmdb::{PoolMemberConfig, PoolStore, PoolStoreConfig};
    use heed::{CompactionOption, EnvFlags};

    let stage_dir = PathBuf::from(
        std::env::var("HTREE_BULK_BENCH_STAGE_DIR")
            .expect("HTREE_BULK_BENCH_STAGE_DIR must name a copied or read-only real stage"),
    );
    let output_dir = PathBuf::from(
        std::env::var("HTREE_BULK_BENCH_OUTPUT_DIR")
            .expect("HTREE_BULK_BENCH_OUTPUT_DIR must name an isolated benchmark target"),
    );
    let author = std::env::var("HTREE_BULK_BENCH_AUTHOR")
        .unwrap_or_else(|_| "3078".to_string())
        .parse::<usize>()
        .expect("HTREE_BULK_BENCH_AUTHOR");
    let offset = std::env::var("HTREE_BULK_BENCH_OFFSET")
        .unwrap_or_else(|_| "0".to_string())
        .parse::<usize>()
        .expect("HTREE_BULK_BENCH_OFFSET");
    let limit = std::env::var("HTREE_BULK_BENCH_LIMIT")
        .unwrap_or_else(|_| "32768".to_string())
        .parse::<usize>()
        .expect("HTREE_BULK_BENCH_LIMIT");
    let mode =
        std::env::var("HTREE_BULK_BENCH_MODE").expect("HTREE_BULK_BENCH_MODE must be explicit");
    if mode == "spool-copy" {
        let source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_SOURCE")
                .expect("HTREE_BULK_BENCH_SPOOL_SOURCE must name the real spool"),
        );
        std::fs::create_dir_all(&output_dir).expect("create spool snapshot output");
        let mut options = EnvOpenOptions::new();
        options.max_dbs(3).max_readers(32);
        unsafe {
            options.flags(EnvFlags::READ_ONLY | EnvFlags::NO_READ_AHEAD);
        }
        let env = unsafe { options.open(&source) }.expect("open real spool read-only");
        let started = Instant::now();
        env.copy_to_file(output_dir.join("data.mdb"), CompactionOption::Disabled)
            .expect("copy consistent real spool snapshot");
        eprintln!(
            "real_bulk_bench mode={mode} source={} output={} elapsed_ms={}",
            source.display(),
            output_dir.display(),
            started.elapsed().as_millis()
        );
        return;
    }
    if mode == "spool-digest" {
        use sha2::{Digest, Sha256};

        let source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_SOURCE")
                .expect("HTREE_BULK_BENCH_SPOOL_SOURCE must name the copied real spool"),
        );
        let spool = BulkProjectionSpool::open(&source).expect("open copied real spool");
        let rtxn = spool.env.read_txn().expect("open real spool digest txn");
        for (name, database) in [
            ("events", spool.events),
            ("slots", spool.slots),
            ("entries", spool.entries),
        ] {
            let started = Instant::now();
            let mut digest = Sha256::new();
            digest.update(name.as_bytes());
            digest.update([0]);
            let mut count = 0u64;
            for item in database.iter(&rtxn).expect("iterate real spool database") {
                let (key, value) = item.expect("read real spool database entry");
                digest.update((key.len() as u64).to_be_bytes());
                digest.update(key);
                digest.update((value.len() as u64).to_be_bytes());
                digest.update(value);
                count = count.saturating_add(1);
            }
            eprintln!(
                "real_bulk_bench mode={mode} database={name} count={count} sha256={} \
                 elapsed_ms={}",
                hex::encode(digest.finalize()),
                started.elapsed().as_millis(),
            );
        }
        return;
    }
    if mode == "spool-compare-entries-batched" {
        const MAX_BATCH_ROWS: usize = 65_536;
        const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

        let left_source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_SOURCE")
                .expect("HTREE_BULK_BENCH_SPOOL_SOURCE must name the first real spool"),
        );
        let right_source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_COMPARE")
                .expect("HTREE_BULK_BENCH_SPOOL_COMPARE must name the second real spool"),
        );
        let left = open_read_only_spool(&left_source, true);
        let right = open_read_only_spool(&right_source, true);
        let left_txn = left
            .env
            .read_txn()
            .expect("open first real batched comparison txn");
        let right_txn = right
            .env
            .read_txn()
            .expect("open second real batched comparison txn");
        let left_iter = left
            .entries
            .iter(&left_txn)
            .expect("iterate first real entries database")
            .map(|item| item.map_err(|error| error.to_string()));
        let right_iter = right
            .entries
            .iter(&right_txn)
            .expect("iterate second real entries database")
            .map(|item| item.map_err(|error| error.to_string()));
        let started = Instant::now();
        let report = compare_borrowed_record_batches(
            left_iter,
            right_iter,
            "entries",
            MAX_BATCH_ROWS,
            MAX_BATCH_BYTES,
            |batch, count, progress| {
                if batch == 1 || batch % 16 == 0 {
                    eprintln!(
                        "real_bulk_bench mode={mode} database=entries progress_batch={batch} \
                         progress_count={count} left_payload_bytes={} \
                         pending_left_payload_bytes={} owned_capacity_bytes={} \
                         peak_live_payload_bytes={} elapsed_ms={}",
                        progress.left_payload_bytes,
                        progress.pending_left_payload_bytes,
                        progress.owned_capacity_bytes,
                        progress.peak_live_payload_bytes,
                        started.elapsed().as_millis(),
                    );
                }
            },
        )
        .unwrap_or_else(|error| panic!("batched real entries comparison failed: {error}"));
        eprintln!(
            "real_bulk_bench mode={mode} database=entries count={} shared_sha256={} \
             batches={} peak_live_payload_bytes={} peak_owned_capacity_bytes={} \
             max_batch_rows={MAX_BATCH_ROWS} max_batch_bytes={MAX_BATCH_BYTES} elapsed_ms={}",
            report.count,
            report.shared_sha256,
            report.batches,
            report.peak_live_payload_bytes,
            report.peak_owned_capacity_bytes,
            started.elapsed().as_millis(),
        );
        return;
    }
    if mode == "spool-compare" {
        use sha2::{Digest, Sha256};

        let left_source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_SOURCE")
                .expect("HTREE_BULK_BENCH_SPOOL_SOURCE must name the first real spool"),
        );
        let right_source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_COMPARE")
                .expect("HTREE_BULK_BENCH_SPOOL_COMPARE must name the second real spool"),
        );
        // This comparison is one ordered streaming pass over both
        // environments. Keep kernel read-ahead enabled so a real
        // multi-gigabyte proof does not refault the same mapped pages for
        // every record under cgroup reclaim.
        let left = open_read_only_spool(&left_source, true);
        let right = open_read_only_spool(&right_source, true);
        let left_txn = left
            .env
            .read_txn()
            .expect("open first real spool comparison txn");
        let right_txn = right
            .env
            .read_txn()
            .expect("open second real spool comparison txn");
        for (name, left_database, right_database) in [
            ("events", left.events, right.events),
            ("slots", left.slots, right.slots),
            ("entries", left.entries, right.entries),
        ] {
            let started = Instant::now();
            let mut digest = Sha256::new();
            digest.update(name.as_bytes());
            digest.update([0]);
            let mut count = 0u64;
            let mut left_iter = left_database
                .iter(&left_txn)
                .expect("iterate first real spool database");
            let mut right_iter = right_database
                .iter(&right_txn)
                .expect("iterate second real spool database");
            loop {
                match (left_iter.next(), right_iter.next()) {
                    (Some(left_item), Some(right_item)) => {
                        let (left_key, left_value) =
                            left_item.expect("read first real spool database entry");
                        let (right_key, right_value) =
                            right_item.expect("read second real spool database entry");
                        assert_eq!(
                            left_key,
                            right_key,
                            "{name} key mismatch at row {count}: left_prefix={} right_prefix={}",
                            hex::encode(&left_key[..left_key.len().min(32)]),
                            hex::encode(&right_key[..right_key.len().min(32)]),
                        );
                        assert_eq!(
                            left_value,
                            right_value,
                            "{name} value mismatch at row {count}, key_prefix={}",
                            hex::encode(&left_key[..left_key.len().min(32)]),
                        );
                        digest.update((left_key.len() as u64).to_be_bytes());
                        digest.update(left_key);
                        digest.update((left_value.len() as u64).to_be_bytes());
                        digest.update(left_value);
                        count = count.saturating_add(1);
                    }
                    (None, None) => break,
                    (Some(left_item), None) => {
                        let (left_key, _) =
                            left_item.expect("read extra first real spool database entry");
                        panic!(
                            "{name} second spool ended at row {count}; first_key_prefix={}",
                            hex::encode(&left_key[..left_key.len().min(32)]),
                        );
                    }
                    (None, Some(right_item)) => {
                        let (right_key, _) =
                            right_item.expect("read extra second real spool database entry");
                        panic!(
                            "{name} first spool ended at row {count}; second_key_prefix={}",
                            hex::encode(&right_key[..right_key.len().min(32)]),
                        );
                    }
                }
            }
            eprintln!(
                "real_bulk_bench mode={mode} database={name} count={count} shared_sha256={} \
                 elapsed_ms={}",
                hex::encode(digest.finalize()),
                started.elapsed().as_millis(),
            );
        }
        return;
    }
    let target_map_size_bytes = std::env::var("HTREE_BULK_BENCH_TARGET_MAP_SIZE_BYTES")
        .unwrap_or_else(|_| (1024_u64 * 1024 * 1024).to_string())
        .parse::<u64>()
        .expect("HTREE_BULK_BENCH_TARGET_MAP_SIZE_BYTES");
    let target_capacity_bytes = std::env::var("HTREE_BULK_BENCH_TARGET_CAPACITY_BYTES")
        .unwrap_or_else(|_| (16_u64 * 1024 * 1024 * 1024).to_string())
        .parse::<u64>()
        .expect("HTREE_BULK_BENCH_TARGET_CAPACITY_BYTES");
    let open_target = || {
        let target = PoolStore::open(output_dir.join("pool"), PoolStoreConfig::default())
            .expect("open isolated PoolStore target");
        if target
            .members()
            .expect("list isolated target members")
            .is_empty()
        {
            let mut member =
                PoolMemberConfig::new(output_dir.join("member"), target_capacity_bytes)
                    .with_map_size_bytes(target_map_size_bytes)
                    .with_external_blobs(
                        output_dir.join("external"),
                        1,
                        true,
                        Some(64 * 1024 * 1024),
                    );
            member.max_read_concurrency = 64;
            member.max_write_concurrency = 8;
            target
                .add_member(member)
                .expect("add isolated PoolStore target member");
        }
        Arc::new(target)
    };
    if mode == "roots-compare-all" {
        let left_source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_SOURCE")
                .expect("HTREE_BULK_BENCH_SPOOL_SOURCE must name the first real spool"),
        );
        let right_source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_COMPARE")
                .expect("HTREE_BULK_BENCH_SPOOL_COMPARE must name the second real spool"),
        );
        let order = std::env::var("HTREE_BULK_BENCH_BTREE_ORDER")
            .unwrap_or_else(|_| "64".to_string())
            .parse::<usize>()
            .expect("HTREE_BULK_BENCH_BTREE_ORDER");
        let left = open_read_only_spool(&left_source, true);
        let right = open_read_only_spool(&right_source, true);
        let target = open_target();
        let btree = BTree::new(Arc::clone(&target), BTreeOptions { order: Some(order) });
        for index in NostrEventIndex::ALL {
            let left_started = Instant::now();
            let left_root = left
                .build_index_root(index, Arc::clone(&target), order)
                .await
                .expect("build first real index root")
                .unwrap_or_else(|| {
                    panic!(
                        "{} first real index unexpectedly produced no root",
                        index.name()
                    )
                });
            let left_ms = left_started.elapsed().as_millis();
            let right_started = Instant::now();
            let right_root = right
                .build_index_root(index, Arc::clone(&target), order)
                .await
                .expect("build second real index root")
                .unwrap_or_else(|| {
                    panic!(
                        "{} second real index unexpectedly produced no root",
                        index.name()
                    )
                });
            let right_ms = right_started.elapsed().as_millis();
            assert_eq!(
                left_root,
                right_root,
                "{} roots differ for byte-equal real spools",
                index.name(),
            );
            target.force_sync().expect("sync compared real index root");
            let validation_started = Instant::now();
            let report = btree
                .validate_link_tree(Some(&left_root))
                .await
                .expect("exhaustively validate compared real index root");
            assert!(
                report.nodes > 0,
                "{} compared real index root validated with zero nodes",
                index.name(),
            );
            assert!(
                report.links > 0,
                "{} compared real index root validated with zero links",
                index.name(),
            );
            let nodes = report.nodes;
            let links = report.links;
            let validation_ms = validation_started.elapsed().as_millis();
            eprintln!(
                "real_bulk_bench mode={mode} index={} root={} left_ms={left_ms} \
                 right_ms={right_ms} validation_ms={validation_ms} nodes={nodes} links={links} \
                 target_map_size_bytes={target_map_size_bytes} \
                 target_capacity_bytes={target_capacity_bytes}",
                index.name(),
                cid_to_nhash(&left_root).expect("encode compared real root"),
            );
        }
        return;
    }
    if mode == "root-buffered" || mode == "root-unbuffered" {
        let source = PathBuf::from(
            std::env::var("HTREE_BULK_BENCH_SPOOL_SOURCE")
                .expect("HTREE_BULK_BENCH_SPOOL_SOURCE must name the copied real spool"),
        );
        let index_name = std::env::var("HTREE_BULK_BENCH_INDEX")
            .expect("HTREE_BULK_BENCH_INDEX must name one real index");
        let index = NostrEventIndex::ALL
            .into_iter()
            .find(|index| index.name() == index_name)
            .expect("HTREE_BULK_BENCH_INDEX is not a known index");
        let order = std::env::var("HTREE_BULK_BENCH_BTREE_ORDER")
            .unwrap_or_else(|_| "64".to_string())
            .parse::<usize>()
            .expect("HTREE_BULK_BENCH_BTREE_ORDER");
        let spool = BulkProjectionSpool::open(&source).expect("open copied real spool");
        let target = open_target();
        let started = Instant::now();
        let root = if mode == "root-buffered" {
            spool
                .build_index_root(index, Arc::clone(&target), order)
                .await
                .expect("build buffered real index root")
        } else {
            spool
                .build_index_root_unbuffered(index, Arc::clone(&target), order)
                .await
                .expect("build unbuffered real index root")
        };
        target
            .force_sync()
            .expect("sync real root benchmark target");
        eprintln!(
            "real_bulk_bench mode={mode} index={} root={} elapsed_ms={} \
             target_map_size_bytes={target_map_size_bytes} \
             target_capacity_bytes={target_capacity_bytes}",
            index.name(),
            root.as_ref()
                .map(cid_to_nhash)
                .transpose()
                .expect("encode real benchmark root")
                .unwrap_or_default(),
            started.elapsed().as_millis(),
        );
        return;
    }

    let stage =
        HashtreeStore::with_options_and_backend(&stage_dir, None, 0, false, &StorageBackend::Lmdb)
            .expect("open real stage");
    let segment = load_stage_segment(&stage_dir, author).expect("load real staged segment");
    let end = offset.saturating_add(limit).min(segment.event_cids.len());
    let cids = segment.event_cids[offset..end]
        .iter()
        .map(|cid| parse_root_text(cid))
        .collect::<Result<Vec<_>>>()
        .expect("parse real staged CIDs");
    let stage_event_store = NostrEventStore::new(stage.store_arc());
    let load_started = Instant::now();
    let blobs = stage_event_store
        .load_validated_event_blobs(cids)
        .await
        .expect("load real validated event blobs");
    let load_ms = load_started.elapsed().as_millis();

    match mode.as_str() {
        "target-copy" | "target-copy-mixed" | "target-legacy" => {
            let mut target = open_target();
            if mode == "target-copy-mixed" {
                let event_store = NostrEventStore::new(Arc::clone(&target));
                event_store
                    .store_validated_event_blobs(blobs.iter().step_by(2))
                    .await
                    .expect("pre-store alternating real event blobs");
                target.force_sync().expect("sync mixed target prefix");
                drop(event_store);
                drop(target);
                target = open_target();
            }
            let target_event_store = NostrEventStore::new(Arc::clone(&target));
            let write_started = Instant::now();
            let target_cids = if mode.starts_with("target-copy") {
                target_event_store
                    .store_validated_event_blobs(&blobs)
                    .await
                    .expect("copy real event blobs")
            } else {
                target_event_store
                    .store_event_blobs(blobs.iter().map(|blob| blob.event().clone()))
                    .await
                    .expect("re-encode real event blobs")
            };
            let write_ms = write_started.elapsed().as_millis();
            let sync_started = Instant::now();
            target.force_sync().expect("sync isolated target");
            let sync_ms = sync_started.elapsed().as_millis();
            assert_eq!(
                target_cids,
                blobs
                    .iter()
                    .map(|blob| blob.cid().clone())
                    .collect::<Vec<_>>()
            );
            drop(target_event_store);
            drop(target);

            let reopen_started = Instant::now();
            let reopened = open_target();
            let reopened_event_store = NostrEventStore::new(Arc::clone(&reopened));
            let loaded = reopened_event_store
                .load_event_blobs(target_cids)
                .await
                .expect("read back copied events after target reopen");
            assert_eq!(
                loaded,
                blobs
                    .iter()
                    .map(|blob| blob.event().clone())
                    .collect::<Vec<_>>()
            );
            let reopen_readback_ms = reopen_started.elapsed().as_millis();
            eprintln!(
                "real_bulk_bench mode={mode} author={author} offset={offset} events={} \
                 live_bytes={} load_ms={load_ms} write_ms={write_ms} sync_ms={sync_ms} \
                 reopen_readback_ms={reopen_readback_ms} \
                 target_map_size_bytes={target_map_size_bytes} \
                 target_capacity_bytes={target_capacity_bytes}",
                blobs.len(),
                segment.live_bytes_selected,
            );
        }
        "spool" | "spool-planned" | "spool-sorted" => {
            let spool = BulkProjectionSpool::open(&output_dir).expect("open isolated spool");
            let events = blobs
                .iter()
                .map(|blob| (blob.event().clone(), blob.cid().clone()))
                .collect::<Vec<_>>();
            let event_values = events
                .iter()
                .map(|(event, _)| event.clone())
                .collect::<Vec<_>>();
            let event_cids = events
                .iter()
                .map(|(_, cid)| cid.clone())
                .collect::<Vec<_>>();
            let plan_started = Instant::now();
            let plan = spool
                .plan_replay_batch(event_values, &event_cids)
                .expect("plan real event spool mutation");
            let plan_ms = plan_started.elapsed().as_millis();
            assert_eq!(plan.missing_positions.len(), events.len());
            let apply = if mode == "spool-planned" || mode == "spool-sorted" {
                let planned = plan
                    .events
                    .into_iter()
                    .zip(plan.record_proofs)
                    .zip(event_cids)
                    .map(|(((event, existing), proof), cid)| {
                        assert!(existing.is_none());
                        (event, cid, proof)
                    })
                    .collect();
                if mode == "spool-sorted" {
                    spool
                        .apply_verified_plan(planned)
                        .expect("apply sorted real event plan to isolated spool")
                } else {
                    spool
                        .apply_verified_plan_immediate(planned)
                        .expect("apply verified real event plan to isolated spool")
                }
            } else {
                let planned = plan
                    .events
                    .into_iter()
                    .zip(event_cids)
                    .map(|((event, existing), cid)| {
                        assert!(existing.is_none());
                        (event, cid)
                    })
                    .collect();
                spool
                    .apply(planned)
                    .expect("apply real events to isolated spool")
            };
            let rtxn = spool.env.read_txn().expect("read benchmark spool counts");
            let spool_events = spool.events.len(&rtxn).expect("count spool events");
            let spool_slots = spool.slots.len(&rtxn).expect("count spool slots");
            let spool_entries = spool.entries.len(&rtxn).expect("count spool entries");
            eprintln!(
                "real_bulk_bench mode={mode} author={author} offset={offset} events={} \
                 live_bytes={} load_ms={load_ms} plan_ms={plan_ms} inserted={} \
                 replaced={} skipped={} index_entries={} spool_write_ms={} \
                 spool_sync_ms={} spool_events={} spool_slots={} spool_entries={}",
                blobs.len(),
                segment.live_bytes_selected,
                apply.inserted,
                apply.replaced,
                apply.skipped,
                apply.index_entries,
                apply.spool_write_ms,
                apply.spool_sync_ms,
                spool_events,
                spool_slots,
                spool_entries,
            );
        }
        other => panic!("unsupported HTREE_BULK_BENCH_MODE {other}"),
    }
}

#[test]
fn entry_trie_streams_long_keys_in_logical_order_across_pages() {
    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let mut expected = BTreeMap::new();
    for key in [
        String::new(),
        "a".repeat(399),
        "a".repeat(400),
        format!("{}a", "a".repeat(400)),
        format!("{}b", "a".repeat(400)),
        format!("{}é", "a".repeat(399)),
        "z".repeat(1_024),
    ]
    .into_iter()
    .chain((0..4_100).map(|number| format!("page-{number:04}")))
    {
        expected.insert(key.clone(), Cid::public(sha256(key.as_bytes())));
    }
    let mut wtxn = spool.env.write_txn().unwrap();
    for (key, cid) in &expected {
        spool
            .put_entry(&mut wtxn, NostrEventIndex::ByTag, key, cid)
            .unwrap();
    }
    wtxn.commit().unwrap();

    let mut cursor = EntryTrieCursor::new(&spool, NostrEventIndex::ByTag);
    let mut actual = Vec::new();
    while let Some(entry) = cursor.next_entry().unwrap() {
        actual.push(entry);
    }
    assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
}

#[tokio::test]
async fn bulk_projection_root_matches_the_incremental_collection_writer() {
    let keys = Keys::generate();
    let long_tag = "x".repeat(1_024);
    let old = EventBuilder::new(Kind::Metadata, "old profile")
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&keys)
        .unwrap();
    let note = EventBuilder::new(Kind::TextNote, "hello")
        .tags([
            Tag::parse(["t", "Hashtree"]).unwrap(),
            Tag::parse(["r", long_tag.as_str()]).unwrap(),
        ])
        .custom_created_at(Timestamp::from_secs(15))
        .sign_with_keys(&keys)
        .unwrap();
    let new = EventBuilder::new(Kind::Metadata, "new profile")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&keys)
        .unwrap();
    let events = [&old, &note, &new]
        .into_iter()
        .map(stored_event_from_nostr_sdk_event)
        .collect::<Vec<_>>();

    let temp = tempfile::tempdir().unwrap();
    let spool = BulkProjectionSpool::open(temp.path()).unwrap();
    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::with_options(
        Arc::clone(&store),
        NostrEventStoreOptions {
            btree_order: Some(8),
            ..NostrEventStoreOptions::default()
        },
    );
    let cids = event_store.store_event_blobs(events.clone()).await.unwrap();
    spool
        .apply(events.clone().into_iter().zip(cids).collect())
        .unwrap();

    let mut roots = BTreeMap::new();
    for index in NostrEventIndex::ALL {
        roots.insert(
            index,
            spool
                .build_index_root(index, Arc::clone(&store), 8)
                .await
                .unwrap(),
        );
    }
    let encoded_roots = roots
        .iter()
        .map(|(index, root)| {
            (
                index.stable_id(),
                root.as_ref()
                    .map(cid_to_nhash)
                    .transpose()
                    .unwrap()
                    .unwrap_or_default(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    validate_built_index_roots(&spool, &event_store, Arc::clone(&store), &encoded_roots, 8)
        .await
        .unwrap();
    let bulk = event_store
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .unwrap();
    let mut incremental = None;
    for event in events {
        incremental = Some(event_store.add(incremental.as_ref(), event).await.unwrap());
    }

    assert_eq!(Some(bulk), incremental);
}
