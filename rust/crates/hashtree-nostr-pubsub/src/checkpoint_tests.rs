use super::*;
use hashtree_core::MemoryStore;
use nostr_pubsub::Filter;
use nostr_sdk::{EventBuilder, Keys, Kind, Tag, Timestamp};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Checkpoint {
    root: std::sync::Mutex<Option<Cid>>,
    fail: AtomicBool,
    prepares: AtomicUsize,
    commits: AtomicUsize,
}

impl EventIndexCheckpoint for Checkpoint {
    fn prepare(&self, _: Option<&Cid>) -> Result<()> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn commit(&self, root: Option<&Cid>) -> Result<()> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(PubsubError::Storage("checkpoint unavailable".into()));
        }
        *self.root.lock().unwrap() = root.cloned();
        Ok(())
    }
}

fn cache(
    store: Arc<MemoryStore>,
    checkpoint: Arc<Checkpoint>,
) -> HashtreeNostrBoundedEventCache<MemoryStore> {
    let root = checkpoint.root.lock().unwrap().clone();
    HashtreeNostrBoundedEventCache::new(
        store,
        root,
        EventSource::local_index("heads"),
        EventRetentionPolicy::new(2, vec![Filter::new().kind(Kind::Custom(30064))]),
    )
    .with_checkpoint(checkpoint)
}

fn head(keys: &Keys, tree: &str, time: u64) -> VerifiedEvent {
    VerifiedEvent::try_from(
        EventBuilder::new(Kind::Custom(30064), "")
            .tags([Tag::identifier(tree)])
            .custom_created_at(Timestamp::from(time))
            .sign_with_keys(keys)
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn checkpoint_reopens_exact_signed_old_heads_and_coalesces_before_limit() {
    let store = Arc::new(MemoryStore::new());
    let checkpoint = Arc::new(Checkpoint::default());
    let bus = cache(store.clone(), checkpoint.clone());
    let keys = Keys::generate();
    let old = head(&keys, "a", 1);
    let other = head(&keys, "b", 2);
    let latest = head(&keys, "a", 3);
    for event in [old, other.clone(), latest.clone()] {
        assert!(
            bus.publish(event, EventSource::peer("writer"))
                .await
                .unwrap()
                .accepted
        );
    }
    drop(bus);
    let reopened = cache(store, checkpoint);
    let report = reopened
        .query(vec![Filter::new()], QueryOptions::default())
        .await
        .unwrap();
    assert_eq!(report.events.len(), 2);
    assert_eq!(report.events[0].event.as_event(), latest.as_event());
    assert_eq!(report.events[1].event.as_event(), other.as_event());
}

#[tokio::test]
async fn failed_checkpoint_keeps_previous_head_on_live_query_and_reopen() {
    let store = Arc::new(MemoryStore::new());
    let checkpoint = Arc::new(Checkpoint::default());
    let bus = cache(store.clone(), checkpoint.clone());
    let keys = Keys::generate();
    let old = head(&keys, "a", 1);
    bus.publish(old.clone(), EventSource::peer("writer"))
        .await
        .unwrap();
    checkpoint.fail.store(true, Ordering::SeqCst);
    assert!(bus
        .publish(head(&keys, "a", 2), EventSource::peer("writer"))
        .await
        .is_err());
    for reader in [&bus, &cache(store, checkpoint)] {
        let report = reader
            .query(vec![Filter::new()], QueryOptions::default())
            .await
            .unwrap();
        assert_eq!(report.events.len(), 1);
        assert_eq!(report.events[0].event.as_event(), old.as_event());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_publisher_does_not_release_the_inflight_checkpoint_lock() {
    struct PausedCheckpoint {
        entered: std::sync::mpsc::Sender<()>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }
    impl EventIndexCheckpoint for PausedCheckpoint {
        fn commit(&self, _root: Option<&Cid>) -> Result<()> {
            let _ = self.entered.send(());
            let (lock, signal) = &*self.release;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = signal.wait(ready).unwrap();
            }
            Ok(())
        }
    }
    let (entered, observed) = std::sync::mpsc::channel();
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let cache = Arc::new(
        HashtreeNostrBoundedEventCache::new(
            Arc::new(MemoryStore::new()),
            None,
            EventSource::local_index("heads"),
            EventRetentionPolicy::new(2, Vec::new()),
        )
        .with_checkpoint(Arc::new(PausedCheckpoint {
            entered,
            release: release.clone(),
        })),
    );
    let keys = Keys::generate();
    let first = head(&keys, "a", 1);
    let writer = cache.clone();
    let pending =
        tokio::spawn(async move { writer.publish(first, EventSource::peer("writer")).await });
    tokio::task::spawn_blocking(move || {
        observed
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
    })
    .await
    .unwrap();
    pending.abort();
    let _ = pending.await;
    let locked = tokio::time::timeout(std::time::Duration::from_millis(25), cache.root_cid()).await;
    // Always release the blocking worker before asserting, including red runs.
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    assert!(
        locked.is_err(),
        "cancellation exposed a partially committed cache"
    );
    cache
        .publish(head(&keys, "b", 2), EventSource::peer("writer"))
        .await
        .unwrap();
    assert_eq!(
        cache
            .query(vec![Filter::new()], QueryOptions::default())
            .await
            .unwrap()
            .events
            .len(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_reader_keeps_checkpoint_ownership_until_its_store_read_finishes() {
    use hashtree_core::{Hash, StoreError};
    struct PausedStore {
        inner: MemoryStore,
        pause: AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl Store for PausedStore {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> std::result::Result<bool, StoreError> {
            self.inner.put(hash, data).await
        }
        async fn get(&self, hash: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
            if self.pause.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.get(hash).await
        }
        async fn has(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
            self.inner.has(hash).await
        }
        async fn delete(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
            self.inner.delete(hash).await
        }
    }
    struct Owner(Arc<AtomicBool>);
    impl EventIndexCheckpoint for Owner {
        fn commit(&self, _root: Option<&Cid>) -> Result<()> {
            Ok(())
        }
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let released = Arc::new(AtomicBool::new(false));
    let store = Arc::new(PausedStore {
        inner: MemoryStore::new(),
        pause: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let cache = Arc::new(
        HashtreeNostrBoundedEventCache::new(
            store.clone(),
            None,
            EventSource::local_index("heads"),
            EventRetentionPolicy::new(2, Vec::new()),
        )
        .with_checkpoint(Arc::new(Owner(released.clone()))),
    );
    cache
        .publish(head(&Keys::generate(), "a", 1), EventSource::peer("writer"))
        .await
        .unwrap();
    store.pause.store(true, Ordering::SeqCst);
    let reader = cache.clone();
    let pending = tokio::spawn(async move {
        reader
            .query(vec![Filter::new()], QueryOptions::default())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), store.entered.notified())
        .await
        .unwrap();
    pending.abort();
    let _ = pending.await;
    drop(cache);
    let prematurely_released = released.load(Ordering::SeqCst);
    store.release.notify_one();
    assert!(
        !prematurely_released,
        "a new writer could reclaim the canceled reader's snapshot"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !released.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn repeated_signed_head_uses_existing_id_index_without_checkpoint_work() {
    let checkpoint = Arc::new(Checkpoint::default());
    let bus = cache(Arc::new(MemoryStore::new()), checkpoint.clone());
    let event = head(&Keys::generate(), "a", 1);
    bus.publish(event.clone(), EventSource::peer("writer"))
        .await
        .unwrap();
    let root = bus.root_cid().await;
    for _ in 0..20 {
        assert!(
            bus.publish(event.clone(), EventSource::peer("replay"))
                .await
                .unwrap()
                .accepted
        );
    }
    assert_eq!(bus.root_cid().await, root);
    assert_eq!(checkpoint.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(checkpoint.commits.load(Ordering::SeqCst), 1);
    assert_eq!(
        bus.query(vec![Filter::new()], QueryOptions::default())
            .await
            .unwrap()
            .events[0]
            .event
            .as_event(),
        event.as_event()
    );
}
