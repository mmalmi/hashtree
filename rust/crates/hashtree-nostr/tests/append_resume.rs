//! Resume only complete, flushed batches through the normal event/index writer.
use hashtree_core::MemoryStore;
use hashtree_nostr::{
    prepare_append_events, stored_event_from_nostr_sdk_event, NostrEventStore,
    NostrEventStoreOptions, StoredNostrEvent,
};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::{EventBuilder, Keys, Kind, Timestamp, SECP256K1};
use std::sync::Arc;

fn event(n: u64, kind: u16) -> StoredNostrEvent {
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::from(kind), format!("event-{n}"))
            .custom_created_at(Timestamp::from_secs(1000 + n))
            .build(keys.public_key())
            .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(n), &keys)
            .unwrap(),
    )
}

#[tokio::test]
async fn prepared_batches_reopen_with_exact_uninterrupted_root_and_history() {
    let store = Arc::new(MemoryStore::new());
    let options = NostrEventStoreOptions {
        index_commit_batch_size: Some(3),
        ..Default::default()
    };
    let writer = NostrEventStore::with_options(store.clone(), options.clone());
    let history = (0..12).map(|n| event(n, 1)).collect::<Vec<_>>();
    let old = writer.build(None, history.clone()).await.unwrap().unwrap();
    let mut incoming = (12..23).map(|n| event(n, 1)).collect::<Vec<_>>();
    incoming.extend([
        event(30, 0),
        event(31, 0),
        event(32, 5),
        history[0].clone(),
        incoming[0].clone(),
    ]);
    incoming.reverse();
    let expected = writer
        .build(Some(&old), incoming.clone())
        .await
        .unwrap()
        .unwrap();
    let prepared = prepare_append_events(incoming);
    assert_eq!(prepare_append_events(prepared.clone()), prepared);
    assert_eq!(prepared.iter().filter(|e| e.kind == 0).count(), 1);
    let mut resumed = old.clone();
    for batch in prepared.chunks(3) {
        // A fresh writer has no process-local resume state or cache.
        let reopened = NostrEventStore::with_options(store.clone(), options.clone());
        resumed = reopened
            .build(Some(&resumed), batch.to_vec())
            .await
            .unwrap()
            .unwrap();
        reopened.validate_index_root(Some(&resumed)).await.unwrap();
    }
    assert_eq!(resumed, expected);
    for item in history {
        assert_eq!(
            writer.get_by_id(Some(&old), &item.id).await.unwrap(),
            Some(item)
        );
    }
}
