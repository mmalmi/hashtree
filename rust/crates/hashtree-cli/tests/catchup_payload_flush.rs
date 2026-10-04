#![cfg(feature = "lmdb")]
//! Real guarded backing-store checks; deliberately no read cache or network.
use async_trait::async_trait;
use hashtree_core::{Cid, Hash, Store, StoreError};
use hashtree_lmdb::{PhysicalSpaceGuard, PoolMemberConfig, PoolStore, PoolStoreConfig};
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStore, StoredNostrEvent};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::{EventBuilder, Keys, Kind, Timestamp, SECP256K1};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

struct Observed {
    pool: Arc<PoolStore>,
    writes: AtomicUsize,
    fail_partial: AtomicBool,
}

#[async_trait]
impl Store for Observed {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.pool.put(hash, data).await
    }
    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_partial.swap(false, Ordering::SeqCst) {
            self.pool
                .put_many_optimistic_sync(&items[..items.len().min(7)])?;
            return Err(StoreError::Other(
                "injected failure after partial Pool write".into(),
            ));
        }
        self.pool.put_many_optimistic_sync(&items)
    }
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.pool.get_sync(hash)
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.pool.has(hash).await
    }
    async fn delete(&self, _: &Hash) -> Result<bool, StoreError> {
        panic!("append must preserve history")
    }
}

fn config(guard: Option<PhysicalSpaceGuard>) -> PoolStoreConfig {
    let mut config = PoolStoreConfig {
        catalog_map_size_bytes: 16 * 1024 * 1024,
        physical_space: guard,
        ..Default::default()
    };
    config.temperature.enabled = false;
    config
}

fn fixture() -> Vec<StoredNostrEvent> {
    let key = Keys::parse(&format!("{:064x}", 1)).unwrap();
    (0..81)
        .map(|i| {
            stored_event_from_nostr_sdk_event(
                &EventBuilder::new(Kind::TextNote, format!("bounded Pool event {i}"))
                    .custom_created_at(Timestamp::from_secs(1000 + i))
                    .build(key.public_key())
                    .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(i), &key)
                    .unwrap(),
            )
        })
        .collect()
}

fn snapshot(pool: &PoolStore) -> BTreeMap<Hash, Vec<u8>> {
    pool.list()
        .unwrap()
        .into_iter()
        .map(|hash| (hash, pool.get_sync(&hash).unwrap().unwrap()))
        .collect()
}

async fn verify(pool: Arc<PoolStore>, root: &Cid, events: &[StoredNostrEvent]) {
    let reader = NostrEventStore::new(pool);
    reader.validate_index_root(Some(root)).await.unwrap();
    for event in events {
        assert_eq!(
            reader.get_by_id(Some(root), &event.id).await.unwrap(),
            Some(event.clone())
        );
    }
}

#[tokio::test]
async fn coalesced_pool_reopen_preserves_exact_bytes_and_history() {
    let events = fixture();
    let mut expected = None;
    for threshold in [0, 8 * 1024 * 1024] {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("catalog");
        let pool = Arc::new(
            PoolStore::open(&catalog, config(Some(PhysicalSpaceGuard::new(0).unwrap()))).unwrap(),
        );
        pool.add_member(PoolMemberConfig::new(
            directory.path().join("member"),
            32 * 1024 * 1024,
        ))
        .unwrap();
        let old = NostrEventStore::new(pool.clone())
            .build(None, events[..32].to_vec())
            .await
            .unwrap()
            .unwrap();
        let before = snapshot(&pool);
        let store = Arc::new(Observed {
            pool: pool.clone(),
            writes: AtomicUsize::new(0),
            fail_partial: AtomicBool::new(false),
        });
        let start = Instant::now();
        let next = NostrEventStore::new(store.clone())
            .with_index_write_buffer_bytes(threshold)
            .build(Some(&old), events[32..].to_vec())
            .await
            .unwrap()
            .unwrap();
        pool.force_sync().unwrap();
        eprintln!(
            "payload-pool threshold={threshold} writes={} append_and_sync_us={}",
            store.writes.load(Ordering::SeqCst),
            start.elapsed().as_micros()
        );
        if threshold > 0 {
            assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        }
        drop(store);
        drop(pool);
        let reopened = Arc::new(PoolStore::open(&catalog, config(None)).unwrap());
        verify(reopened.clone(), &old, &events[..32]).await;
        verify(reopened.clone(), &next, &events).await;
        let after = snapshot(&reopened);
        for (hash, bytes) in before {
            assert_eq!(after.get(&hash), Some(&bytes));
        }
        let result = (old, next, after);
        if let Some(expected) = &expected {
            assert_eq!(&result, expected);
        } else {
            expected = Some(result);
        }
    }
}

#[tokio::test]
async fn coalesced_pool_admission_and_partial_failure_allow_reopened_retry() {
    let events = fixture();
    for partial in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("catalog");
        let pool = Arc::new(PoolStore::open(&catalog, config(None)).unwrap());
        pool.add_member(PoolMemberConfig::new(
            directory.path().join("member"),
            32 * 1024 * 1024,
        ))
        .unwrap();
        let old = NostrEventStore::new(pool.clone())
            .build(None, events[..32].to_vec())
            .await
            .unwrap()
            .unwrap();
        let before = snapshot(&pool);
        pool.force_sync().unwrap();
        drop(pool);
        let guard = PhysicalSpaceGuard::new(if partial { 0 } else { u64::MAX }).unwrap();
        let pool = Arc::new(PoolStore::open(&catalog, config(Some(guard.clone()))).unwrap());
        let store = Arc::new(Observed {
            pool: pool.clone(),
            writes: AtomicUsize::new(0),
            fail_partial: AtomicBool::new(partial),
        });
        let error = NostrEventStore::new(store.clone())
            .with_index_write_buffer_bytes(8 * 1024 * 1024)
            .build(Some(&old), events[32..].to_vec())
            .await
            .unwrap_err();
        if partial {
            assert!(error.to_string().contains("partial Pool write"));
        } else {
            assert!(guard.has_refused());
        }
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        drop(store);
        drop(pool);
        let reopened = Arc::new(PoolStore::open(&catalog, config(None)).unwrap());
        verify(reopened.clone(), &old, &events[..32]).await;
        let after_failure = snapshot(&reopened);
        for (hash, bytes) in &before {
            assert_eq!(after_failure.get(hash), Some(bytes));
        }
        if !partial {
            assert_eq!(after_failure, before);
        }
        let next = NostrEventStore::new(reopened.clone())
            .with_index_write_buffer_bytes(8 * 1024 * 1024)
            .build(Some(&old), events[32..].to_vec())
            .await
            .unwrap()
            .unwrap();
        reopened.force_sync().unwrap();
        drop(reopened);
        let final_pool = Arc::new(PoolStore::open(&catalog, config(None)).unwrap());
        verify(final_pool.clone(), &old, &events[..32]).await;
        verify(final_pool, &next, &events).await;
    }
}
