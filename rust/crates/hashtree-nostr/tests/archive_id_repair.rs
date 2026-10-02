use std::collections::BTreeMap;
use std::sync::Arc;

use hashtree_core::{
    nhash_encode_full, sha256, Cid, HashTree, HashTreeConfig, MemoryStore, NHashData, Store,
};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    ListEventsOptions, NostrEventIndex, NostrEventStore, NostrEventStoreOptions, StoredNostrEvent,
    VerifiedEvent, VerifiedStoredNostrEvent,
};
use nostr_sdk::{EventBuilder, Keys, Kind, Tag, Timestamp};

fn signed_note(keys: &Keys, created_at: u64) -> StoredNostrEvent {
    let event = EventBuilder::new(Kind::TextNote, format!("synthetic note {created_at}"))
        .custom_created_at(Timestamp::from_secs(created_at))
        .tags([Tag::parse(["t", "shared-fixture-tag"]).unwrap()])
        .sign_with_keys(keys)
        .unwrap();
    VerifiedEvent::try_from(event)
        .unwrap()
        .to_stored_event()
        .into_stored()
}

async fn index_roots(
    backing: &Arc<MemoryStore>,
    root: &Cid,
) -> BTreeMap<NostrEventIndex, Option<Cid>> {
    let tree = HashTree::new(HashTreeConfig::new(backing.clone()));
    let entries = tree.list_directory_required(root).await.unwrap();
    NostrEventIndex::ALL
        .into_iter()
        .map(|index| {
            let cid = entries
                .iter()
                .find(|entry| entry.name == index.name())
                .map(|entry| Cid {
                    hash: entry.hash,
                    key: entry.key,
                });
            (index, cid)
        })
        .collect()
}

async fn projections(
    backing: &Arc<MemoryStore>,
    root: &Cid,
) -> BTreeMap<NostrEventIndex, Vec<(String, Cid)>> {
    let index = BTree::new(backing.clone(), BTreeOptions { order: Some(4) });
    let mut result = BTreeMap::new();
    for (name, root) in index_roots(backing, root).await {
        result.insert(name, index.links_entries(root.as_ref()).await.unwrap());
    }
    result
}

#[tokio::test]
async fn verified_reappend_repairs_only_missing_id_and_retains_every_prior_root() {
    let keys = Keys::parse(&format!("{:064x}", 1)).unwrap();
    let historical = (1..=48)
        .map(|at| signed_note(&keys, at))
        .collect::<Vec<_>>();
    let target = historical[23].clone();
    let backing = Arc::new(MemoryStore::new());
    let store = NostrEventStore::with_options(
        backing.clone(),
        NostrEventStoreOptions {
            btree_order: Some(4),
            index_commit_batch_size: Some(7),
            ..Default::default()
        },
    );
    let original = store
        .build(None, historical.clone())
        .await
        .unwrap()
        .unwrap();
    let original_projection = projections(&backing, &original).await;
    let original_bytes = backing.get(&original.hash).await.unwrap().unwrap();
    let original_by_id = original_projection[&NostrEventIndex::ById].clone();
    let target_cid = original_by_id
        .iter()
        .find(|(id, _)| id == &target.id)
        .unwrap()
        .1
        .clone();
    let mut damaged_roots = index_roots(&backing, &original).await;
    let index = BTree::new(backing.clone(), BTreeOptions { order: Some(4) });
    let by_id_root = damaged_roots[&NostrEventIndex::ById].as_ref().unwrap();
    let tree = HashTree::new(HashTreeConfig::new(backing.clone()));
    assert!(
        tree.list_directory_required(by_id_root)
            .await
            .unwrap()
            .len()
            < historical.len()
    );
    damaged_roots.insert(
        NostrEventIndex::ById,
        index
            .update_links(Some(by_id_root), [(target.id.clone(), None)])
            .await
            .unwrap(),
    );
    let damaged = store
        .write_bulk_index_manifest(&damaged_roots)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.get_by_id(Some(&damaged), &target.id).await.unwrap(),
        None
    );
    let author_before = store
        .list_by_author(Some(&damaged), &target.pubkey, ListEventsOptions::default())
        .await
        .unwrap();
    assert!(author_before.contains(&target));
    assert_eq!(
        author_before,
        store
            .list_by_author(
                Some(&original),
                &target.pubkey,
                ListEventsOptions::default()
            )
            .await
            .unwrap()
    );
    for (name, entries) in projections(&backing, &damaged).await {
        let expected = if name == NostrEventIndex::ById {
            original_projection[&name]
                .iter()
                .filter(|(id, _)| id != &target.id)
                .cloned()
                .collect()
        } else {
            original_projection[&name].clone()
        };
        assert_eq!(
            entries, expected,
            "only the by-id projection is damaged: {name:?}"
        );
    }

    let unrelated = signed_note(&Keys::parse(&format!("{:064x}", 2)).unwrap(), 500);
    let appended = store
        .build_with_superseded_nodes(Some(&damaged), [unrelated.clone()])
        .await
        .unwrap()
        .root
        .unwrap();
    assert_eq!(
        store.get_by_id(Some(&appended), &target.id).await.unwrap(),
        None,
        "incoming-only append must not pretend it audited old author history"
    );
    let expected = store
        .build_with_superseded_nodes(Some(&original), [unrelated])
        .await
        .unwrap()
        .root
        .unwrap();
    let verified = VerifiedStoredNostrEvent::try_from(target.clone()).unwrap();
    let repair = store
        .build_with_superseded_nodes(Some(&appended), [verified.into_stored()])
        .await
        .unwrap();
    let repaired = repair.root.unwrap();
    assert_ne!(repaired, appended);
    assert_eq!(
        store.get_by_id(Some(&repaired), &target.id).await.unwrap(),
        Some(target.clone())
    );
    assert_eq!(
        projections(&backing, &repaired).await,
        projections(&backing, &expected).await,
        "reappend restores all nine logical projections without dropping unrelated entries"
    );
    assert_eq!(
        index
            .get_link(
                index_roots(&backing, &repaired).await[&NostrEventIndex::ById].as_ref(),
                &target.id
            )
            .await
            .unwrap(),
        Some(target_cid.clone())
    );
    // Optional cross-language acceptance artifact. Only this generated fixture
    // is exported, and the caller must provide a new directory under its own
    // test workspace. Normal test runs create no output files.
    if let Some(directory) = std::env::var_os("HTREE_ARCHIVE_ID_REPAIR_FIXTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir(&directory).unwrap();
        let blocks = directory.join("blocks");
        std::fs::create_dir(&blocks).unwrap();
        let hashes = backing.keys();
        assert!(hashes.len() < 5000 && backing.total_bytes() < 8 * 1024 * 1024);
        for hash in &hashes {
            let bytes = backing.get(hash).await.unwrap().unwrap();
            assert_eq!(sha256(&bytes), *hash);
            std::fs::write(blocks.join(format!("{}.bin", hex::encode(hash))), bytes).unwrap();
        }
        let root_text = |cid: &Cid| {
            nhash_encode_full(&NHashData {
                hash: cid.hash,
                decrypt_key: cid.key,
            })
            .unwrap()
        };
        let canaries = std::iter::once(target.clone())
            .chain(historical.iter().take(15).cloned())
            .collect::<Vec<_>>();
        let metadata = serde_json::json!({
            "format": "hashtree/archive-id-repair-fixture@1",
            "originalRoot": root_text(&original), "previousRoot": root_text(&damaged),
            "beforeRoot": root_text(&appended), "repairedRoot": root_text(&repaired),
            "event": target, "eventCid": root_text(&target_cid),
            "canaryEventIds": canaries.iter().map(|event| &event.id).collect::<Vec<_>>(),
            "canaryEvents": canaries,
            "authors": [keys.public_key().to_hex(), Keys::parse(&format!("{:064x}", 2)).unwrap().public_key().to_hex()],
            "blockCount": hashes.len(), "blockBytes": backing.total_bytes(),
        });
        std::fs::write(
            directory.join("fixture.json"),
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .unwrap();
    }
    assert_eq!(
        store
            .build_with_superseded_nodes(Some(&repaired), [target.clone()])
            .await
            .unwrap()
            .root,
        Some(repaired)
    );
    assert_eq!(
        backing.get(&original.hash).await.unwrap(),
        Some(original_bytes)
    );
    for event in historical {
        assert_eq!(
            store.get_by_id(Some(&original), &event.id).await.unwrap(),
            Some(event)
        );
    }
    assert_eq!(
        store.get_by_id(Some(&damaged), &target.id).await.unwrap(),
        None
    );
    assert_eq!(
        store
            .list_by_author(Some(&damaged), &target.pubkey, ListEventsOptions::default())
            .await
            .unwrap(),
        author_before
    );
    assert!(!repair.superseded_nodes.is_empty());
    for cid in repair.superseded_nodes {
        assert!(
            backing.get(&cid.hash).await.unwrap().is_some(),
            "repair must not delete superseded nodes"
        );
    }
}
