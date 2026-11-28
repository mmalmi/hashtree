//! Production append reconciliation is bounded by retained sample plus incoming notes.
use async_trait::async_trait;
use hashtree_core::{Cid, Hash, HashTree, HashTreeConfig, MemoryStore, Store, StoreError};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    NostrEventIndex, NostrEventStore, NostrEventStoreOptions, StoredNostrEvent, VerifiedEvent,
};
use nostr_sdk::{EventBuilder, Keys, Kind, Timestamp};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Default)]
struct CountingStore {
    inner: MemoryStore,
    reads: AtomicUsize,
}
#[async_trait]
impl Store for CountingStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(hash).await
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}
fn note(keys: &Keys, at: u64, content: &str) -> StoredNostrEvent {
    VerifiedEvent::try_from(
        EventBuilder::new(Kind::TextNote, content)
            .custom_created_at(Timestamp::from_secs(at))
            .sign_with_keys(keys)
            .unwrap(),
    )
    .unwrap()
    .to_stored_event()
    .into_stored()
}
fn key(event: &StoredNostrEvent) -> String {
    format!(
        "{}:{:08x}:{:016x}:{}",
        event.pubkey,
        event.kind,
        u64::MAX - event.created_at,
        event.id
    )
}
type Roots = BTreeMap<NostrEventIndex, Option<Cid>>;
struct Fixture {
    store: Arc<CountingStore>,
    events: NostrEventStore<CountingStore>,
    index: BTree<CountingStore>,
    root: Cid,
    original: Cid,
    notes: Vec<StoredNostrEvent>,
    author: String,
}
impl Fixture {
    async fn new(count: usize, missing: &[usize]) -> Self {
        let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
        let notes = (0..count)
            .map(|i| note(&keys, i as u64 + 1, &format!("old note {i}")))
            .collect::<Vec<_>>();
        Self::from_notes(notes, missing).await
    }
    async fn from_notes(notes: Vec<StoredNostrEvent>, missing: &[usize]) -> Self {
        let store = Arc::new(CountingStore::default());
        let events = NostrEventStore::with_options(
            store.clone(),
            NostrEventStoreOptions {
                btree_order: Some(64),
                ..Default::default()
            },
        );
        let original = events.build(None, notes.clone()).await.unwrap().unwrap();
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(64) });
        let mut f = Self {
            store,
            events,
            index,
            root: original.clone(),
            original,
            author: notes[0].pubkey.clone(),
            notes,
        };
        // Note-only builds legitimately omit these optional projections. Give
        // the fixture real empty roots so its strict canonical-manifest checks
        // exercise the same ten-link contract as the published archive.
        let mut roots = f.roots(&f.original).await;
        let empty = HashTree::new(HashTreeConfig::new(f.store.clone()))
            .put_directory(Vec::new())
            .await
            .unwrap();
        for name in [
            NostrEventIndex::ByTag,
            NostrEventIndex::Replaceable,
            NostrEventIndex::ParameterizedReplaceable,
        ] {
            assert!(roots[&name].is_none());
            roots.insert(name, Some(empty.clone()));
        }
        f.original = f
            .events
            .write_bulk_index_manifest(&roots)
            .await
            .unwrap()
            .unwrap();
        f.events.get_canonical_manifest(&f.original).await.unwrap();
        f.root = f.original.clone();
        if !missing.is_empty() {
            let mut roots = f.roots(&f.original).await;
            roots.insert(
                NostrEventIndex::ById,
                f.index
                    .update_links(
                        roots[&NostrEventIndex::ById].as_ref(),
                        missing.iter().map(|i| (f.notes[*i].id.clone(), None)),
                    )
                    .await
                    .unwrap(),
            );
            f.root = f
                .events
                .write_bulk_index_manifest(&roots)
                .await
                .unwrap()
                .unwrap();
        }
        f
    }
    async fn roots(&self, root: &Cid) -> Roots {
        let entries = HashTree::new(HashTreeConfig::new(self.store.clone()))
            .list_directory_required(root)
            .await
            .unwrap();
        NostrEventIndex::ALL
            .into_iter()
            .map(|name| {
                (
                    name,
                    entries
                        .iter()
                        .find(|entry| entry.name == name.name())
                        .map(|entry| Cid {
                            hash: entry.hash,
                            key: entry.key,
                        }),
                )
            })
            .collect()
    }
    async fn id(&self, root: &Cid, event: &StoredNostrEvent) -> Option<Cid> {
        let roots = self.roots(root).await;
        self.index
            .get_link(roots[&NostrEventIndex::ById].as_ref(), &event.id)
            .await
            .unwrap()
    }
    async fn repair(&self, incoming: &[StoredNostrEvent]) -> hashtree_nostr::NostrIdReconciliation {
        self.events
            .reconcile_author_note_ids_for_append(&self.root, &self.author, incoming)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn large_author_reads_only_retention_sample_instead_of_all_history() {
    let f = Fixture::new(2049, &[]).await;
    f.store.reads.store(0, Ordering::Relaxed);
    let bounded = f.repair(&[]).await;
    let bounded_reads = f.store.reads.load(Ordering::Relaxed);
    assert_eq!(bounded.entries_scanned, 3);
    assert_eq!(bounded.ids_restored, 0);
    assert_eq!(bounded.root, f.root);
    assert!(
        bounded_reads <= 64,
        "sample made {bounded_reads} store reads"
    );
    f.store.reads.store(0, Ordering::Relaxed);
    let audit = f
        .events
        .reconcile_author_note_ids(&f.root, &f.author, 256)
        .await
        .unwrap();
    let full_reads = f.store.reads.load(Ordering::Relaxed);
    assert_eq!(audit.entries_scanned, 2049);
    assert_eq!(audit.root, f.root);
    assert!(
        full_reads > bounded_reads * 4,
        "full={full_reads}, bounded={bounded_reads}"
    );
    eprintln!("bounded-id-reconcile fixture_notes=2049 selected=3 bounded_reads={bounded_reads} full_reads={full_reads}");
}

#[tokio::test]
async fn old_top_three_are_repaired_without_a_time_cutoff_or_full_audit_claim() {
    let f = Fixture::new(9, &[0, 6, 7, 8]).await;
    let before = f.events.get_canonical_manifest(&f.root).await.unwrap();
    let result = f.repair(&[]).await;
    assert_eq!(result.entries_scanned, 3);
    assert_eq!(result.ids_restored, 3);
    let after = f.events.get_canonical_manifest(&result.root).await.unwrap();
    assert_eq!(before.metadata_cid, after.metadata_cid);
    let old = f.roots(&f.root).await;
    let new = f.roots(&result.root).await;
    for name in NostrEventIndex::ALL {
        if name != NostrEventIndex::ById {
            assert_eq!(old[&name], new[&name]);
        }
    }
    for event in &f.notes[6..] {
        assert_eq!(
            f.id(&result.root, event).await,
            f.id(&f.original, event).await
        );
    }
    assert!(
        f.id(&result.root, &f.notes[0]).await.is_none(),
        "older unsampled gaps require a separate audit"
    );
    assert!(
        f.id(&f.root, &f.notes[8]).await.is_none(),
        "old root must remain unchanged"
    );
}

#[tokio::test]
async fn incoming_old_projection_preserves_original_cid_before_duplicate_append() {
    let mut f = Fixture::new(12, &[0]).await;
    // Store the same signed event with a different valid MessagePack integer
    // encoding. Re-encoding the relay event would silently change its body CID.
    let mut bytes = f.events.encode_event(&f.notes[0]).unwrap();
    assert_eq!(bytes[0], 0x98);
    assert_eq!(bytes[1], 1);
    bytes.splice(1..2, [0xcc, 1]);
    assert_eq!(f.events.decode_event(&bytes).unwrap(), f.notes[0]);
    let (original_cid, _) = HashTree::new(HashTreeConfig::new(f.store.clone()))
        .put_file(&bytes)
        .await
        .unwrap();
    let mut roots = f.roots(&f.root).await;
    roots.insert(
        NostrEventIndex::ByAuthorKindTime,
        f.index
            .update_links(
                roots[&NostrEventIndex::ByAuthorKindTime].as_ref(),
                [(key(&f.notes[0]), Some(original_cid.clone()))],
            )
            .await
            .unwrap(),
    );
    f.root = f
        .events
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .unwrap();
    let before = f.events.get_canonical_manifest(&f.root).await.unwrap();
    let result = f
        .repair(&[f.notes[0].clone(), f.notes[0].clone(), f.notes[11].clone()])
        .await;
    assert_eq!(result.entries_scanned, 4);
    assert_eq!(result.ids_restored, 1);
    assert_eq!(f.id(&result.root, &f.notes[0]).await, Some(original_cid));
    assert_eq!(
        before.metadata_cid,
        f.events
            .get_canonical_manifest(&result.root)
            .await
            .unwrap()
            .metadata_cid
    );
    let appended = f
        .events
        .build_with_superseded_nodes(Some(&result.root), [f.notes[0].clone()])
        .await
        .unwrap();
    assert_eq!(appended.root, Some(result.root));
}

#[tokio::test]
async fn old_and_new_retention_union_resolves_after_append_with_timestamp_ties() {
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    let mut notes = (0..7)
        .map(|i| note(&keys, 1, &format!("tie {i}")))
        .collect::<Vec<_>>();
    notes.sort_by(|a, b| a.id.cmp(&b.id));
    let f = Fixture::from_notes(notes, &[0, 1, 2, 6]).await;
    let incoming = vec![
        note(&keys, 200_000, "fresh1"),
        note(&keys, 200_001, "fresh2"),
        note(&keys, 200_002, "fresh3"),
        f.notes[6].clone(),
    ];
    let checked = f.repair(&incoming).await;
    assert_eq!(checked.entries_scanned, 4);
    assert_eq!(checked.ids_restored, 4);
    let next = f
        .events
        .build_with_superseded_nodes(Some(&checked.root), incoming.clone())
        .await
        .unwrap()
        .root
        .unwrap();
    for event in f.notes[..3].iter().chain(incoming.iter()) {
        assert_eq!(
            f.events
                .get_by_id(Some(&next), &event.id)
                .await
                .unwrap()
                .as_ref(),
            Some(event)
        );
    }
}

#[tokio::test]
async fn selected_conflict_is_fatal_and_never_overwrites_the_other_projection() {
    let mut f = Fixture::new(9, &[]).await;
    let wrong = f.id(&f.root, &f.notes[0]).await.unwrap();
    let mut roots = f.roots(&f.root).await;
    roots.insert(
        NostrEventIndex::ById,
        f.index
            .update_links(
                roots[&NostrEventIndex::ById].as_ref(),
                [(f.notes[8].id.clone(), Some(wrong.clone()))],
            )
            .await
            .unwrap(),
    );
    f.root = f
        .events
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .unwrap();
    let error = f
        .events
        .reconcile_author_note_ids_for_append(&f.root, &f.author, &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("disagree"));
    assert_eq!(f.id(&f.root, &f.notes[8]).await, Some(wrong));
}

#[tokio::test]
async fn selected_missing_body_or_required_index_node_is_fatal() {
    for index in [
        None,
        Some(NostrEventIndex::ById),
        Some(NostrEventIndex::ByAuthorKindTime),
    ] {
        let f = Fixture::new(9, &[8]).await;
        let missing = match index {
            Some(name) => f.roots(&f.root).await[&name].clone().unwrap(),
            None => f.id(&f.original, &f.notes[8]).await.unwrap(),
        };
        let bytes = f.store.get(&missing.hash).await.unwrap().unwrap();
        f.store.delete(&missing.hash).await.unwrap();
        assert!(f
            .events
            .reconcile_author_note_ids_for_append(&f.root, &f.author, &[])
            .await
            .is_err());
        f.store.put(missing.hash, bytes).await.unwrap();
        assert!(f.id(&f.root, &f.notes[8]).await.is_none());
        assert_eq!(f.repair(&[]).await.ids_restored, 1);
    }
}

#[tokio::test]
async fn selected_invalid_signed_body_or_wrong_author_key_is_fatal() {
    for wrong_author in [false, true] {
        let mut f = Fixture::new(9, &[8]).await;
        let mut corrupt = f.notes[8].clone();
        if wrong_author {
            corrupt = f.notes[0].clone();
        } else {
            corrupt.content.push_str(" invalid signature");
        }
        let bytes = f.events.encode_event(&corrupt).unwrap();
        let (cid, _) = HashTree::new(HashTreeConfig::new(f.store.clone()))
            .put_file(&bytes)
            .await
            .unwrap();
        let mut roots = f.roots(&f.root).await;
        roots.insert(
            NostrEventIndex::ByAuthorKindTime,
            f.index
                .update_links(
                    roots[&NostrEventIndex::ByAuthorKindTime].as_ref(),
                    [(key(&f.notes[8]), Some(cid))],
                )
                .await
                .unwrap(),
        );
        f.root = f
            .events
            .write_bulk_index_manifest(&roots)
            .await
            .unwrap()
            .unwrap();
        assert!(f
            .events
            .reconcile_author_note_ids_for_append(&f.root, &f.author, &[])
            .await
            .is_err());
        assert!(f.id(&f.root, &f.notes[8]).await.is_none());
    }
}

#[tokio::test]
async fn small_and_unknown_authors_are_bounded_and_foreign_incoming_is_rejected() {
    for count in 1..=3 {
        let f = Fixture::new(count, &[]).await;
        assert_eq!(f.repair(&[]).await.entries_scanned, count as u64);
        let unknown = format!("{:064x}", 9);
        let result = f
            .events
            .reconcile_author_note_ids_for_append(&f.root, &unknown, &[])
            .await
            .unwrap();
        assert_eq!(result.entries_scanned, 0);
        assert_eq!(result.root, f.root);
        assert!(f
            .events
            .reconcile_author_note_ids_for_append(&f.root, &unknown, &f.notes)
            .await
            .is_err());
    }
}

#[tokio::test]
async fn incoming_pages_repair_every_encountered_note_and_late_failure_keeps_old_root() {
    let f = Fixture::new(600, &[0, 255, 256, 299, 400]).await;
    let incoming = f.notes[..300].to_vec();
    let late = f.id(&f.original, &f.notes[0]).await.unwrap();
    let bytes = f.store.get(&late.hash).await.unwrap().unwrap();
    f.store.delete(&late.hash).await.unwrap();
    assert!(f
        .events
        .reconcile_author_note_ids_for_append(&f.root, &f.author, &incoming)
        .await
        .is_err());
    for index in [0, 255, 256, 299, 400] {
        assert!(f.id(&f.root, &f.notes[index]).await.is_none());
    }
    f.store.put(late.hash, bytes).await.unwrap();
    let result = f.repair(&incoming).await;
    assert_eq!(result.entries_scanned, 303);
    assert_eq!(result.ids_restored, 4);
    for index in [0, 255, 256, 299] {
        assert_eq!(
            f.id(&result.root, &f.notes[index]).await,
            f.id(&f.original, &f.notes[index]).await
        );
    }
    assert!(f.id(&result.root, &f.notes[400]).await.is_none());
}
