use hashtree_core::{Cid, HashTree, HashTreeConfig, MemoryStore, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    NostrEventIndex, NostrEventStore, NostrEventStoreOptions, StoredNostrEvent, VerifiedEvent,
};
use nostr_sdk::{EventBuilder, Keys, Kind, Timestamp};
use std::collections::BTreeMap;
use std::sync::Arc;

fn note(keys: &Keys, at: u64) -> StoredNostrEvent {
    VerifiedEvent::try_from(
        EventBuilder::new(Kind::TextNote, format!("retained note {at}"))
            .custom_created_at(Timestamp::from_secs(at))
            .sign_with_keys(keys)
            .unwrap(),
    )
    .unwrap()
    .to_stored_event()
    .into_stored()
}
async fn roots(store: &Arc<MemoryStore>, root: &Cid) -> BTreeMap<NostrEventIndex, Option<Cid>> {
    let entries = HashTree::new(HashTreeConfig::new(store.clone()))
        .list_directory_required(root)
        .await
        .unwrap();
    NostrEventIndex::ALL
        .into_iter()
        .map(|name| {
            let cid = entries
                .iter()
                .find(|entry| entry.name == name.name())
                .map(|entry| Cid {
                    hash: entry.hash,
                    key: entry.key,
                });
            (name, cid)
        })
        .collect()
}
struct Fixture {
    backing: Arc<MemoryStore>,
    events: NostrEventStore<MemoryStore>,
    original: Cid,
    damaged: Cid,
    notes: Vec<StoredNostrEvent>,
    author: String,
}
impl Fixture {
    async fn new(missing: &[usize]) -> Self {
        let backing = Arc::new(MemoryStore::new());
        let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
        let notes = (1..=25).map(|at| note(&keys, at)).collect::<Vec<_>>();
        let mut seed = notes.clone();
        let other = Keys::parse(&format!("{:064x}", 2)).unwrap();
        seed.push(note(&other, 1));
        let events = NostrEventStore::with_options(
            backing.clone(),
            NostrEventStoreOptions {
                btree_order: Some(4),
                index_commit_batch_size: Some(3),
                ..Default::default()
            },
        );
        let original = events.build(None, seed).await.unwrap().unwrap();
        let mut map = roots(&backing, &original).await;
        let index = BTree::new(backing.clone(), BTreeOptions { order: Some(4) });
        map.insert(
            NostrEventIndex::ById,
            index
                .update_links(
                    map[&NostrEventIndex::ById].as_ref(),
                    missing.iter().map(|i| (notes[*i].id.clone(), None)),
                )
                .await
                .unwrap(),
        );
        let damaged = events
            .write_bulk_index_manifest(&map)
            .await
            .unwrap()
            .unwrap();
        Self {
            backing,
            events,
            original,
            damaged,
            notes,
            author: keys.public_key().to_hex(),
        }
    }
}

#[tokio::test]
async fn repairs_all_historical_note_pages_by_id_only_and_retains_original_cids() {
    let f = Fixture::new(&[0, 7, 15, 24]).await;
    let original = roots(&f.backing, &f.original).await;
    let before = roots(&f.backing, &f.damaged).await;
    let report = f
        .events
        .reconcile_author_note_ids(&f.damaged, &f.author, 3)
        .await
        .unwrap();
    assert_eq!(report.entries_scanned, 25);
    assert_eq!(report.ids_restored, 4);
    let after = roots(&f.backing, &report.root).await;
    for name in NostrEventIndex::ALL {
        if name != NostrEventIndex::ById {
            assert_eq!(before[&name], after[&name]);
        }
    }
    let index = BTree::new(f.backing.clone(), BTreeOptions { order: Some(4) });
    assert_eq!(
        index
            .links_entries(after[&NostrEventIndex::ById].as_ref())
            .await
            .unwrap(),
        index
            .links_entries(original[&NostrEventIndex::ById].as_ref())
            .await
            .unwrap()
    );
    for i in [0, 7, 15, 24] {
        assert!(f
            .events
            .get_by_id(Some(&f.damaged), &f.notes[i].id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            f.events
                .get_by_id(Some(&report.root), &f.notes[i].id)
                .await
                .unwrap(),
            Some(f.notes[i].clone())
        );
    }
    let duplicate = f
        .events
        .reconcile_author_note_ids(&report.root, &f.author, 3)
        .await
        .unwrap();
    assert_eq!(duplicate.ids_restored, 0);
    assert_eq!(duplicate.root, report.root);
}

#[tokio::test]
async fn healthy_author_and_unknown_author_preserve_exact_root() {
    let f = Fixture::new(&[]).await;
    for author in [&f.author, &format!("{:064x}", 9)] {
        let result = f
            .events
            .reconcile_author_note_ids(&f.original, author, 2)
            .await
            .unwrap();
        assert_eq!(result.root, f.original);
        assert_eq!(result.ids_restored, 0);
    }
}

#[tokio::test]
async fn conflicting_by_id_cid_is_fatal_without_replacing_either_projection() {
    let f = Fixture::new(&[]).await;
    let mut map = roots(&f.backing, &f.original).await;
    let index = BTree::new(f.backing.clone(), BTreeOptions::default());
    let wrong = index
        .get_link(map[&NostrEventIndex::ById].as_ref(), &f.notes[1].id)
        .await
        .unwrap()
        .unwrap();
    map.insert(
        NostrEventIndex::ById,
        index
            .update_links(
                map[&NostrEventIndex::ById].as_ref(),
                [(f.notes[0].id.clone(), Some(wrong.clone()))],
            )
            .await
            .unwrap(),
    );
    let root = f
        .events
        .write_bulk_index_manifest(&map)
        .await
        .unwrap()
        .unwrap();
    assert!(f
        .events
        .reconcile_author_note_ids(&root, &f.author, 2)
        .await
        .unwrap_err()
        .to_string()
        .contains("disagree"));
    assert_eq!(
        index
            .get_link(map[&NostrEventIndex::ById].as_ref(), &f.notes[0].id)
            .await
            .unwrap(),
        Some(wrong)
    );
}

#[tokio::test]
async fn missing_late_body_leaves_old_root_and_replay_recovers_after_orphan_writes() {
    let f = Fixture::new(&[0, 24]).await;
    let index = BTree::new(f.backing.clone(), BTreeOptions::default());
    let old = roots(&f.backing, &f.original).await;
    // Reverse-time order repairs index24 first, before it encounters this missing body.
    let cid = index
        .get_link(old[&NostrEventIndex::ById].as_ref(), &f.notes[0].id)
        .await
        .unwrap()
        .unwrap();
    let bytes = f.backing.get(&cid.hash).await.unwrap().unwrap();
    f.backing.delete(&cid.hash).await.unwrap();
    assert!(f
        .events
        .reconcile_author_note_ids(&f.damaged, &f.author, 1)
        .await
        .is_err());
    let damaged = roots(&f.backing, &f.damaged).await;
    assert!(index
        .get_link(damaged[&NostrEventIndex::ById].as_ref(), &f.notes[24].id)
        .await
        .unwrap()
        .is_none());
    f.backing.put(cid.hash, bytes).await.unwrap();
    let repaired = f
        .events
        .reconcile_author_note_ids(&f.damaged, &f.author, 1)
        .await
        .unwrap();
    assert_eq!(repaired.ids_restored, 2);
    assert_eq!(
        f.events
            .get_by_id(Some(&repaired.root), &f.notes[0].id)
            .await
            .unwrap(),
        Some(f.notes[0].clone())
    );
}

#[tokio::test]
async fn unavailable_by_id_or_author_node_never_becomes_an_empty_projection() {
    for name in [NostrEventIndex::ById, NostrEventIndex::ByAuthorKindTime] {
        let f = Fixture::new(&[0]).await;
        let map = roots(&f.backing, &f.damaged).await;
        let cid = map[&name].as_ref().unwrap();
        f.backing.delete(&cid.hash).await.unwrap();
        assert!(f
            .events
            .reconcile_author_note_ids(&f.damaged, &f.author, 2)
            .await
            .is_err());
    }
}

#[tokio::test]
async fn repair_composes_with_new_append_without_changing_historical_bodies() {
    let f = Fixture::new(&[0]).await;
    let repaired = f
        .events
        .reconcile_author_note_ids(&f.damaged, &f.author, 4)
        .await
        .unwrap();
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    let fresh = note(&keys, 500);
    let root = f
        .events
        .build(Some(&repaired.root), [fresh.clone()])
        .await
        .unwrap()
        .unwrap();
    for event in [&f.notes[0], &fresh] {
        assert_eq!(
            f.events
                .get_by_id(Some(&root), &event.id)
                .await
                .unwrap()
                .as_ref(),
            Some(event)
        );
    }
}

#[tokio::test]
async fn canonical_empty_indexes_are_valid_and_can_receive_the_first_note() {
    let backing = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(backing.clone()));
    let empty = tree.put_directory(Vec::new()).await.unwrap();
    let roots = NostrEventIndex::ALL
        .into_iter()
        .map(|name| (name, Some(empty.clone())))
        .collect();
    let events = NostrEventStore::new(backing.clone());
    let root = events
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .unwrap();
    events.get_canonical_manifest(&root).await.unwrap();
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    let checked = events
        .reconcile_author_note_ids(&root, &keys.public_key().to_hex(), 2)
        .await
        .unwrap();
    assert_eq!(checked.root, root);
    assert_eq!(checked.entries_scanned, 0);
    assert_eq!(checked.ids_restored, 0);
    let first = note(&keys, 50);
    let next = events
        .build(Some(&checked.root), [first.clone()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        events.get_by_id(Some(&next), &first.id).await.unwrap(),
        Some(first)
    );
    backing.delete(&empty.hash).await.unwrap();
    assert!(events
        .reconcile_author_note_ids(&root, &keys.public_key().to_hex(), 2)
        .await
        .is_err());
}

#[tokio::test]
async fn author_link_cannot_restore_a_different_signed_event() {
    let f = Fixture::new(&[0]).await;
    let mut map = roots(&f.backing, &f.damaged).await;
    let index = BTree::new(f.backing.clone(), BTreeOptions::default());
    let wrong = index
        .get_link(map[&NostrEventIndex::ById].as_ref(), &f.notes[1].id)
        .await
        .unwrap()
        .unwrap();
    let key = hashtree_nostr::nostr_event_index_entries(&f.notes[0], &wrong)
        .into_iter()
        .find(|entry| entry.index == NostrEventIndex::ByAuthorKindTime)
        .unwrap()
        .key;
    map.insert(
        NostrEventIndex::ByAuthorKindTime,
        index
            .update_links(
                map[&NostrEventIndex::ByAuthorKindTime].as_ref(),
                [(key, Some(wrong))],
            )
            .await
            .unwrap(),
    );
    let root = f
        .events
        .write_bulk_index_manifest(&map)
        .await
        .unwrap()
        .unwrap();
    assert!(f
        .events
        .reconcile_author_note_ids(&root, &f.author, 3)
        .await
        .unwrap_err()
        .to_string()
        .contains("projection key"));
    assert!(f
        .events
        .get_by_id(Some(&root), &f.notes[0].id)
        .await
        .unwrap()
        .is_none());
}
