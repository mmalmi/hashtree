use std::sync::Arc;

use futures::executor::block_on;
use hashtree_core::{
    sha256, Cid, DirEntry, HashTree, HashTreeConfig, MemoryStore, Store, TreeVisibility,
};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    decode_signed_event_json, decode_stored_event_msgpack, encode_signed_event_json,
    encode_stored_event_msgpack, parse_hashtree_root_event, read_signed_event_snapshot,
    store_signed_event_snapshot, ListEventsOptions, NostrEventStore, StoredNostrEvent,
};

fn event(
    id: &str,
    pubkey: &str,
    created_at: u64,
    kind: u32,
    content: &str,
    sig: &str,
) -> StoredNostrEvent {
    StoredNostrEvent {
        id: id.to_string(),
        pubkey: pubkey.to_string(),
        created_at,
        kind,
        tags: Vec::new(),
        content: content.to_string(),
        sig: sig.to_string(),
    }
}

fn canonical_event_id(
    pubkey: &str,
    created_at: u64,
    kind: u32,
    tags: &[Vec<String>],
    content: &str,
) -> String {
    let payload = serde_json::to_string(&(0u8, pubkey, created_at, kind, tags, content))
        .expect("canonical payload");
    hex::encode(sha256(payload.as_bytes()))
}

fn canonical_store_event(
    pubkey: &str,
    created_at: u64,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: &str,
) -> StoredNostrEvent {
    StoredNostrEvent {
        id: canonical_event_id(pubkey, created_at, kind, &tags, content),
        pubkey: pubkey.to_string(),
        created_at,
        kind,
        tags,
        content: content.to_string(),
        sig: "2".repeat(128),
    }
}

async fn by_id_event_cid(store: Arc<MemoryStore>, root: &Cid, event_id: &str) -> Option<Cid> {
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
    let by_id_root = tree
        .list_directory(root)
        .await
        .expect("list manifest directory")
        .into_iter()
        .find(|entry| entry.name == "by-id")
        .map(|entry| Cid {
            hash: entry.hash,
            key: entry.key,
        })?;
    let index = BTree::new(store, BTreeOptions::default());
    index
        .get_link(Some(&by_id_root), event_id)
        .await
        .expect("get by-id link")
}

async fn replaceable_event_cid(
    store: Arc<MemoryStore>,
    root: &Cid,
    pubkey: &str,
    kind: u32,
) -> Option<Cid> {
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
    let replaceable_root = tree
        .list_directory(root)
        .await
        .expect("list manifest directory")
        .into_iter()
        .find(|entry| entry.name == "replaceable")
        .map(|entry| Cid {
            hash: entry.hash,
            key: entry.key,
        })?;
    let index = BTree::new(store, BTreeOptions::default());
    index
        .get_link(Some(&replaceable_root), &format!("{pubkey}:{kind:08x}"))
        .await
        .expect("get replaceable link")
}

async fn manifest_index_root(store: Arc<MemoryStore>, root: &Cid, name: &str) -> Option<Cid> {
    let tree = HashTree::new(HashTreeConfig::new(store));
    tree.list_directory(root)
        .await
        .expect("list manifest directory")
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| Cid {
            hash: entry.hash,
            key: entry.key,
        })
}

fn reverse_timestamp(created_at: u64) -> String {
    format!("{:016x}", u64::MAX - created_at)
}

#[test]
fn stores_events_by_id_author_and_replaceable_views() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let author = "a".repeat(64);
        let other_author = "b".repeat(64);
        let event1 = event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &author,
            10,
            1,
            "older",
            &"2".repeat(128),
        );
        let event2 = event(
            "ff92321262e009d97bc0292e83a851e4a2435b2b9748f656fbdbd5c0ccd6f0b4",
            &author,
            20,
            1,
            "newer",
            &"2".repeat(128),
        );
        let profile = event(
            "74c5538f00cc767f7b40113e315e731bd80b06d5160b950c154efca10535f805",
            &author,
            30,
            0,
            "profile",
            &"3".repeat(128),
        );
        let other = event(
            "ee5e6609ca7f7beb6a0e1927740e8cb1c68cc29e407bc85b2936883757cb0884",
            &other_author,
            40,
            1,
            "other",
            &"4".repeat(128),
        );
        let hashtagged_tags = vec![
            vec!["t".to_string(), "nostr".to_string()],
            vec!["t".to_string(), "Hashtree".to_string()],
        ];
        let hashtagged = StoredNostrEvent {
            id: canonical_event_id(&author, 50, 1, &hashtagged_tags, "tagged"),
            pubkey: author.clone(),
            created_at: 50,
            kind: 1,
            tags: hashtagged_tags,
            content: "tagged".to_string(),
            sig: "5".repeat(128),
        };

        let mut root = store.add(None, event1.clone()).await.unwrap();
        root = store.add(Some(&root), event2.clone()).await.unwrap();
        root = store.add(Some(&root), profile.clone()).await.unwrap();
        root = store.add(Some(&root), other.clone()).await.unwrap();
        root = store.add(Some(&root), hashtagged.clone()).await.unwrap();

        let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&backing)));
        let names = tree
            .list_directory(&root)
            .await
            .expect("list manifest directory")
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "by-kind-time-author"));

        assert_eq!(
            store.get_by_id(Some(&root), &event2.id).await.unwrap(),
            Some(event2.clone())
        );
        assert_eq!(
            store
                .list_by_author(Some(&root), &author, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![
                hashtagged.clone(),
                profile.clone(),
                event2.clone(),
                event1.clone()
            ]
        );
        assert_eq!(
            store
                .list_by_kind(Some(&root), 1, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![
                hashtagged.clone(),
                other.clone(),
                event2.clone(),
                event1.clone()
            ]
        );
        assert_eq!(
            store
                .list_recent(
                    Some(&root),
                    ListEventsOptions {
                        limit: Some(3),
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
            vec![hashtagged.clone(), other.clone(), profile.clone()]
        );
        assert_eq!(
            store
                .list_recent(
                    Some(&root),
                    ListEventsOptions {
                        since: Some(20),
                        until: Some(40),
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
            vec![other.clone(), profile.clone(), event2.clone()]
        );
        assert_eq!(
            store
                .get_replaceable(Some(&root), &author, 0)
                .await
                .unwrap(),
            Some(profile)
        );
        assert_eq!(
            store
                .list_by_tag(
                    Some(&root),
                    "t",
                    "nostr",
                    ListEventsOptions {
                        limit: Some(10),
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
            vec![hashtagged.clone()]
        );
        assert_eq!(
            store
                .list_by_tag(
                    Some(&root),
                    "t",
                    "hashtree",
                    ListEventsOptions {
                        limit: Some(10),
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
            vec![hashtagged]
        );
    });
}

#[test]
fn upgrades_legacy_manifest_with_kind_time_author_index() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let author = "a".repeat(64);
        let other_author = "b".repeat(64);
        let event1 = event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &author,
            10,
            1,
            "older",
            &"2".repeat(128),
        );
        let event2 = event(
            "ff92321262e009d97bc0292e83a851e4a2435b2b9748f656fbdbd5c0ccd6f0b4",
            &author,
            20,
            1,
            "newer",
            &"2".repeat(128),
        );
        let other = event(
            "ee5e6609ca7f7beb6a0e1927740e8cb1c68cc29e407bc85b2936883757cb0884",
            &other_author,
            40,
            1,
            "other",
            &"4".repeat(128),
        );

        let mut root = store.add(None, event1.clone()).await.unwrap();
        root = store.add(Some(&root), event2.clone()).await.unwrap();
        root = store.add(Some(&root), other.clone()).await.unwrap();

        let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&backing)));
        let legacy_entries = tree
            .list_directory(&root)
            .await
            .expect("list manifest directory")
            .into_iter()
            .filter(|entry| entry.name != "by-kind-time-author")
            .map(|entry| {
                DirEntry::from_cid(
                    entry.name,
                    &Cid {
                        hash: entry.hash,
                        key: entry.key,
                    },
                )
                .with_size(entry.size)
                .with_link_type(entry.link_type)
            })
            .collect::<Vec<_>>();
        let legacy_root = tree
            .put_directory(legacy_entries)
            .await
            .expect("write legacy manifest directory");

        assert!(
            manifest_index_root(Arc::clone(&backing), &legacy_root, "by-kind-time-author")
                .await
                .is_none()
        );

        let upgraded_root = store
            .upgrade_manifest_indexes(Some(&legacy_root))
            .await
            .expect("upgrade manifest indexes")
            .expect("upgraded root");
        let by_kind_time_author_root =
            manifest_index_root(Arc::clone(&backing), &upgraded_root, "by-kind-time-author")
                .await
                .expect("kind-time-author root");

        let index = BTree::new(Arc::clone(&backing), BTreeOptions::default());
        let entries = index
            .prefix_links(&by_kind_time_author_root, "00000001:")
            .await
            .expect("list kind-time-author entries");
        let keys = entries.into_iter().map(|(key, _)| key).collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                format!(
                    "00000001:{}:{}:{}",
                    reverse_timestamp(other.created_at),
                    other.pubkey,
                    other.id
                ),
                format!(
                    "00000001:{}:{}:{}",
                    reverse_timestamp(event2.created_at),
                    event2.pubkey,
                    event2.id
                ),
                format!(
                    "00000001:{}:{}:{}",
                    reverse_timestamp(event1.created_at),
                    event1.pubkey,
                    event1.id
                ),
            ]
        );

        assert_eq!(
            store
                .upgrade_manifest_indexes(Some(&upgraded_root))
                .await
                .expect("re-upgrade manifest indexes"),
            Some(upgraded_root)
        );
    });
}

#[test]
fn shared_msgpack_helpers_round_trip_events() {
    let event = canonical_store_event(
        "a".repeat(64).as_str(),
        1_700_000_000,
        1,
        Vec::new(),
        "hello",
    );
    let encoded = encode_stored_event_msgpack(&event).expect("encode msgpack");
    let decoded = decode_stored_event_msgpack(&encoded).expect("decode msgpack");
    assert_eq!(decoded, event);
}

#[test]
fn lossy_kind_listing_skips_missing_event_blobs() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let nostr_store = NostrEventStore::new(Arc::clone(&store));
        let author = "a".repeat(64);
        let older = canonical_store_event(&author, 10, 1, Vec::new(), "older");
        let newer = canonical_store_event(&author, 20, 1, Vec::new(), "newer");

        let mut root = nostr_store.add(None, older.clone()).await.unwrap();
        root = nostr_store.add(Some(&root), newer.clone()).await.unwrap();

        let missing_cid = by_id_event_cid(Arc::clone(&store), &root, &newer.id)
            .await
            .expect("event cid");
        assert!(store.delete(&missing_cid.hash).await.unwrap());

        assert!(nostr_store
            .list_by_kind(Some(&root), 1, ListEventsOptions::default())
            .await
            .is_err());
        assert_eq!(
            nostr_store
                .list_by_kind_lossy(Some(&root), 1, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![older.clone()]
        );
        assert_eq!(
            nostr_store
                .list_recent_lossy(Some(&root), ListEventsOptions::default())
                .await
                .unwrap(),
            vec![older]
        );
    });
}

#[test]
fn lossy_author_listing_skips_missing_event_blobs() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let nostr_store = NostrEventStore::new(Arc::clone(&store));
        let author = "a".repeat(64);
        let older = canonical_store_event(&author, 10, 0, Vec::new(), r#"{"name":"older"}"#);
        let newer = canonical_store_event(&author, 20, 1, Vec::new(), "newer");

        let mut root = nostr_store.add(None, older.clone()).await.unwrap();
        root = nostr_store.add(Some(&root), newer.clone()).await.unwrap();

        let missing_cid = by_id_event_cid(Arc::clone(&store), &root, &newer.id)
            .await
            .expect("event cid");
        assert!(store.delete(&missing_cid.hash).await.unwrap());

        assert!(nostr_store
            .list_by_author(Some(&root), &author, ListEventsOptions::default())
            .await
            .is_err());
        assert_eq!(
            nostr_store
                .list_by_author_lossy(Some(&root), &author, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![older]
        );
    });
}

#[test]
fn signed_event_json_snapshot_roundtrips_deterministically() {
    let event = event(
        "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
        &"a".repeat(64),
        10,
        30078,
        "",
        &"2".repeat(128),
    );
    let encoded = encode_signed_event_json(&event).unwrap();
    let decoded = decode_signed_event_json(&encoded).unwrap();

    assert_eq!(
        String::from_utf8(encoded).unwrap(),
        serde_json::to_string(&event).unwrap()
    );
    assert_eq!(decoded, event);
}

#[test]
fn stores_and_reads_public_signed_event_snapshots() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let event = StoredNostrEvent {
            tags: vec![
                vec!["d".to_string(), "videos/demo".to_string()],
                vec!["l".to_string(), "hashtree".to_string()],
                vec!["hash".to_string(), "3".repeat(64)],
                vec!["key".to_string(), "4".repeat(64)],
            ],
            ..event(
                "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
                &"a".repeat(64),
                10,
                30078,
                "",
                &"2".repeat(128),
            )
        };

        let snapshot = store_signed_event_snapshot(Arc::clone(&store), &event)
            .await
            .unwrap();
        let restored = read_signed_event_snapshot(store, &snapshot, None)
            .await
            .unwrap();

        assert_eq!(snapshot.key, None);
        assert_eq!(restored, event);
    });
}

#[test]
fn parses_hashtree_root_events_from_signed_snapshots() {
    let event = StoredNostrEvent {
        tags: vec![
            vec!["d".to_string(), "videos/demo".to_string()],
            vec!["l".to_string(), "hashtree".to_string()],
            vec!["hash".to_string(), "3".repeat(64)],
            vec!["encryptedKey".to_string(), "6".repeat(64)],
            vec!["keyId".to_string(), "7".repeat(64)],
        ],
        ..event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &"a".repeat(64),
            10,
            30078,
            "",
            &"2".repeat(128),
        )
    };

    let parsed = parse_hashtree_root_event(&event).unwrap().unwrap();

    assert_eq!(parsed.tree_name, "videos/demo");
    assert_eq!(parsed.visibility, TreeVisibility::LinkVisible);
    assert_eq!(parsed.root_cid.key, None);
    assert_eq!(parsed.labels, vec!["hashtree".to_string()]);
    assert_eq!(parsed.encrypted_key, Some("6".repeat(64)));
    assert_eq!(parsed.key_id, Some("7".repeat(64)));
}

#[test]
fn manifest_exposes_by_id_key_only() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(backing.clone()));
        let store = NostrEventStore::new(backing);
        let author = "a".repeat(64);
        let event = event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &author,
            10,
            1,
            "older",
            &"2".repeat(128),
        );

        let root = store.add(None, event).await.unwrap();
        let entries = tree.list_directory(&root).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();

        assert!(names.contains(&"by-id"));
        assert!(!names.contains(&"events_by_id"));
    });
}

#[test]
fn validates_publishable_event_index_roots() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let author = "a".repeat(64);
        let event = event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &author,
            10,
            1,
            "older",
            &"2".repeat(128),
        );

        let root = store.add(None, event).await.unwrap();
        store
            .validate_index_root(Some(&root))
            .await
            .expect("event index root should validate");
    });
}

#[test]
fn rejects_file_blobs_as_event_index_roots() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&backing)));
        let store = NostrEventStore::new(backing);
        let (blob_root, _) = tree.put_file(b"not a nostr event index").await.unwrap();

        let err = store
            .validate_index_root(Some(&blob_root))
            .await
            .expect_err("file blob must not validate as an event index root");
        assert!(
            err.to_string().contains("hash tree error")
                || err
                    .to_string()
                    .contains("missing required manifest entries"),
            "unexpected validation error: {err}"
        );
    });
}

#[test]
fn manifest_root_matches_typescript_fixture() {
    block_on(async {
        let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let author = "a".repeat(64);
        let event1 = event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &author,
            10,
            1,
            "older",
            &"2".repeat(128),
        );
        let event2 = event(
            "ff92321262e009d97bc0292e83a851e4a2435b2b9748f656fbdbd5c0ccd6f0b4",
            &author,
            20,
            1,
            "newer",
            &"2".repeat(128),
        );
        let profile = event(
            "74c5538f00cc767f7b40113e315e731bd80b06d5160b950c154efca10535f805",
            &author,
            30,
            0,
            "profile",
            &"3".repeat(128),
        );

        let mut root = store.add(None, event1).await.unwrap();
        root = store.add(Some(&root), event2).await.unwrap();
        root = store.add(Some(&root), profile).await.unwrap();
        let manifest = store.get_manifest(Some(&root)).await.unwrap();

        assert_eq!(
            cid_to_pair(&root),
            (
                "42fe879573c9d4b526c8e0b0302ccf5c7be5976da11c82a37268da9d79c7ff79".to_string(),
                Some(
                    "d71321a960031651d28135f4d1304a2e2cf8ae612ca40ab07eb5fb1c389b06b4".to_string()
                )
            )
        );

        assert_eq!(
            cid_to_pair(manifest.by_id.as_ref().unwrap()),
            (
                "cfef6382cd6e8f76eeac020241e0bf2cf06f1d4aa04f22386563f51cd6b82255".to_string(),
                Some(
                    "b6574a09ef40e5e058bdefb41da932984754a29dd41286b1edb2a0d76e949df3".to_string()
                )
            )
        );
        assert_eq!(
            cid_to_pair(manifest.by_author_time.as_ref().unwrap()),
            (
                "59c18768cfd9635b0fcd9aa4364428176eaf81b198cf01dd15d5d7fbd64f8b58".to_string(),
                Some(
                    "a9a6b38d6fc3ae3ec08ce09a5d9ffe1c1a3ee7b1019713abf691ce9635c9ef0c".to_string()
                )
            )
        );
        assert_eq!(
            cid_to_pair(manifest.by_kind_time.as_ref().unwrap()),
            (
                "66679b40e811a34aa6f769a1463b0c3d99ad902ce25765ee7f11e4e6a2c9504d".to_string(),
                Some(
                    "b6c798064906e42b709e44271942d9a489f8304ac6f6e99d49ce7f88fe11e6f7".to_string()
                )
            )
        );
        assert_eq!(
            cid_to_pair(manifest.by_time.as_ref().unwrap()),
            (
                "3a06b344cc4f726e9000f00d6ddea99f28466fc08a33a84c01def4b682fbb2f0".to_string(),
                Some(
                    "4d6e07652d9fd5d148d826e2acb06195a416efff0df27fdd0c11a52cd7ee3a34".to_string()
                )
            )
        );
    });
}

#[test]
fn add_recovers_when_existing_replaceable_blob_is_missing() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let nostr_store = NostrEventStore::new(Arc::clone(&store));
        let author = "a".repeat(64);
        let older = canonical_store_event(&author, 10, 3, Vec::new(), "older contacts");
        let newer = canonical_store_event(&author, 20, 3, Vec::new(), "newer contacts");

        let root = nostr_store.add(None, older.clone()).await.unwrap();
        let missing_cid = replaceable_event_cid(Arc::clone(&store), &root, &author, 3)
            .await
            .expect("replaceable cid");
        assert!(store.delete(&missing_cid.hash).await.unwrap());

        let next_root = nostr_store.add(Some(&root), newer.clone()).await.unwrap();
        assert_eq!(
            nostr_store
                .get_replaceable(Some(&next_root), &author, 3)
                .await
                .unwrap(),
            Some(newer)
        );
    });
}

#[test]
fn build_sorts_events_deterministically() {
    block_on(async {
        let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let author = "a".repeat(64);
        let older = event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &author,
            10,
            1,
            "older",
            &"2".repeat(128),
        );
        let newer = event(
            "ff92321262e009d97bc0292e83a851e4a2435b2b9748f656fbdbd5c0ccd6f0b4",
            &author,
            20,
            1,
            "newer",
            &"2".repeat(128),
        );
        let profile = event(
            "74c5538f00cc767f7b40113e315e731bd80b06d5160b950c154efca10535f805",
            &author,
            30,
            0,
            "profile",
            &"3".repeat(128),
        );

        let built = store
            .build(None, vec![profile.clone(), older.clone(), newer.clone()])
            .await
            .unwrap()
            .expect("root");

        let mut incremental = store.add(None, older).await.unwrap();
        incremental = store.add(Some(&incremental), newer).await.unwrap();
        incremental = store.add(Some(&incremental), profile).await.unwrap();

        assert_eq!(cid_to_pair(&built), cid_to_pair(&incremental));
    });
}

#[test]
fn stale_replaceable_events_do_not_remain_in_general_indexes() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let author = "a".repeat(64);
        let older = canonical_store_event(&author, 5, 0, Vec::new(), r#"{"name":"older"}"#);
        let newer = canonical_store_event(&author, 6, 0, Vec::new(), r#"{"name":"newer"}"#);
        let stale = canonical_store_event(&author, 4, 0, Vec::new(), r#"{"name":"stale"}"#);

        let mut root = store.add(None, older.clone()).await.unwrap();
        let older_cid = by_id_event_cid(Arc::clone(&backing), &root, &older.id)
            .await
            .expect("older event cid");
        root = store.add(Some(&root), newer.clone()).await.unwrap();
        root = store.add(Some(&root), stale.clone()).await.unwrap();

        assert_eq!(store.get_by_id(Some(&root), &older.id).await.unwrap(), None);
        assert_eq!(store.get_by_id(Some(&root), &stale.id).await.unwrap(), None);
        assert_eq!(backing.get(&older_cid.hash).await.unwrap(), None);
        assert_eq!(
            store.get_by_id(Some(&root), &newer.id).await.unwrap(),
            Some(newer.clone())
        );
        assert_eq!(
            store
                .list_by_author(Some(&root), &author, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![newer.clone()]
        );
        assert_eq!(
            store
                .list_by_kind(Some(&root), 0, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![newer.clone()]
        );
        assert_eq!(
            store
                .get_replaceable(Some(&root), &author, 0)
                .await
                .unwrap(),
            Some(newer)
        );
    });
}

#[test]
fn kind_41_is_treated_as_replaceable() {
    block_on(async {
        let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let author = "b".repeat(64);
        let older = canonical_store_event(&author, 5, 41, Vec::new(), "older channel metadata");
        let newer = canonical_store_event(&author, 6, 41, Vec::new(), "newer channel metadata");

        let mut root = store.add(None, older.clone()).await.unwrap();
        root = store.add(Some(&root), newer.clone()).await.unwrap();

        assert_eq!(store.get_by_id(Some(&root), &older.id).await.unwrap(), None);
        assert_eq!(
            store
                .list_by_kind(Some(&root), 41, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![newer.clone()]
        );
        assert_eq!(
            store
                .get_replaceable(Some(&root), &author, 41)
                .await
                .unwrap(),
            Some(newer)
        );
    });
}

#[test]
fn parameterized_replaceable_without_d_tag_uses_empty_identifier() {
    block_on(async {
        let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let author = "c".repeat(64);
        let older = canonical_store_event(&author, 5, 30_078, Vec::new(), "");
        let newer = canonical_store_event(&author, 6, 30_078, Vec::new(), "");

        let mut root = store.add(None, older.clone()).await.unwrap();
        root = store.add(Some(&root), newer.clone()).await.unwrap();

        assert_eq!(store.get_by_id(Some(&root), &older.id).await.unwrap(), None);
        assert_eq!(
            store
                .list_by_kind(Some(&root), 30_078, ListEventsOptions::default())
                .await
                .unwrap(),
            vec![newer.clone()]
        );
        assert_eq!(
            store
                .get_parameterized_replaceable(Some(&root), &author, 30_078, "")
                .await
                .unwrap(),
            Some(newer)
        );
    });
}

#[test]
fn missing_parameterized_replaceable_winner_blob_does_not_block_new_winner() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let author = "d".repeat(64);
        let d_tag = "profile-search";
        let tags = vec![vec!["d".to_string(), d_tag.to_string()]];
        let older = canonical_store_event(&author, 5, 30_078, tags.clone(), "");
        let newer = canonical_store_event(&author, 6, 30_078, tags, "");

        let mut root = store.add(None, older.clone()).await.unwrap();
        let older_cid = by_id_event_cid(Arc::clone(&backing), &root, &older.id)
            .await
            .expect("older event cid");
        assert!(backing.delete(&older_cid.hash).await.unwrap());

        assert_eq!(store.get_by_id(Some(&root), &older.id).await.unwrap(), None);
        assert_eq!(
            store
                .get_parameterized_replaceable(Some(&root), &author, 30_078, d_tag)
                .await
                .unwrap(),
            None
        );

        root = store.add(Some(&root), newer.clone()).await.unwrap();

        assert_eq!(
            store.get_by_id(Some(&root), &newer.id).await.unwrap(),
            Some(newer.clone())
        );
        assert_eq!(
            store
                .get_parameterized_replaceable(Some(&root), &author, 30_078, d_tag)
                .await
                .unwrap(),
            Some(newer)
        );
    });
}

fn cid_to_pair(cid: &Cid) -> (String, Option<String>) {
    (hex::encode(cid.hash), cid.key.map(hex::encode))
}
