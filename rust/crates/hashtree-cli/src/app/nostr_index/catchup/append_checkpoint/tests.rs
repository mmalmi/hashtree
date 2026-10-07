use super::super::super::{cid_to_nhash, parse_root_text};
use super::*;
use hashtree_nostr::{
    catchup::{CatchupPolicy, CatchupSourceMode},
    prepare_append_events, stored_event_from_nostr_sdk_event, NostrEventStore,
    NostrEventStoreOptions,
};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::{EventBuilder, Keys, Kind, Timestamp, SECP256K1};

fn event(n: u64) -> StoredNostrEvent {
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, format!("event-{n}"))
            .custom_created_at(Timestamp::from_secs(1000 + n))
            .build(keys.public_key())
            .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(n), &keys)
            .unwrap(),
    )
}
fn store(path: &Path) -> HashtreeStore {
    HashtreeStore::with_options_and_backend(
        path,
        None,
        1024 * 1024 * 1024,
        false,
        &hashtree_config::StorageBackend::Fs,
    )
    .unwrap()
}
fn state(root: String) -> CatchupState {
    CatchupState {
        version: 2,
        policy: CatchupPolicy {
            base_root: root.clone(),
            authors_sha256: "a".repeat(64),
            author_count: 2,
            initial_since: 1000,
            overlap_secs: 10,
            relays: vec!["ws://source.invalid".into()],
            source_mode: CatchupSourceMode::Strict,
            kinds: vec![1],
            page_size: 4,
            max_pages_per_author: 100,
            max_events_per_author: 100,
            max_bytes_per_author: 65536,
            fetch_timeout_secs: 1,
            index_commit_batch_size: 3,
        },
        root,
        pass_since: 1000,
        pass_until: 2000,
        next_author: 0,
        events_received: 0,
        coverage_head: None,
    }
}

#[tokio::test]
async fn interrupted_cursor_write_reopens_last_batch_without_advancing_author() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catchup-append.json");
    let data = dir.path().join("store");
    let storage = store(&data);
    let options = NostrEventStoreOptions {
        index_commit_batch_size: Some(3),
        ..Default::default()
    };
    let writer = NostrEventStore::with_options(storage.store_arc(), options.clone());
    let old = writer.build(None, [event(0)]).await.unwrap().unwrap();
    let before = state(cid_to_nhash(&old).unwrap());
    let events = prepare_append_events((1..9).map(event));
    let mut cursor = AppendCheckpoint::new(&before, &events, 8, before.root.clone()).unwrap();
    cursor.save(&storage, &path).unwrap();
    let partial = writer
        .build(Some(&old), events[..3].to_vec())
        .await
        .unwrap()
        .unwrap();
    cursor.root = cid_to_nhash(&partial).unwrap();
    cursor.next_event = 3;
    cursor.save(&storage, &path).unwrap();
    let saved_bytes = std::fs::read(&path).unwrap();
    let later = writer
        .build(Some(&partial), events[3..6].to_vec())
        .await
        .unwrap()
        .unwrap();
    cursor.root = cid_to_nhash(&later).unwrap();
    cursor.next_event = 6;
    let blocked = dir.path().join(".catchup-append.json.tmp");
    std::fs::create_dir(&blocked).unwrap();
    assert!(cursor.save(&storage, &path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), saved_bytes);
    std::fs::remove_dir(blocked).unwrap();
    drop(writer);
    drop(storage);

    let storage = store(&data);
    let writer = NostrEventStore::with_options(storage.store_arc(), options);
    let cursor = AppendCheckpoint::load(&path, Some(&before))
        .unwrap()
        .unwrap();
    assert_eq!(cursor.next_event, 3);
    assert!(cursor.matches(&before, &events, 8).unwrap());
    let root = parse_root_text(&cursor.root).unwrap();
    let resumed = writer
        .build(Some(&root), events[3..].to_vec())
        .await
        .unwrap()
        .unwrap();
    let expected_dir = tempfile::tempdir().unwrap();
    let expected_storage = store(expected_dir.path());
    let expected_writer = NostrEventStore::with_options(
        expected_storage.store_arc(),
        NostrEventStoreOptions {
            index_commit_batch_size: Some(3),
            ..Default::default()
        },
    );
    let expected_old = expected_writer
        .build(None, [event(0)])
        .await
        .unwrap()
        .unwrap();
    let expected = expected_writer
        .build(Some(&expected_old), events.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed, expected);
    assert_eq!(
        writer.get_by_id(Some(&old), &event(0).id).await.unwrap(),
        Some(event(0))
    );
    let mut changed = events.clone();
    changed[0].content.push('!');
    assert!(!cursor.matches(&before, &changed, 8).unwrap());
    assert!(!cursor.matches(&before, &events, 9).unwrap());
    let mut changed_policy = before.clone();
    changed_policy.policy.index_commit_batch_size = 4;
    assert!(!cursor.matches(&changed_policy, &events, 8).unwrap());
    assert!(AppendCheckpoint::load(&path, None).is_err());

    let mut done = cursor.clone();
    done.next_event = events.len();
    done.root = cid_to_nhash(&resumed).unwrap();
    done.save(&storage, &path).unwrap();
    let mut after = before.clone();
    after.next_author += 1;
    after.events_received += 8;
    after.root = done.root.clone();
    assert!(AppendCheckpoint::load(&path, Some(&after))
        .unwrap()
        .is_none());
    assert!(!path.exists());
    done.save(&storage, &path).unwrap();
    after.events_received += 1;
    assert!(AppendCheckpoint::load(&path, Some(&after)).is_err());
}

#[tokio::test]
async fn malformed_or_unaligned_cursor_is_not_used() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(&dir.path().join("store"));
    let root = NostrEventStore::new(storage.store_arc())
        .build(None, [event(0)])
        .await
        .unwrap()
        .unwrap();
    let before = state(cid_to_nhash(&root).unwrap());
    let events = prepare_append_events((1..9).map(event));
    let cursor = AppendCheckpoint::new(&before, &events, 8, before.root.clone()).unwrap();
    let path = dir.path().join("catchup-append.json");
    for offset in [1, 2, 9] {
        let mut invalid = cursor.clone();
        invalid.next_event = offset;
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(AppendCheckpoint::load(&path, Some(&before)).is_err());
    }
    std::fs::write(&path, b"{incomplete").unwrap();
    assert!(AppendCheckpoint::load(&path, Some(&before)).is_err());
}
