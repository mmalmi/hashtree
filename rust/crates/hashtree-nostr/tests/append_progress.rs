//! Exercise observations through the real append/index path, including failures.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hashtree_collection::CollectionBatchPhase;
use hashtree_core::{Hash, Store, StoreError};
use hashtree_nostr::{
    stored_event_from_nostr_sdk_event, NostrAppendPhase, NostrAppendProgress, NostrEventStore,
    NostrEventStoreOptions, StoredNostrEvent,
};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp, SECP256K1};

#[derive(Default)]
struct State {
    blobs: BTreeMap<Hash, Vec<u8>>,
    reads: usize,
    flushes: usize,
    fail_read: Option<Hash>,
    fail_flush_on: Option<usize>,
}

#[derive(Default)]
struct ObservedStore(Mutex<State>);

#[async_trait]
impl Store for ObservedStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        Ok(self.0.lock().unwrap().blobs.insert(hash, data).is_none())
    }
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        if state.fail_read == Some(*hash) {
            return Err(StoreError::Other("injected read refusal".into()));
        }
        Ok(state.blobs.get(hash).cloned())
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.0.lock().unwrap().blobs.contains_key(hash))
    }
    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        {
            let mut state = self.0.lock().unwrap();
            state.flushes += 1;
            if state.fail_flush_on == Some(state.flushes) {
                return Err(StoreError::Other("injected flush refusal".into()));
            }
        }
        self.put_many(items).await
    }
    async fn delete(&self, _: &Hash) -> Result<bool, StoreError> {
        panic!("append must retain old roots");
    }
}

fn event(ordinal: u64, kind: u16) -> StoredNostrEvent {
    let key = Keys::parse(&format!("{:064x}", 1)).unwrap();
    let event = EventBuilder::new(Kind::from(kind), format!("private-content-{ordinal}"))
        .tags([Tag::parse(["t", "private-topic"]).unwrap()])
        .custom_created_at(Timestamp::from_secs(1000 + ordinal))
        .build(key.public_key())
        .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(ordinal), &key)
        .unwrap();
    stored_event_from_nostr_sdk_event(&event)
}

fn writer(store: Arc<ObservedStore>) -> NostrEventStore<ObservedStore> {
    NostrEventStore::with_options(
        store,
        NostrEventStoreOptions {
            index_commit_batch_size: Some(3),
            ..Default::default()
        },
    )
    .with_index_write_buffer_bytes(8 * 1024 * 1024)
}

#[tokio::test]
async fn append_observer_preserves_exact_roots_bytes_and_io() {
    let history = (0..20)
        .map(|n| event(n, 1))
        .chain([event(20, 0)])
        .collect::<Vec<_>>();
    let mut incoming = (21..31).map(|n| event(n, 1)).collect::<Vec<_>>();
    incoming.extend([
        event(31, 0),
        event(32, 5),
        history[0].clone(),
        incoming[0].clone(),
    ]);
    let mut reference = None;
    for observe in [false, true] {
        let store = Arc::new(ObservedStore::default());
        let writer = writer(store.clone());
        let old = writer.build(None, history.clone()).await.unwrap().unwrap();
        let trace = Arc::new(Mutex::new(Vec::<NostrAppendProgress>::new()));
        let captured = trace.clone();
        let observer = move |progress| captured.lock().unwrap().push(progress);
        let report = writer
            .build_with_superseded_nodes_and_observer(
                Some(&old),
                incoming.clone(),
                observe.then_some(&observer),
            )
            .await
            .unwrap();
        let state = store.0.lock().unwrap();
        let result = (
            report.root,
            report.superseded_nodes,
            state.blobs.clone(),
            state.reads,
            state.flushes,
        );
        if let Some(expected) = &reference {
            assert_eq!(&result, expected);
        } else {
            reference = Some(result);
        }
        let trace = trace.lock().unwrap();
        if !observe {
            assert!(trace.is_empty());
            continue;
        }
        assert!(
            !trace.is_empty(),
            "observer must expose work before an author checkpoint"
        );
        let batches = trace[0].batches;
        assert!(batches > 1);
        assert!(trace.len() <= batches * 40);
        for batch in 1..=batches {
            let rows = trace
                .iter()
                .filter(|row| row.batch == batch)
                .collect::<Vec<_>>();
            assert_eq!(rows.last().unwrap().phase, NostrAppendPhase::Complete);
            assert_eq!(rows.last().unwrap().completed_batches, batch);
            assert!(rows[..rows.len() - 1]
                .iter()
                .all(|row| row.completed_batches == batch - 1));
        }
        assert!(trace.iter().any(|row| matches!(
            row.phase,
            NostrAppendPhase::Collection(CollectionBatchPhase::UpdateIndex { index: 6, .. })
        )));
        let diagnostic = format!("{trace:?}");
        for event in history.iter().chain(&incoming) {
            assert!(!diagnostic.contains(&event.id));
            assert!(!diagnostic.contains(&event.pubkey));
            assert!(!diagnostic.contains(&event.content));
        }
        assert!(!diagnostic.contains("private-topic"));
        println!(
            "append-phase-store-evidence {}",
            serde_json::json!({
                "batches": batches, "phase_callbacks": trace.len(),
                "gets": state.reads, "flushes": state.flushes,
                "retained_blobs": state.blobs.len(),
                "retained_bytes": state.blobs.values().map(Vec::len).sum::<usize>(),
                "same_root_and_superseded_nodes_and_bytes_and_io": true,
            })
        );
    }
}

#[tokio::test]
async fn failed_read_and_flush_never_report_a_completed_batch() {
    for fail_read in [true, false] {
        let store = Arc::new(ObservedStore::default());
        let writer = writer(store.clone());
        let old = writer.build(None, [event(0, 1)]).await.unwrap().unwrap();
        let manifest = writer.get_manifest(Some(&old)).await.unwrap();
        {
            let mut state = store.0.lock().unwrap();
            state.fail_read = fail_read.then_some(manifest.by_id.unwrap().hash);
            state.fail_flush_on = (!fail_read).then_some(state.flushes + 1);
        }
        let trace = Arc::new(Mutex::new(Vec::<NostrAppendProgress>::new()));
        let captured = trace.clone();
        let observer = move |progress| captured.lock().unwrap().push(progress);
        assert!(writer
            .build_with_superseded_nodes_and_observer(Some(&old), [event(1, 1)], Some(&observer),)
            .await
            .is_err());
        let trace = trace.lock().unwrap();
        assert_eq!(
            trace.last().unwrap().phase,
            if fail_read {
                NostrAppendPhase::LookupIds
            } else {
                NostrAppendPhase::FlushCommit
            }
        );
        assert!(trace
            .iter()
            .all(|row| row.completed_batches == 0 && row.phase != NostrAppendPhase::Complete));
    }
}

#[tokio::test]
async fn failed_later_batch_retains_only_the_completed_prefix_count() {
    let store = Arc::new(ObservedStore::default());
    let writer = writer(store.clone());
    let old = writer.build(None, [event(0, 1)]).await.unwrap().unwrap();
    {
        let mut state = store.0.lock().unwrap();
        state.fail_flush_on = Some(state.flushes + 2);
    }
    let trace = Arc::new(Mutex::new(Vec::<NostrAppendProgress>::new()));
    let captured = trace.clone();
    let observer = move |progress| captured.lock().unwrap().push(progress);
    assert!(writer
        .build_with_superseded_nodes_and_observer(
            Some(&old),
            (1..=4).map(|ordinal| event(ordinal, 1)),
            Some(&observer),
        )
        .await
        .is_err());
    let trace = trace.lock().unwrap();
    let completed = trace
        .iter()
        .filter(|row| row.phase == NostrAppendPhase::Complete)
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].batch, 1);
    let last = trace.last().unwrap();
    assert_eq!(
        (last.batch, last.completed_batches, last.phase),
        (2, 1, NostrAppendPhase::FlushCommit)
    );
}
