use super::*;
use hashtree_core::MemoryStore;
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStoreOptions};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

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
