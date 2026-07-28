use crate::{map_heed_error, LmdbBlobReader};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;
use heed::types::Bytes;
use heed::{Database, RoTxn};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

const HASH_RANGE_BITS: u16 = 12;
const HASH_RANGE_COUNT: u16 = 1 << HASH_RANGE_BITS;
const DEFAULT_RANGE_CHUNK_KEYS: usize = 32 * 1024;
const SNAPSHOT_OPEN_ATTEMPTS: usize = 64;

/// Hard cap for concurrent value-free LMDB key-range cursors.
///
/// Sixteen cursors provide useful queue depth to mirrored HDDs while keeping
/// the maximum buffered key material below 16 MiB in the ordinary scanner.
pub const MAX_PARALLEL_RAW_KEY_SCAN_CONCURRENCY: usize = 16;

#[derive(Debug, Clone, Copy)]
struct RangeTask {
    range: u16,
    after: Option<Hash>,
}

struct RangePage {
    range: u16,
    hashes: Vec<Hash>,
    complete: bool,
}

struct RangeResult {
    range: u16,
    result: Result<RangePage, StoreError>,
}

struct CurrentChunk {
    hashes: Vec<Hash>,
    offset: usize,
}

/// Ordered, bounded-memory raw-key scanner backed by concurrent LMDB cursors.
///
/// The scanner divides the SHA-256 keyspace into 4096 ordered 12-bit ranges.
/// Every worker owns an independent read transaction with the same LMDB
/// transaction ID, so all cursors observe one coherent snapshot. Ranges are
/// prefetched through a bounded sliding window and are emitted only in
/// lexicographic order. Values are never requested from LMDB.
pub struct LmdbParallelRawKeyScanner {
    task_tx: Option<mpsc::SyncSender<RangeTask>>,
    result_rx: mpsc::Receiver<RangeResult>,
    workers: Vec<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    concurrency: usize,
    next_range_to_schedule: u16,
    next_range_to_emit: u16,
    initial_after: Option<Hash>,
    in_flight: usize,
    pending: BTreeMap<u16, RangePage>,
    current_chunk: Option<CurrentChunk>,
    previous_chunk_hash: Option<Hash>,
    exhausted: bool,
    snapshot_transaction_id: usize,
}

impl LmdbParallelRawKeyScanner {
    fn new(
        reader: &LmdbBlobReader,
        after: Option<Hash>,
        requested_concurrency: usize,
        range_chunk_keys: usize,
    ) -> Result<Self, StoreError> {
        if requested_concurrency == 0 {
            return Err(StoreError::Other(
                "parallel raw-key scan concurrency must be non-zero".into(),
            ));
        }
        if range_chunk_keys == 0 {
            return Err(StoreError::Other(
                "parallel raw-key scan range chunk must be non-zero".into(),
            ));
        }
        let concurrency = requested_concurrency.min(MAX_PARALLEL_RAW_KEY_SCAN_CONCURRENCY);
        let transactions = open_coherent_snapshot_transactions(reader, concurrency)?;
        let snapshot_transaction_id = transactions[0].id();
        let blobs = reader.store.blobs;
        let (task_tx, task_rx) = mpsc::sync_channel(concurrency);
        let task_rx = Arc::new(Mutex::new(task_rx));
        let (result_tx, result_rx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(concurrency);

        for (worker_index, transaction) in transactions.into_iter().enumerate() {
            let task_rx = Arc::clone(&task_rx);
            let result_tx = result_tx.clone();
            let cancelled = Arc::clone(&cancelled);
            let worker = thread::Builder::new()
                .name(format!("lmdb-key-scan-{worker_index}"))
                .spawn(move || {
                    raw_key_scan_worker(
                        blobs,
                        transaction,
                        task_rx,
                        result_tx,
                        cancelled,
                        range_chunk_keys,
                    );
                })
                .map_err(StoreError::Io)?;
            workers.push(worker);
        }
        drop(result_tx);

        let first_range = after.as_ref().map(hash_range).unwrap_or(0);
        let mut scanner = Self {
            task_tx: Some(task_tx),
            result_rx,
            workers,
            cancelled,
            concurrency,
            next_range_to_schedule: first_range,
            next_range_to_emit: first_range,
            initial_after: after,
            in_flight: 0,
            pending: BTreeMap::new(),
            current_chunk: None,
            previous_chunk_hash: after,
            exhausted: false,
            snapshot_transaction_id,
        };
        scanner.fill_window()?;
        Ok(scanner)
    }

    /// LMDB transaction ID shared by every worker cursor.
    pub fn snapshot_transaction_id(&self) -> usize {
        self.snapshot_transaction_id
    }

    /// Return the next globally ordered page of exact raw blob hashes.
    ///
    /// The final page may be shorter than `limit`; a subsequent call returns
    /// an empty vector. `limit` bounds caller-visible allocation independently
    /// of the internal range-prefetch window.
    pub fn next_page(&mut self, limit: usize) -> Result<Vec<Hash>, StoreError> {
        if limit == 0 || self.exhausted {
            return Ok(Vec::new());
        }
        let mut page = Vec::with_capacity(limit);
        while page.len() < limit {
            if self
                .current_chunk
                .as_ref()
                .is_none_or(|chunk| chunk.offset == chunk.hashes.len())
            {
                self.current_chunk = self
                    .next_ordered_chunk()?
                    .map(|hashes| CurrentChunk { hashes, offset: 0 });
                if self.current_chunk.is_none() {
                    self.exhausted = true;
                    break;
                }
            }
            let chunk = self
                .current_chunk
                .as_mut()
                .expect("parallel key scanner loaded a current chunk");
            let take = (limit - page.len()).min(chunk.hashes.len() - chunk.offset);
            page.extend_from_slice(&chunk.hashes[chunk.offset..chunk.offset + take]);
            chunk.offset += take;
        }
        Ok(page)
    }

    fn next_ordered_chunk(&mut self) -> Result<Option<Vec<Hash>>, StoreError> {
        loop {
            if self.next_range_to_emit == HASH_RANGE_COUNT {
                if self.in_flight != 0 || !self.pending.is_empty() {
                    return Err(StoreError::Other(
                        "parallel raw-key scan exhausted with outstanding ranges".into(),
                    ));
                }
                return Ok(None);
            }

            if let Some(range_page) = self.pending.remove(&self.next_range_to_emit) {
                if range_page.complete {
                    self.next_range_to_emit = self
                        .next_range_to_emit
                        .checked_add(1)
                        .expect("12-bit hash range index overflow");
                    self.fill_window()?;
                } else {
                    let after = range_page.hashes.last().copied().ok_or_else(|| {
                        StoreError::Other(
                            "parallel raw-key range continuation made no progress".into(),
                        )
                    })?;
                    self.send_task(RangeTask {
                        range: range_page.range,
                        after: Some(after),
                    })?;
                }

                if range_page.hashes.is_empty() {
                    continue;
                }
                if range_page.hashes.windows(2).any(|pair| pair[0] >= pair[1])
                    || self
                        .previous_chunk_hash
                        .zip(range_page.hashes.first().copied())
                        .is_some_and(|(previous, first)| previous >= first)
                {
                    return Err(StoreError::Other(
                        "parallel raw-key scan produced non-increasing hashes".into(),
                    ));
                }
                self.previous_chunk_hash = range_page.hashes.last().copied();
                return Ok(Some(range_page.hashes));
            }

            let received = self.result_rx.recv().map_err(|_| {
                StoreError::Other("parallel raw-key scan workers stopped early".into())
            })?;
            self.in_flight = self.in_flight.checked_sub(1).ok_or_else(|| {
                StoreError::Other("parallel raw-key scan in-flight count underflow".into())
            })?;
            let range_page = received.result?;
            if range_page.range != received.range
                || self.pending.insert(received.range, range_page).is_some()
            {
                return Err(StoreError::Other(
                    "parallel raw-key scan returned duplicate or mismatched range".into(),
                ));
            }
        }
    }

    fn fill_window(&mut self) -> Result<(), StoreError> {
        while self.in_flight + self.pending.len() < self.concurrency
            && self.next_range_to_schedule < HASH_RANGE_COUNT
        {
            let range = self.next_range_to_schedule;
            self.next_range_to_schedule = self
                .next_range_to_schedule
                .checked_add(1)
                .expect("12-bit hash range index overflow");
            let after = if range == self.next_range_to_emit {
                self.initial_after.take()
            } else {
                None
            };
            self.send_task(RangeTask { range, after })?;
        }
        Ok(())
    }

    fn send_task(&mut self, task: RangeTask) -> Result<(), StoreError> {
        self.task_tx
            .as_ref()
            .ok_or_else(|| StoreError::Other("parallel raw-key scanner is closed".into()))?
            .send(task)
            .map_err(|_| StoreError::Other("parallel raw-key scan workers stopped early".into()))?;
        self.in_flight = self
            .in_flight
            .checked_add(1)
            .ok_or_else(|| StoreError::Other("parallel raw-key scan count overflow".into()))?;
        Ok(())
    }
}

impl Drop for LmdbParallelRawKeyScanner {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.task_tx.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl LmdbBlobReader {
    /// Start a coherent-snapshot, value-free parallel scan after `after`.
    pub fn parallel_raw_key_scanner(
        &self,
        after: Option<Hash>,
        concurrency: usize,
    ) -> Result<LmdbParallelRawKeyScanner, StoreError> {
        LmdbParallelRawKeyScanner::new(self, after, concurrency, DEFAULT_RANGE_CHUNK_KEYS)
    }

    #[cfg(test)]
    fn parallel_raw_key_scanner_with_chunk_limit(
        &self,
        after: Option<Hash>,
        concurrency: usize,
        range_chunk_keys: usize,
    ) -> Result<LmdbParallelRawKeyScanner, StoreError> {
        LmdbParallelRawKeyScanner::new(self, after, concurrency, range_chunk_keys)
    }
}

fn open_coherent_snapshot_transactions(
    reader: &LmdbBlobReader,
    concurrency: usize,
) -> Result<Vec<RoTxn<'static>>, StoreError> {
    let env = heed::Env::clone(&reader.store.env);
    for _ in 0..SNAPSHOT_OPEN_ATTEMPTS {
        let mut transactions = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            transactions.push(env.clone().static_read_txn().map_err(map_heed_error)?);
        }
        let snapshot = transactions[0].id();
        if transactions
            .iter()
            .all(|transaction| transaction.id() == snapshot)
        {
            return Ok(transactions);
        }
        drop(transactions);
        thread::yield_now();
    }
    Err(StoreError::Other(format!(
        "could not open {concurrency} parallel raw-key readers on one LMDB snapshot"
    )))
}

fn raw_key_scan_worker(
    blobs: Database<Bytes, Bytes>,
    transaction: RoTxn<'static>,
    task_rx: Arc<Mutex<mpsc::Receiver<RangeTask>>>,
    result_tx: mpsc::Sender<RangeResult>,
    cancelled: Arc<AtomicBool>,
    range_chunk_keys: usize,
) {
    while !cancelled.load(Ordering::Acquire) {
        let task = {
            let receiver = match task_rx.lock() {
                Ok(receiver) => receiver,
                Err(_) => return,
            };
            match receiver.recv() {
                Ok(task) => task,
                Err(_) => return,
            }
        };
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let result = scan_raw_key_range(blobs, &transaction, task, range_chunk_keys).map(
            |(hashes, complete)| RangePage {
                range: task.range,
                hashes,
                complete,
            },
        );
        if result_tx
            .send(RangeResult {
                range: task.range,
                result,
            })
            .is_err()
        {
            return;
        }
    }
}

fn scan_raw_key_range(
    blobs: Database<Bytes, Bytes>,
    transaction: &RoTxn<'_>,
    task: RangeTask,
    range_chunk_keys: usize,
) -> Result<(Vec<Hash>, bool), StoreError> {
    let lower = hash_range_boundary(task.range);
    let upper = (task.range + 1 < HASH_RANGE_COUNT).then(|| hash_range_boundary(task.range + 1));
    let start = match task.after.as_ref() {
        Some(after) => {
            if hash_range(after) != task.range {
                return Err(StoreError::Other(
                    "parallel raw-key continuation left its 12-bit range".into(),
                ));
            }
            Bound::Excluded(after.as_slice())
        }
        None => Bound::Included(lower.as_slice()),
    };
    let (keys, complete) = blobs
        .raw_keys_in_range(
            transaction,
            start,
            upper.as_ref().map(|bound| bound.as_slice()),
            range_chunk_keys,
        )
        .map_err(map_heed_error)?;
    let hashes = keys
        .into_iter()
        .map(|key| {
            let hash: Hash = key
                .try_into()
                .map_err(|_| StoreError::Other("invalid hash length".into()))?;
            if hash_range(&hash) != task.range {
                return Err(StoreError::Other(
                    "parallel raw-key cursor escaped its 12-bit range".into(),
                ));
            }
            Ok(hash)
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok((hashes, complete))
}

fn hash_range(hash: &Hash) -> u16 {
    (u16::from(hash[0]) << 4) | u16::from(hash[1] >> 4)
}

fn hash_range_boundary(range: u16) -> [u8; 2] {
    debug_assert!(range < HASH_RANGE_COUNT);
    [(range >> 4) as u8, ((range & 0x0f) << 4) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LmdbBlobStore;
    use tempfile::TempDir;

    fn hash_in_range(range: u16, suffix: u64) -> Hash {
        let boundary = hash_range_boundary(range);
        let mut hash = [0u8; 32];
        hash[..2].copy_from_slice(&boundary);
        hash[24..].copy_from_slice(&suffix.to_be_bytes());
        hash
    }

    fn insert_raw(store: &LmdbBlobStore, hash: Hash, value: &[u8]) -> Result<(), StoreError> {
        let mut wtxn = store.env.write_txn().map_err(map_heed_error)?;
        store
            .blobs
            .put(&mut wtxn, &hash, value)
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)
    }

    fn collect(
        scanner: &mut LmdbParallelRawKeyScanner,
        page_size: usize,
    ) -> Result<Vec<Hash>, StoreError> {
        let mut hashes = Vec::new();
        loop {
            let page = scanner.next_page(page_size)?;
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= page_size);
            hashes.extend(page);
        }
        Ok(hashes)
    }

    #[test]
    fn parallel_raw_key_scan_is_ordered_resumable_and_value_free() -> Result<(), StoreError> {
        let temp = TempDir::new().map_err(StoreError::Io)?;
        let store = LmdbBlobStore::new(temp.path().join("source"))?;
        let mut expected = Vec::new();
        for (range, count) in [(0, 5), (1, 3), (17, 7), (2048, 4), (4095, 6)] {
            for suffix in 0..count {
                let hash = hash_in_range(range, suffix);
                // Multi-page values ensure the real LMDB database contains
                // overflow payloads. The raw scanner asks LMDB only for keys.
                insert_raw(&store, hash, &vec![range as u8; 128 * 1024])?;
                expected.push(hash);
            }
        }
        // Exercise both lower-nibble transitions explicitly: range 0 to 1,
        // and the final range 4094 to 4095.
        for (index, prefix) in [[0x00, 0x0f], [0x00, 0x10], [0xff, 0xef], [0xff, 0xf0]]
            .into_iter()
            .enumerate()
        {
            let mut hash = [0u8; 32];
            hash[..2].copy_from_slice(&prefix);
            hash[2] = 0xaa;
            hash[31] = index as u8;
            insert_raw(&store, hash, &vec![index as u8; 128 * 1024])?;
            expected.push(hash);
        }
        expected.sort_unstable();
        let reader = LmdbBlobReader {
            store,
            external_read_concurrency: 1,
        };

        let mut scanner = reader.parallel_raw_key_scanner_with_chunk_limit(None, 4, 2)?;
        assert_eq!(
            scanner.snapshot_transaction_id(),
            reader.environment_generation().last_txn_id as usize,
        );
        assert_eq!(collect(&mut scanner, 3)?, expected);
        drop(scanner);

        let resume = expected[9];
        let mut scanner = reader.parallel_raw_key_scanner_with_chunk_limit(Some(resume), 4, 2)?;
        assert_eq!(collect(&mut scanner, 4)?, expected[10..]);
        drop(scanner);

        let final_hash = *expected.last().expect("nonempty source");
        let mut scanner =
            reader.parallel_raw_key_scanner_with_chunk_limit(Some(final_hash), 4, 2)?;
        assert!(
            scanner.next_page(4)?.is_empty(),
            "resuming after the final raw key must reach exact EOF"
        );
        Ok(())
    }

    #[test]
    fn parallel_raw_key_workers_share_one_snapshot() -> Result<(), StoreError> {
        let temp = TempDir::new().map_err(StoreError::Io)?;
        let store = LmdbBlobStore::new(temp.path().join("source"))?;
        let original = hash_in_range(2, 1);
        let committed_later = hash_in_range(3000, 1);
        insert_raw(&store, original, b"original")?;
        let reader = LmdbBlobReader {
            store,
            external_read_concurrency: 1,
        };

        let mut old_snapshot = reader.parallel_raw_key_scanner_with_chunk_limit(None, 8, 2)?;
        insert_raw(&reader.store, committed_later, b"committed later")?;
        assert_eq!(collect(&mut old_snapshot, 1)?, vec![original]);
        drop(old_snapshot);

        let mut new_snapshot = reader.parallel_raw_key_scanner_with_chunk_limit(None, 8, 2)?;
        assert_eq!(
            collect(&mut new_snapshot, 1)?,
            vec![original, committed_later],
        );
        Ok(())
    }

    #[test]
    fn parallel_raw_key_scan_caps_worker_and_buffer_concurrency() -> Result<(), StoreError> {
        let temp = TempDir::new().map_err(StoreError::Io)?;
        let store = LmdbBlobStore::new(temp.path().join("source"))?;
        insert_raw(&store, hash_in_range(0, 1), b"one")?;
        let reader = LmdbBlobReader {
            store,
            external_read_concurrency: 1,
        };
        let scanner = reader.parallel_raw_key_scanner_with_chunk_limit(None, 64, 1)?;
        assert_eq!(scanner.workers.len(), MAX_PARALLEL_RAW_KEY_SCAN_CONCURRENCY);
        assert!(scanner.in_flight + scanner.pending.len() <= MAX_PARALLEL_RAW_KEY_SCAN_CONCURRENCY);
        assert_eq!(
            scanner.in_flight, MAX_PARALLEL_RAW_KEY_SCAN_CONCURRENCY,
            "the initial prefetch window must be bounded and fully scheduled"
        );
        let drop_started = std::time::Instant::now();
        drop(scanner);
        assert!(
            drop_started.elapsed() < std::time::Duration::from_secs(5),
            "dropping a scanner with outstanding tasks must cancel and join promptly"
        );
        Ok(())
    }

    #[test]
    #[ignore = "real ephemeral LMDB throughput benchmark"]
    fn parallel_raw_key_scan_benchmark() -> Result<(), StoreError> {
        const ENTRIES: usize = 250_000;
        const PAGE_SIZE: usize = 4_096;
        let temp = TempDir::new().map_err(StoreError::Io)?;
        let store = LmdbBlobStore::new(temp.path().join("source"))?;
        let value = vec![0x5a; 512];
        let mut wtxn = store.env.write_txn().map_err(map_heed_error)?;
        for index in 0..ENTRIES {
            // Cycle across every 12-bit range so lexicographic leaf pages are
            // allocated non-sequentially, like a long-lived hash store.
            let hash = hash_in_range((index % HASH_RANGE_COUNT as usize) as u16, index as u64);
            store
                .blobs
                .put(&mut wtxn, &hash, &value)
                .map_err(map_heed_error)?;
        }
        wtxn.commit().map_err(map_heed_error)?;
        store.force_sync()?;
        let reader = LmdbBlobReader {
            store,
            external_read_concurrency: 1,
        };

        let sequential_started = std::time::Instant::now();
        let mut sequential_count = 0usize;
        let mut cursor = None;
        loop {
            let hashes = reader.scan_hashes_after(cursor, PAGE_SIZE)?;
            if hashes.is_empty() {
                break;
            }
            sequential_count += hashes.len();
            cursor = hashes.last().copied();
        }
        let sequential_elapsed = sequential_started.elapsed();

        let parallel_started = std::time::Instant::now();
        let mut scanner = reader.parallel_raw_key_scanner(None, 16)?;
        let parallel_count = collect(&mut scanner, PAGE_SIZE)?.len();
        let parallel_elapsed = parallel_started.elapsed();
        assert_eq!(sequential_count, ENTRIES);
        assert_eq!(parallel_count, ENTRIES);
        eprintln!(
            "real ephemeral LMDB raw-key scan: entries={ENTRIES} sequential_ms={} parallel_ms={} sequential_keys_s={:.0} parallel_keys_s={:.0}",
            sequential_elapsed.as_millis(),
            parallel_elapsed.as_millis(),
            ENTRIES as f64 / sequential_elapsed.as_secs_f64(),
            ENTRIES as f64 / parallel_elapsed.as_secs_f64(),
        );
        Ok(())
    }
}
