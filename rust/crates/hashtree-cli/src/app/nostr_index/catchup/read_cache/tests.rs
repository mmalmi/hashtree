use super::*;
use hashtree_core::{sha256, MemoryStore};
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStore, NostrEventStoreOptions};
use nostr::{EventBuilder, Keys, Kind, Timestamp};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Explicit diagnostic feature: cold admission requires a real Linux filesystem
// and a dedicated resource window, outside the ordinary parallel test suite.
#[cfg(all(feature = "lmdb", feature = "archive-io-diagnostics"))]
mod append_io;

#[derive(Default)]
struct CountedStore {
    store: MemoryStore,
    reads: AtomicUsize,
    fail_reads: AtomicBool,
    fail_writes: AtomicBool,
    optimistic_calls: AtomicUsize,
    flushes: AtomicUsize,
    corrupt_read: Mutex<Option<Vec<u8>>>,
}
#[async_trait]
impl Store for CountedStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        if self.fail_writes.load(Ordering::Relaxed) {
            return Err(StoreError::Other("write guard".into()));
        }
        self.store.put(hash, data).await
    }
    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.optimistic_calls.fetch_add(1, Ordering::Relaxed);
        self.put_many(items).await
    }
    async fn flush_pending(&self) -> Result<usize, StoreError> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(7)
    }
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.fail_reads.load(Ordering::Relaxed) {
            return Err(StoreError::Other("transient read failure".into()));
        }
        if let Some(bytes) = self.corrupt_read.lock().unwrap().as_ref() {
            return Ok(Some(bytes.clone()));
        }
        self.store.get(hash).await
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.store.has(hash).await
    }
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.store.delete(hash).await
    }
}

#[tokio::test]
async fn failures_and_unverified_bytes_are_not_reused_and_writes_keep_their_guards() {
    let base = Arc::new(CountedStore::default());
    let cache = CatchupReadCache::new(base.clone(), 1024);
    let bytes = vec![1, 2, 3];
    let hash = sha256(&bytes);
    base.fail_reads.store(true, Ordering::Relaxed);
    assert!(cache.get(&hash).await.is_err());
    base.fail_reads.store(false, Ordering::Relaxed);
    *base.corrupt_read.lock().unwrap() = Some(vec![9]);
    assert_eq!(cache.get(&hash).await.unwrap(), Some(vec![9]));
    assert_eq!(
        cache.read_stats().2,
        0,
        "invalid bytes must not enter the cache"
    );
    *base.corrupt_read.lock().unwrap() = None;
    base.fail_writes.store(true, Ordering::Relaxed);
    assert!(cache
        .put_many_optimistic(vec![(hash, bytes.clone())])
        .await
        .is_err());
    assert_eq!(base.optimistic_calls.load(Ordering::Relaxed), 1);
    assert!(cache.get(&hash).await.unwrap().is_none());
    base.fail_writes.store(false, Ordering::Relaxed);
    cache
        .put_many_optimistic(vec![(hash, bytes.clone())])
        .await
        .unwrap();
    assert_eq!(cache.flush_pending().await.unwrap(), 7);
    assert_eq!(base.flushes.load(Ordering::Relaxed), 1);
    assert_eq!(cache.get(&hash).await.unwrap(), Some(bytes));
    assert_eq!(
        base.reads.load(Ordering::Relaxed),
        4,
        "errors, invalid bytes and misses each require a new read"
    );
}

#[tokio::test]
async fn many_small_blobs_cannot_grow_cache_metadata_without_bound() {
    let base = Arc::new(CountedStore::default());
    let cache = CatchupReadCache::new(base.clone(), 64 * 1024 * 1024);
    for value in 0..MAX_ENTRIES + 2 {
        let bytes = (value as u64).to_le_bytes().to_vec();
        let hash = sha256(&bytes);
        base.put(hash, bytes).await.unwrap();
        cache.get(&hash).await.unwrap();
    }
    let (_, _, bytes, entries) = cache.read_stats();
    assert_eq!(entries, MAX_ENTRIES);
    assert_eq!(bytes, MAX_ENTRIES * 8);
    assert!(!cache
        .cache
        .lock()
        .unwrap()
        .entries
        .contains(&sha256(&0u64.to_le_bytes())));
}

#[tokio::test]
async fn bounded_positive_reads_do_not_cache_absence_or_hide_deletion() {
    let base = Arc::new(CountedStore::default());
    let cache = CatchupReadCache::new(base.clone(), 8);
    let first = vec![1; 4];
    let hash = sha256(&first);
    assert!(cache.get(&hash).await.unwrap().is_none());
    base.put(hash, first.clone()).await.unwrap();
    assert_eq!(cache.get(&hash).await.unwrap(), Some(first.clone()));
    let reads = base.reads.load(Ordering::Relaxed);
    assert_eq!(cache.get(&hash).await.unwrap(), Some(first));
    assert_eq!(base.reads.load(Ordering::Relaxed), reads);
    for byte in 2..6 {
        let bytes = vec![byte; 4];
        let hash = sha256(&bytes);
        base.put(hash, bytes).await.unwrap();
        cache.get(&hash).await.unwrap();
        assert!(cache.cache.lock().unwrap().bytes <= 8);
    }
    cache.get(&hash).await.unwrap();
    cache.delete(&hash).await.unwrap();
    assert!(cache.get(&hash).await.unwrap().is_none());
    let huge = vec![7; 9];
    let hash = sha256(&huge);
    base.put(hash, huge).await.unwrap();
    cache.get(&hash).await.unwrap();
    assert!(!cache.cache.lock().unwrap().entries.contains(&hash));
}

#[tokio::test]
async fn repeated_archive_appends_reuse_reads_without_changing_roots_or_history() {
    let keys = Keys::parse(&format!("{:064x}", 17)).unwrap();
    let events = (0..256)
        .map(|i| {
            stored_event_from_nostr_sdk_event(
                &EventBuilder::new(Kind::TextNote, format!("cache fixture {i}"))
                    .custom_created_at(Timestamp::from_secs(1000 + i))
                    .sign_with_keys(&keys)
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let mut measurements = Vec::new();
    let mut roots = Vec::new();
    for budget in [0, 64 * 1024 * 1024] {
        let base = Arc::new(CountedStore::default());
        let cached = Arc::new(CatchupReadCache::new(base.clone(), budget));
        let store = NostrEventStore::with_options(
            cached.clone(),
            NostrEventStoreOptions {
                btree_order: Some(8),
                index_commit_batch_size: Some(16),
                ..Default::default()
            },
        );
        let original = store
            .build(None, events[..128].to_vec())
            .await
            .unwrap()
            .unwrap();
        base.reads.store(0, Ordering::Relaxed);
        let mut root = original.clone();
        for batch in events[128..].chunks(16) {
            root = store
                .build(Some(&root), batch.to_vec())
                .await
                .unwrap()
                .unwrap();
        }
        let reads = base.reads.load(Ordering::Relaxed);
        measurements.push(reads);
        roots.push(root.clone());
        for event in &events[..128] {
            assert!(store
                .get_by_id(Some(&original), &event.id)
                .await
                .unwrap()
                .is_some());
        }
        for event in &events {
            assert!(store
                .get_by_id(Some(&root), &event.id)
                .await
                .unwrap()
                .is_some());
        }
        assert!(cached.cache.lock().unwrap().bytes <= budget);
    }
    assert_eq!(
        roots[0], roots[1],
        "all content-addressed projections must remain identical"
    );
    eprintln!(
        "catchup-read-cache base_reads={} cached_reads={}",
        measurements[0], measurements[1]
    );
    assert!(
        measurements[1] * 3 < measurements[0] * 2,
        "must avoid at least one third of backing reads"
    );
}
