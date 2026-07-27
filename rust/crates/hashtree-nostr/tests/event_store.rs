use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::executor::block_on;
use hashtree_core::{
    sha256, Cid, DirEntry, Hash, HashTree, HashTreeConfig, MemoryStore, Store, StoreError,
    TreeVisibility, DEFAULT_CHUNK_SIZE,
};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    build_private_hashtree_root_event, decode_signed_event_json, decode_stored_event_msgpack,
    encode_signed_event_json, encode_stored_event_msgpack, parse_hashtree_root_event,
    parse_verified_hashtree_root_event, read_signed_event_snapshot,
    resolve_self_encrypted_root_cid, store_signed_event_snapshot,
    stored_event_from_nostr_sdk_event, ListEventsOptions, NostrEventIndex, NostrEventStore,
    NostrEventStoreOptions, StoredNostrEvent, VerifiedEvent, VerifiedStoredNostrEvent,
    HASHTREE_LEGACY_ROOT_KIND, HASHTREE_ROOT_KIND,
};
use nostr_sdk::{
    Alphabet, Event, EventBuilder, Filter, JsonUtil, Keys, Kind, SingleLetterTag, Tag, Timestamp,
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

#[derive(Default)]
struct OptimisticBatchRecordingStore {
    inner: MemoryStore,
    optimistic_batch_sizes: Mutex<Vec<usize>>,
}

#[async_trait]
impl Store for OptimisticBatchRecordingStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }

    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.optimistic_batch_sizes
            .lock()
            .expect("optimistic batch sizes lock")
            .push(items.len());
        self.inner.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get(hash).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

fn signed_rating_fact_event(
    keys: &Keys,
    subject: &str,
    scope: &str,
    rating: i64,
    created_at: u64,
) -> Event {
    let scope_index = scope.to_lowercase();
    let rating = rating.to_string();
    let created_at_tag = created_at.to_string();
    let rater = keys.public_key().to_hex();
    EventBuilder::new(Kind::from(7368_u16), "")
        .tags(vec![
            Tag::parse(["i", scope_index.as_str()]).expect("scope index tag"),
            Tag::parse(["i", subject]).expect("subject index tag"),
            Tag::parse(["type", "rating"]).expect("type fact tag"),
            Tag::parse(["schema", "1"]).expect("schema fact tag"),
            Tag::parse(["created_at", created_at_tag.as_str()]).expect("created at fact tag"),
            Tag::parse(["rater", rater.as_str()]).expect("rater fact tag"),
            Tag::parse(["subject", subject]).expect("subject fact tag"),
            Tag::parse(["scope", scope]).expect("scope fact tag"),
            Tag::parse(["rating", rating.as_str()]).expect("rating fact tag"),
            Tag::parse(["min_rating", "0"]).expect("min rating fact tag"),
            Tag::parse(["max_rating", "100"]).expect("max rating fact tag"),
        ])
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign rating fact event")
}

fn stored_event_has_tag(event: &StoredNostrEvent, parts: &[&str]) -> bool {
    let expected = parts
        .iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>();
    event.tags.iter().any(|tag| tag == &expected)
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

async fn manifest_index_entries(
    store: Arc<MemoryStore>,
    root: &Cid,
) -> std::collections::BTreeMap<String, Vec<(String, Cid)>> {
    let mut indexes = std::collections::BTreeMap::new();
    for name in [
        "by-id",
        "by-author-time",
        "by-author-kind-time",
        "by-kind-time",
        "by-kind-time-author",
        "by-time",
        "by-tag",
        "replaceable",
        "parameterized-replaceable",
    ] {
        let entries = match manifest_index_root(Arc::clone(&store), root, name).await {
            Some(index_root) => BTree::new(Arc::clone(&store), BTreeOptions::default())
                .links_entries(Some(&index_root))
                .await
                .unwrap(),
            None => Vec::new(),
        };
        indexes.insert(name.to_string(), entries);
    }
    indexes
}

async fn canonical_bulk_manifest(backing: Arc<MemoryStore>) -> (NostrEventStore<MemoryStore>, Cid) {
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&backing)));
    let index_root = tree.put_directory(Vec::new()).await.unwrap();
    let roots = NostrEventIndex::ALL
        .into_iter()
        .map(|index| (index, Some(index_root.clone())))
        .collect::<std::collections::BTreeMap<_, _>>();
    let store = NostrEventStore::new(backing);
    let root = store
        .write_bulk_index_manifest(&roots)
        .await
        .unwrap()
        .expect("canonical bulk manifest");
    (store, root)
}

async fn replace_manifest_metadata(backing: Arc<MemoryStore>, root: &Cid, metadata: &[u8]) -> Cid {
    let tree = HashTree::new(HashTreeConfig::new(backing));
    let (metadata_cid, metadata_size) = tree.put_file(metadata).await.unwrap();
    let entries = tree
        .list_directory_required(root)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| {
            if entry.name == ".collection-manifest.json" {
                DirEntry::from_cid(entry.name, &metadata_cid)
                    .with_size(metadata_size)
                    .with_link_type(hashtree_core::LinkType::File)
            } else {
                DirEntry {
                    name: entry.name,
                    hash: entry.hash,
                    size: entry.size,
                    key: entry.key,
                    link_type: entry.link_type,
                    meta: entry.meta,
                }
            }
        })
        .collect();
    tree.put_directory(entries).await.unwrap()
}

async fn rewrite_manifest_entries<F>(backing: Arc<MemoryStore>, root: &Cid, mutate: F) -> Cid
where
    F: FnOnce(&mut Vec<DirEntry>),
{
    let tree = HashTree::new(HashTreeConfig::new(backing));
    let mut entries = tree
        .list_directory_required(root)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| DirEntry {
            name: entry.name,
            hash: entry.hash,
            size: entry.size,
            key: entry.key,
            link_type: entry.link_type,
            meta: entry.meta,
        })
        .collect::<Vec<_>>();
    mutate(&mut entries);
    tree.put_directory(entries).await.unwrap()
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
fn query_events_answers_normal_filters_from_rating_fact_indexes() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let rater = Keys::generate();
        let subject = Keys::generate().public_key().to_hex();
        let older_rating = signed_rating_fact_event(&rater, &subject, "fips.peer", 80, 20);
        let newer_rating = signed_rating_fact_event(&rater, &subject, "fips.peer", 95, 40);
        let other_scope = signed_rating_fact_event(&rater, &subject, "nvpn.exit", 90, 50);
        let note = EventBuilder::text_note("not a rating")
            .custom_created_at(Timestamp::from(60))
            .sign_with_keys(&rater)
            .expect("sign note");

        let root = store
            .build(
                None,
                [
                    older_rating.clone(),
                    newer_rating.clone(),
                    other_scope,
                    note,
                ]
                .into_iter()
                .map(|event| stored_event_from_nostr_sdk_event(&event)),
            )
            .await
            .unwrap()
            .expect("rating root");

        let filter = Filter::new()
            .kind(Kind::from(7368_u16))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::I), "fips.peer")
            .limit(10);
        let events = store.query_events(Some(&root), &filter, 100).await.unwrap();

        assert_eq!(
            events.iter().map(|event| &event.id).collect::<Vec<_>>(),
            vec![&newer_rating.id.to_hex(), &older_rating.id.to_hex()]
        );
        assert!(stored_event_has_tag(&events[0], &["schema", "1"]));
        assert!(stored_event_has_tag(&events[0], &["created_at", "40"]));
        assert!(stored_event_has_tag(
            &events[0],
            &["rater", &rater.public_key().to_hex()]
        ));

        let limited = store
            .query_events(Some(&root), &filter.clone().limit(1), 100)
            .await
            .unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, newer_rating.id.to_hex());
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
        assert!(nostr_store
            .list_by_author_and_kind(Some(&root), &author, 1, ListEventsOptions::default())
            .await
            .is_err());
        assert!(nostr_store
            .list_by_author_and_kind_lossy(Some(&root), &author, 1, ListEventsOptions::default(),)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            nostr_store
                .list_by_author_and_kind_lossy(
                    Some(&root),
                    &author,
                    0,
                    ListEventsOptions::default(),
                )
                .await
                .unwrap(),
            vec![older.clone()]
        );
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
                HASHTREE_ROOT_KIND,
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
            HASHTREE_ROOT_KIND,
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
fn parses_legacy_hashtree_root_events() {
    let event = StoredNostrEvent {
        tags: vec![
            vec!["d".to_string(), "videos/demo".to_string()],
            vec!["l".to_string(), "hashtree".to_string()],
            vec!["hash".to_string(), "3".repeat(64)],
        ],
        ..event(
            "1195275911eb877e6687b4f8a3495de1e0719280e7fc1fb229a9de37b2d87bea",
            &"a".repeat(64),
            10,
            HASHTREE_LEGACY_ROOT_KIND,
            "",
            &"2".repeat(128),
        )
    };

    let parsed = parse_hashtree_root_event(&event).unwrap().unwrap();

    assert_eq!(parsed.tree_name, "videos/demo");
}

#[test]
fn builds_and_resolves_private_hashtree_root_events() {
    let owner = Keys::generate();
    let root_hash = [0x31; 32];
    let root_key = [0x42; 32];
    let root_cid = Cid::encrypted(root_hash, root_key);

    let event = build_private_hashtree_root_event(&owner, "main", &root_cid, Some(1_700_000_000))
        .expect("build private root event");
    let parsed = parse_verified_hashtree_root_event(&event)
        .expect("parse event")
        .expect("hashtree root");
    let resolved = resolve_self_encrypted_root_cid(&parsed, &owner).expect("resolve private key");

    assert_eq!(u32::from(event.kind.as_u16()), HASHTREE_ROOT_KIND);
    assert_eq!(parsed.tree_name, "main");
    assert_eq!(parsed.visibility, TreeVisibility::Private);
    assert_eq!(parsed.root_cid.key, None);
    assert_eq!(resolved, root_cid);
    assert!(event.tags.iter().any(|tag| {
        let fields = tag.as_slice();
        fields.first().is_some_and(|name| name == "ms")
            && fields.get(1).is_some_and(|value| value == "1700000000000")
    }));
    assert!(!event.as_json().contains(&hex::encode(root_key)));
}

#[test]
fn resolving_private_hashtree_root_requires_matching_owner_key() {
    let owner = Keys::generate();
    let other = Keys::generate();
    let root_cid = Cid::encrypted([0x32; 32], [0x43; 32]);

    let event = build_private_hashtree_root_event(&owner, "main", &root_cid, None)
        .expect("build private root event");
    let parsed = parse_verified_hashtree_root_event(&event)
        .expect("parse event")
        .expect("hashtree root");

    assert!(resolve_self_encrypted_root_cid(&parsed, &other).is_err());
}

#[test]
fn verified_event_types_reject_tampered_signatures() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "release root")
        .sign_with_keys(&keys)
        .expect("signed event");
    let mut tampered = event.clone();
    tampered.content = "tampered release root".to_string();

    assert!(VerifiedEvent::try_from(event.clone()).is_ok());
    assert!(VerifiedEvent::try_from(tampered.clone()).is_err());

    let stored = stored_event_from_nostr_sdk_event(&tampered);
    assert!(VerifiedStoredNostrEvent::try_from(stored).is_err());
}

#[test]
fn event_store_can_decode_verified_events_from_storage_bytes() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "stored event")
        .sign_with_keys(&keys)
        .expect("signed event");
    let stored = stored_event_from_nostr_sdk_event(&event);
    let encoded = encode_stored_event_msgpack(&stored).expect("encode event");
    let store = NostrEventStore::new(Arc::new(MemoryStore::new()));

    let verified = store
        .decode_verified_event(&encoded)
        .expect("decode verified event");

    assert_eq!(verified.as_stored(), &stored);
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
fn validates_exact_canonical_bulk_manifest_contract() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let (store, root) = canonical_bulk_manifest(backing).await;

        let canonical = store
            .get_canonical_manifest(&root)
            .await
            .expect("current bulk manifest must be canonical");

        assert!(canonical.manifest.by_id.is_some());
        assert_ne!(canonical.metadata_cid.hash, [0; 32]);
    });
}

#[test]
fn canonical_manifest_rejects_noncanonical_link_shapes_and_cardinality() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let (store, root) = canonical_bulk_manifest(Arc::clone(&backing)).await;
        let nonzero_index_size = rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
            entries
                .iter_mut()
                .find(|entry| entry.name == "by-id")
                .unwrap()
                .size = 1;
        })
        .await;
        let wrong_index_type = rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
            entries
                .iter_mut()
                .find(|entry| entry.name == "by-id")
                .unwrap()
                .link_type = hashtree_core::LinkType::File;
        })
        .await;
        let index_meta = rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
            entries
                .iter_mut()
                .find(|entry| entry.name == "by-id")
                .unwrap()
                .meta = Some(std::collections::HashMap::from([(
                "unexpected".to_string(),
                serde_json::json!(true),
            )]));
        })
        .await;
        let metadata_meta = rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
            entries
                .iter_mut()
                .find(|entry| entry.name == ".collection-manifest.json")
                .unwrap()
                .meta = Some(std::collections::HashMap::from([(
                "unexpected".to_string(),
                serde_json::json!(true),
            )]));
        })
        .await;
        let wrong_metadata_type =
            rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
                entries
                    .iter_mut()
                    .find(|entry| entry.name == ".collection-manifest.json")
                    .unwrap()
                    .link_type = hashtree_core::LinkType::Blob;
            })
            .await;
        let wrong_metadata_size =
            rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
                entries
                    .iter_mut()
                    .find(|entry| entry.name == ".collection-manifest.json")
                    .unwrap()
                    .size += 1;
            })
            .await;
        let duplicate = rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
            entries.push(
                entries
                    .iter()
                    .find(|entry| entry.name == "by-id")
                    .unwrap()
                    .clone(),
            );
        })
        .await;
        let extra = rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
            let mut extra = entries
                .iter()
                .find(|entry| entry.name == "by-id")
                .unwrap()
                .clone();
            extra.name = "unexpected-index".to_string();
            entries.push(extra);
        })
        .await;
        let missing = rewrite_manifest_entries(Arc::clone(&backing), &root, |entries| {
            entries.retain(|entry| entry.name != "by-tag");
        })
        .await;

        for (label, malformed_root) in [
            ("nonzero index size", nonzero_index_size),
            ("wrong index type", wrong_index_type),
            ("index metadata", index_meta),
            ("metadata metadata", metadata_meta),
            ("wrong metadata type", wrong_metadata_type),
            ("wrong metadata size", wrong_metadata_size),
            ("duplicate link", duplicate),
            ("extra link", extra),
            ("missing link", missing),
        ] {
            let error = store
                .get_canonical_manifest(&malformed_root)
                .await
                .expect_err(label);
            assert!(
                error.to_string().contains("canonical Nostr manifest"),
                "unexpected {label} error: {error}"
            );
        }
    });
}

#[test]
fn canonical_manifest_rejects_unknown_metadata_and_schema_fields() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let (store, root) = canonical_bulk_manifest(Arc::clone(&backing)).await;
        let malformed = [
            br#"{"version":1,"schemaVersion":1,"publishedSchema":{"itemFormat":"nostr/event@1","projectionFormat":"hashtree/nostr-event-index@1"},"unexpected":true}"#
                .as_slice(),
            br#"{"version":1,"schemaVersion":1,"publishedSchema":{"itemFormat":"nostr/event@1","projectionFormat":"hashtree/nostr-event-index@1","schemaRef":{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}"#
                .as_slice(),
            br#"{"version":1,"schemaVersion":2,"publishedSchema":{"itemFormat":"nostr/event@1","projectionFormat":"hashtree/nostr-event-index@1"}}"#
                .as_slice(),
            br#"{"version":2,"schemaVersion":1,"publishedSchema":{"itemFormat":"nostr/event@1","projectionFormat":"hashtree/nostr-event-index@1"}}"#
                .as_slice(),
            br#"{"version":1,"schemaVersion":1,"publishedSchema":{"itemFormat":"nostr/event@2","projectionFormat":"hashtree/nostr-event-index@1"}}"#
                .as_slice(),
            br#"{"version":1,"schemaVersion":1,"publishedSchema":{"itemFormat":"nostr/event@1","projectionFormat":"hashtree/nostr-event-index@2"}}"#
                .as_slice(),
        ];

        for metadata in malformed {
            let malformed_root =
                replace_manifest_metadata(Arc::clone(&backing), &root, metadata).await;
            let error = store
                .get_canonical_manifest(&malformed_root)
                .await
                .expect_err("malformed canonical metadata must fail");
            assert!(
                error.to_string().contains("metadata"),
                "unexpected strict manifest error: {error}"
            );
        }
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
fn build_deduplicates_non_replaceable_events_within_batch() {
    block_on(async {
        let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let author = "a".repeat(64);
        let seed = canonical_store_event(&author, 10, 1, Vec::new(), "seed");
        let duplicate = canonical_store_event(&author, 20, 1, Vec::new(), "duplicate");
        let root = store
            .build(None, vec![seed])
            .await
            .unwrap()
            .expect("seed root");

        let single = store
            .build(Some(&root), vec![duplicate.clone()])
            .await
            .unwrap()
            .expect("single event root");
        let duplicated = store
            .build(Some(&root), vec![duplicate.clone(), duplicate])
            .await
            .unwrap()
            .expect("deduplicated event root");

        assert_eq!(cid_to_pair(&duplicated), cid_to_pair(&single));
    });
}

#[test]
fn resumed_build_with_only_existing_and_stale_events_keeps_exact_root() {
    block_on(async {
        let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let author = "e".repeat(64);
        let plain = canonical_store_event(&author, 10, 1, Vec::new(), "plain");
        let profile = canonical_store_event(&author, 20, 0, Vec::new(), "profile");
        let stale_profile = canonical_store_event(&author, 19, 0, Vec::new(), "stale profile");
        let root = store
            .build(None, [plain.clone(), profile])
            .await
            .unwrap()
            .expect("initial root");

        let unchanged = store
            .build(Some(&root), [plain, stale_profile])
            .await
            .unwrap()
            .expect("unchanged root");

        assert_eq!(unchanged, root);
    });
}

#[test]
fn bulk_build_flushes_each_projection_within_configured_event_commits() {
    block_on(async {
        let backing = Arc::new(OptimisticBatchRecordingStore::default());
        let store = NostrEventStore::with_options(
            Arc::clone(&backing),
            NostrEventStoreOptions {
                btree_order: Some(4),
                btree_update_concurrency: None,
                index_commit_batch_size: Some(2),
            },
        );
        let events = (0..5)
            .map(|index| {
                canonical_store_event(
                    &format!("{:064x}", index + 1),
                    100 + index,
                    1,
                    Vec::new(),
                    &format!("bounded commit {index}"),
                )
            })
            .collect::<Vec<_>>();

        let root = store
            .build(None, events)
            .await
            .expect("bounded build")
            .expect("bounded root");
        assert_eq!(
            store
                .list_recent(Some(&root), ListEventsOptions::default())
                .await
                .expect("list bounded build")
                .len(),
            5
        );
        let flush_sizes = backing
            .optimistic_batch_sizes
            .lock()
            .expect("optimistic batch sizes lock")
            .clone();
        assert!(
            flush_sizes.len() > 3,
            "three event commits should flush intermediate index projections: {flush_sizes:?}"
        );
    });
}

#[test]
fn individual_event_blobs_round_trip_without_creating_an_index_root() {
    block_on(async {
        let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let first = canonical_store_event(&"1".repeat(64), 10, 1, Vec::new(), "first");
        let second = canonical_store_event(&"2".repeat(64), 20, 7, Vec::new(), "second");

        let cids = store
            .store_event_blobs([first.clone(), second.clone()])
            .await
            .expect("store individual event blobs");

        assert_eq!(cids.len(), 2);
        assert_eq!(
            store
                .load_event_blobs(cids)
                .await
                .expect("load individual event blobs"),
            vec![first, second]
        );
        assert!(store
            .list_recent(None, ListEventsOptions::default())
            .await
            .expect("list empty index")
            .is_empty());
    });
}

#[test]
fn validated_event_blobs_copy_exact_cids_between_stores() {
    block_on(async {
        let source = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let target = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let first = canonical_store_event(&"1".repeat(64), 10, 1, Vec::new(), "first");
        let second = canonical_store_event(&"2".repeat(64), 20, 7, Vec::new(), "second");
        let large_content = "z".repeat(DEFAULT_CHUNK_SIZE + 1);
        let third = canonical_store_event(&"3".repeat(64), 30, 1, Vec::new(), &large_content);
        let source_cids = source
            .store_event_blobs([first.clone(), second.clone(), third.clone()])
            .await
            .expect("store source event blobs");

        let blobs = source
            .load_validated_event_blobs(source_cids.clone())
            .await
            .expect("load validated source blobs");
        assert_eq!(
            blobs
                .iter()
                .map(|blob| blob.event().clone())
                .collect::<Vec<_>>(),
            vec![first.clone(), second.clone(), third.clone()]
        );
        assert_eq!(
            blobs
                .iter()
                .map(|blob| blob.cid().clone())
                .collect::<Vec<_>>(),
            source_cids
        );

        let copied_cids = target
            .store_validated_event_blobs(&blobs)
            .await
            .expect("copy validated event blobs");
        assert_eq!(copied_cids, source_cids);
        assert_eq!(
            target
                .load_event_blobs(copied_cids)
                .await
                .expect("load copied event blobs"),
            vec![first, second, third]
        );
    });
}

#[test]
fn bulk_event_blob_load_identifies_the_failing_cid_and_sequence() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let first = canonical_store_event(&"1".repeat(64), 10, 1, Vec::new(), "first");
        let second = canonical_store_event(&"2".repeat(64), 20, 7, Vec::new(), "second");
        let cids = store
            .store_event_blobs([first, second])
            .await
            .expect("store individual event blobs");
        assert!(backing.delete(&cids[1].hash).await.unwrap());

        let error = store
            .load_event_blobs(cids.clone())
            .await
            .expect_err("missing event blob should fail");
        let message = error.to_string();
        assert!(message.contains(&cids[1].to_string()), "{message}");
        assert!(message.contains("sequence 1"), "{message}");
    });
}

#[test]
fn optional_bulk_event_blob_load_preserves_present_and_missing_positions() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let first = canonical_store_event(&"1".repeat(64), 10, 1, Vec::new(), "first");
        let second = canonical_store_event(&"2".repeat(64), 20, 7, Vec::new(), "second");
        let cids = store
            .store_event_blobs([first.clone(), second.clone()])
            .await
            .expect("store individual event blobs");
        assert!(backing.delete(&cids[1].hash).await.unwrap());

        assert_eq!(
            store
                .try_load_event_blobs(cids)
                .await
                .expect("load optional event blobs"),
            vec![Some(first), None]
        );
    });
}

#[test]
fn resumed_bulk_build_matches_sequential_semantics_across_every_projection() {
    block_on(async {
        let options = NostrEventStoreOptions {
            btree_order: Some(4),
            btree_update_concurrency: None,
            index_commit_batch_size: Some(16),
        };
        let profile_author = "a".repeat(64);
        let article_author = "b".repeat(64);
        let article_tags = vec![vec!["d".to_string(), "article".to_string()]];
        let old_profile = canonical_store_event(
            &profile_author,
            10,
            0,
            vec![vec!["t".to_string(), "profile".to_string()]],
            r#"{"name":"old"}"#,
        );
        let new_profile = canonical_store_event(
            &profile_author,
            1_000,
            0,
            vec![vec!["t".to_string(), "profile".to_string()]],
            r#"{"name":"new"}"#,
        );
        let stale_profile = canonical_store_event(
            &profile_author,
            9,
            0,
            vec![vec!["t".to_string(), "profile".to_string()]],
            r#"{"name":"stale"}"#,
        );
        let old_article = canonical_store_event(
            &article_author,
            20,
            30_023,
            article_tags.clone(),
            "old article",
        );
        let article_candidate_a = canonical_store_event(
            &article_author,
            1_001,
            30_023,
            article_tags.clone(),
            "new article a",
        );
        let article_candidate_b = canonical_store_event(
            &article_author,
            1_001,
            30_023,
            article_tags,
            "new article b",
        );
        let (new_article, losing_article) = if article_candidate_a.id < article_candidate_b.id {
            (article_candidate_a, article_candidate_b)
        } else {
            (article_candidate_b, article_candidate_a)
        };
        let mut initial = (0..60)
            .map(|index| {
                let author = format!("{:064x}", index % 8 + 1);
                canonical_store_event(
                    &author,
                    100 + index,
                    1 + (index % 3) as u32,
                    vec![
                        vec!["t".to_string(), format!("topic-{}", index % 7)],
                        vec!["p".to_string(), format!("{:064x}", index % 5 + 20)],
                    ],
                    &format!("initial-{index}"),
                )
            })
            .collect::<Vec<_>>();
        initial.extend([old_profile.clone(), old_article.clone()]);

        let mut appended = (0..80)
            .map(|index| {
                let author = format!("{:064x}", index % 9 + 30);
                canonical_store_event(
                    &author,
                    2_000 + index,
                    1 + (index % 5) as u32,
                    vec![
                        vec!["t".to_string(), format!("topic-{}", index % 11)],
                        vec!["e".to_string(), format!("{:064x}", index + 100)],
                    ],
                    &format!("appended-{index}"),
                )
            })
            .collect::<Vec<_>>();
        appended.extend([
            stale_profile.clone(),
            new_profile.clone(),
            losing_article,
            new_article.clone(),
            canonical_store_event(
                &format!("{:064x}", 99),
                3_000,
                5,
                vec![vec!["e".to_string(), initial[0].id.clone()]],
                "deletion tombstone",
            ),
        ]);

        let bulk_backing = Arc::new(MemoryStore::new());
        let bulk_store = NostrEventStore::with_options(Arc::clone(&bulk_backing), options.clone());
        let initial_root = bulk_store
            .build(None, initial.clone())
            .await
            .unwrap()
            .expect("bulk initial root");
        let old_profile_cid =
            by_id_event_cid(Arc::clone(&bulk_backing), &initial_root, &old_profile.id)
                .await
                .expect("old profile cid");
        let bulk_report = bulk_store
            .build_with_superseded_nodes(Some(&initial_root), appended.clone())
            .await
            .unwrap();
        let bulk_root = bulk_report.root.expect("bulk appended root");

        let repeated_backing = Arc::new(MemoryStore::new());
        let repeated_store =
            NostrEventStore::with_options(Arc::clone(&repeated_backing), options.clone());
        let repeated_initial = repeated_store
            .build(None, initial.clone())
            .await
            .unwrap()
            .expect("repeated initial root");
        let mut shuffled_appended = appended.clone();
        shuffled_appended.reverse();
        let repeated_root = repeated_store
            .build(Some(&repeated_initial), shuffled_appended)
            .await
            .unwrap()
            .expect("repeated appended root");
        assert_eq!(cid_to_pair(&bulk_root), cid_to_pair(&repeated_root));

        let sequential_backing = Arc::new(MemoryStore::new());
        let sequential_store =
            NostrEventStore::with_options(Arc::clone(&sequential_backing), options);
        let mut sequential_root = sequential_store
            .build(None, initial)
            .await
            .unwrap()
            .expect("sequential initial root");
        appended.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        for event in appended {
            sequential_root = sequential_store
                .add(Some(&sequential_root), event)
                .await
                .unwrap();
        }

        assert_eq!(
            manifest_index_entries(Arc::clone(&bulk_backing), &bulk_root).await,
            manifest_index_entries(Arc::clone(&sequential_backing), &sequential_root).await
        );
        assert_eq!(
            bulk_store
                .get_replaceable(Some(&bulk_root), &profile_author, 0)
                .await
                .unwrap(),
            Some(new_profile)
        );
        assert_eq!(
            bulk_store
                .get_parameterized_replaceable(
                    Some(&bulk_root),
                    &article_author,
                    30_023,
                    "article",
                )
                .await
                .unwrap(),
            Some(new_article)
        );
        assert!(bulk_backing
            .get(&old_profile_cid.hash)
            .await
            .unwrap()
            .is_some());
        assert!(bulk_report
            .superseded_nodes
            .iter()
            .any(|cid| cid == &old_profile_cid));
        bulk_store
            .delete_superseded_nodes(&bulk_report.superseded_nodes)
            .await
            .unwrap();
        assert_eq!(bulk_backing.get(&old_profile_cid.hash).await.unwrap(), None);
        assert_eq!(
            bulk_store
                .get_by_id(Some(&bulk_root), &stale_profile.id)
                .await
                .unwrap(),
            None
        );
    });
}

#[test]
fn resumed_bulk_build_matches_sequential_recovery_from_missing_event_blobs() {
    block_on(async {
        let author = "c".repeat(64);
        let plain = canonical_store_event(
            &author,
            100,
            1,
            vec![vec!["t".to_string(), "missing".to_string()]],
            "plain",
        );
        let old_profile = canonical_store_event(&author, 101, 0, Vec::new(), r#"{"name":"old"}"#);
        let new_profile = canonical_store_event(&author, 200, 0, Vec::new(), r#"{"name":"new"}"#);
        let extra = canonical_store_event(
            &"d".repeat(64),
            201,
            5,
            vec![vec!["e".to_string(), plain.id.clone()]],
            "tombstone",
        );
        let initial = vec![plain.clone(), old_profile.clone()];
        let appended = vec![plain.clone(), new_profile.clone(), extra];

        let bulk_backing = Arc::new(MemoryStore::new());
        let bulk_store = NostrEventStore::new(Arc::clone(&bulk_backing));
        let bulk_initial = bulk_store
            .build(None, initial.clone())
            .await
            .unwrap()
            .expect("bulk initial root");
        for event in [&plain, &old_profile] {
            let cid = by_id_event_cid(Arc::clone(&bulk_backing), &bulk_initial, &event.id)
                .await
                .expect("bulk event cid");
            assert!(bulk_backing.delete(&cid.hash).await.unwrap());
        }
        let bulk_root = bulk_store
            .build(Some(&bulk_initial), appended.clone())
            .await
            .unwrap()
            .expect("bulk recovered root");

        let sequential_backing = Arc::new(MemoryStore::new());
        let sequential_store = NostrEventStore::new(Arc::clone(&sequential_backing));
        let mut sequential_root = sequential_store
            .build(None, initial)
            .await
            .unwrap()
            .expect("sequential initial root");
        for event in [&plain, &old_profile] {
            let cid = by_id_event_cid(Arc::clone(&sequential_backing), &sequential_root, &event.id)
                .await
                .expect("sequential event cid");
            assert!(sequential_backing.delete(&cid.hash).await.unwrap());
        }
        for event in appended {
            sequential_root = sequential_store
                .build(Some(&sequential_root), [event])
                .await
                .unwrap()
                .expect("sequential recovered root");
        }

        assert_eq!(
            manifest_index_entries(Arc::clone(&bulk_backing), &bulk_root).await,
            manifest_index_entries(Arc::clone(&sequential_backing), &sequential_root).await
        );
        assert_eq!(
            bulk_store
                .get_by_id(Some(&bulk_root), &plain.id)
                .await
                .unwrap(),
            Some(plain)
        );
        assert_eq!(
            bulk_store
                .get_replaceable(Some(&bulk_root), &author, 0)
                .await
                .unwrap(),
            Some(new_profile)
        );
    });
}

#[test]
fn incremental_build_reports_nodes_safe_to_delete_after_root_publication() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::with_options(
            Arc::clone(&backing),
            NostrEventStoreOptions {
                btree_order: Some(8),
                btree_update_concurrency: None,
                index_commit_batch_size: Some(64),
            },
        );
        let author = "d".repeat(64);
        let initial = (0..128)
            .map(|index| {
                canonical_store_event(
                    &author,
                    1_000 + index,
                    1,
                    vec![vec!["t".to_string(), format!("initial-{}", index % 7)]],
                    &format!("initial-{index}"),
                )
            })
            .collect::<Vec<_>>();
        let root = store
            .build(None, initial)
            .await
            .unwrap()
            .expect("initial root");
        let appended = (0..96)
            .map(|index| {
                canonical_store_event(
                    &author,
                    2_000 + index,
                    1,
                    vec![vec!["t".to_string(), format!("appended-{}", index % 5)]],
                    &format!("appended-{index}"),
                )
            })
            .collect::<Vec<_>>();

        let report = store
            .build_with_superseded_nodes(Some(&root), appended)
            .await
            .expect("incremental build");
        let updated = report.root.expect("updated root");
        assert!(!report.superseded_nodes.is_empty());
        assert!(report.superseded_nodes.contains(&root));
        assert_eq!(
            store
                .delete_superseded_nodes(&report.superseded_nodes)
                .await
                .expect("delete superseded nodes"),
            report.superseded_nodes.len()
        );

        assert_eq!(
            store
                .list_by_author(Some(&updated), &author, ListEventsOptions::default())
                .await
                .expect("list after reclamation")
                .len(),
            224
        );
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
fn damaged_replaceable_index_can_be_rebuilt_from_author_kind_time() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::with_options(
            Arc::clone(&backing),
            NostrEventStoreOptions {
                btree_order: Some(4),
                btree_update_concurrency: None,
                index_commit_batch_size: Some(8),
            },
        );
        let author_a = "a".repeat(64);
        let author_b = "b".repeat(64);
        let profile_a = canonical_store_event(&author_a, 7, 0, Vec::new(), "profile a");
        let profile_b = canonical_store_event(&author_b, 9, 0, Vec::new(), "profile b");
        let note = canonical_store_event(&author_a, 11, 1, Vec::new(), "ordinary note");
        let article = canonical_store_event(
            &author_a,
            13,
            30_023,
            vec![vec!["d".to_string(), "repair-article".to_string()]],
            "article",
        );
        let mut events = vec![
            note.clone(),
            profile_b.clone(),
            profile_a.clone(),
            article.clone(),
        ];
        for index in 0u64..96 {
            let author = if index.is_multiple_of(2) {
                &author_a
            } else {
                &author_b
            };
            events.push(canonical_store_event(
                author,
                100 + index,
                1,
                Vec::new(),
                &format!("repair note {index}"),
            ));
        }
        let expected_events = events.len() as u64;
        let root = store
            .build(None, events)
            .await
            .expect("build event index")
            .expect("event index root");
        let manifest = store
            .get_manifest(Some(&root))
            .await
            .expect("read manifest");
        let article_cid = by_id_event_cid(Arc::clone(&backing), &root, &article.id)
            .await
            .expect("article event cid");
        assert!(backing
            .delete(&article_cid.hash)
            .await
            .expect("delete article event blob"));
        let missing = store
            .missing_parameterized_event_links(&root, 2)
            .await
            .expect("scan missing parameterized event links");
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].cid, article_cid);
        let damaged = manifest.replaceable.expect("replaceable root");
        let damaged_parameterized = manifest
            .parameterized_replaceable
            .expect("parameterized replaceable root");
        assert!(backing
            .delete(&damaged.hash)
            .await
            .expect("delete replaceable root"));
        assert!(backing
            .delete(&damaged_parameterized.hash)
            .await
            .expect("delete parameterized replaceable root"));

        let repair_base = store
            .clear_replaceable_indexes(&root)
            .await
            .expect("clear damaged replaceable indexes");
        let event_repair = store
            .repair_missing_event_blobs_with_superseded_nodes(&repair_base, [article.clone()])
            .await
            .expect("repair missing event blob");
        assert!(
            !event_repair.superseded_nodes.is_empty(),
            "event repair should report the replaced copy-on-write path"
        );
        let repaired_event_root = event_repair.root.expect("repaired event root");
        assert_eq!(
            store
                .get_by_id(Some(&repaired_event_root), &article.id)
                .await
                .expect("read restored article by id"),
            Some(article.clone())
        );

        let repaired = store
            .rebuild_replaceable_index(&repaired_event_root, 2)
            .await
            .expect("repair replaceable index");

        assert_eq!(repaired.entries_scanned, expected_events);
        assert_eq!(repaired.replaceable_entries, 2);
        assert_eq!(repaired.parameterized_replaceable_entries, 1);
        let repaired_manifest = store
            .get_manifest(Some(&repaired.root))
            .await
            .expect("read replaceable-repaired manifest");
        let damaged_time = repaired_manifest.by_time.expect("time root");
        assert!(backing
            .delete(&damaged_time.hash)
            .await
            .expect("delete time root"));
        let prepared = store
            .clear_time_index(&repaired.root)
            .await
            .expect("publish time-repair manifest");
        let prepared_manifest = store
            .get_manifest(Some(&prepared))
            .await
            .expect("read time-repair manifest");
        assert_eq!(prepared_manifest.by_time, None);
        assert_eq!(
            store
                .get_by_id(Some(&prepared), &note.id)
                .await
                .expect("read authoritative index through repair manifest"),
            Some(note.clone())
        );
        let time_repaired = store
            .rebuild_time_index(&prepared, 2)
            .await
            .expect("repair time index");
        assert_eq!(time_repaired.entries_scanned, expected_events);
        let final_root = &time_repaired.root;
        let final_manifest = store
            .get_manifest(Some(final_root))
            .await
            .expect("read final manifest");
        let by_time = final_manifest.by_time.clone().expect("rebuilt time root");
        let rebuilt_time_index = BTree::new(Arc::clone(&backing), BTreeOptions { order: Some(4) });
        assert_eq!(
            rebuilt_time_index
                .count_stored_links(Some(&by_time))
                .await
                .expect("traverse complete rebuilt time index before cleanup"),
            Some(expected_events)
        );
        store
            .delete_superseded_nodes(&event_repair.superseded_nodes)
            .await
            .expect("delete nodes superseded by event repair");
        assert_eq!(
            store
                .get_replaceable(Some(final_root), &author_a, 0)
                .await
                .expect("read repaired profile a"),
            Some(profile_a)
        );
        assert_eq!(
            store
                .get_replaceable(Some(final_root), &author_b, 0)
                .await
                .expect("read repaired profile b"),
            Some(profile_b)
        );
        assert_eq!(
            store
                .get_parameterized_replaceable(
                    Some(final_root),
                    &author_a,
                    30_023,
                    "repair-article",
                )
                .await
                .expect("read repaired article"),
            Some(article.clone())
        );
        assert_eq!(
            store
                .get_by_id(Some(final_root), &note.id)
                .await
                .expect("read unchanged by-id index"),
            Some(note.clone())
        );
        assert_eq!(
            rebuilt_time_index
                .count_stored_links(Some(&by_time))
                .await
                .expect("traverse complete rebuilt time index"),
            Some(expected_events)
        );

        let damaged_by_tag = final_manifest.by_tag.expect("tag root");
        assert!(backing
            .delete(&damaged_by_tag.hash)
            .await
            .expect("delete tag root"));
        let prepared_all = store
            .clear_derived_indexes(final_root)
            .await
            .expect("publish all-derived repair manifest");
        let prepared_all_manifest = store
            .get_manifest(Some(&prepared_all))
            .await
            .expect("read all-derived repair manifest");
        assert!(prepared_all_manifest.by_author_kind_time.is_some());
        assert_eq!(prepared_all_manifest.by_id, None);
        assert_eq!(prepared_all_manifest.by_author_time, None);
        assert_eq!(prepared_all_manifest.by_kind_time, None);
        assert_eq!(prepared_all_manifest.by_kind_time_author, None);
        assert_eq!(prepared_all_manifest.by_time, None);
        assert_eq!(prepared_all_manifest.by_tag, None);
        assert_eq!(prepared_all_manifest.replaceable, None);
        assert_eq!(prepared_all_manifest.parameterized_replaceable, None);

        let all_repaired = store
            .rebuild_derived_indexes(&prepared_all, 2)
            .await
            .expect("rebuild all derived indexes");
        assert_eq!(all_repaired.entries_scanned, expected_events);
        assert_eq!(all_repaired.replaceable_entries, 2);
        assert_eq!(all_repaired.parameterized_replaceable_entries, 1);
        store
            .validate_index_root(Some(&all_repaired.root))
            .await
            .expect("validate fully rebuilt root");
        assert_eq!(
            store
                .get_by_id(Some(&all_repaired.root), &note.id)
                .await
                .expect("read rebuilt by-id index"),
            Some(note)
        );
        assert_eq!(
            store
                .list_by_tag(
                    Some(&all_repaired.root),
                    "d",
                    "repair-article",
                    ListEventsOptions::default(),
                )
                .await
                .expect("read rebuilt tag index"),
            vec![article]
        );
    });
}

#[test]
fn same_second_replaceable_events_prefer_the_lowest_id_in_every_build_order() {
    block_on(async {
        let author = "a".repeat(64);
        let profile_a = canonical_store_event(&author, 10, 0, Vec::new(), r#"{"name":"a"}"#);
        let profile_b = canonical_store_event(&author, 10, 0, Vec::new(), r#"{"name":"b"}"#);
        let article_tags = vec![vec!["d".to_string(), "article-1".to_string()]];
        let article_a =
            canonical_store_event(&author, 20, 30_023, article_tags.clone(), "article a");
        let article_b = canonical_store_event(&author, 20, 30_023, article_tags, "article b");
        let (profile_low, profile_high) = if profile_a.id < profile_b.id {
            (profile_a, profile_b)
        } else {
            (profile_b, profile_a)
        };
        let (article_low, article_high) = if article_a.id < article_b.id {
            (article_a, article_b)
        } else {
            (article_b, article_a)
        };

        let bulk_high_first_store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let bulk_high_first = bulk_high_first_store
            .build(
                None,
                vec![
                    profile_high.clone(),
                    article_high.clone(),
                    profile_low.clone(),
                    article_low.clone(),
                ],
            )
            .await
            .unwrap()
            .expect("high-first bulk root");

        let bulk_low_first_store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let bulk_low_first = bulk_low_first_store
            .build(
                None,
                vec![
                    article_low.clone(),
                    profile_low.clone(),
                    article_high.clone(),
                    profile_high.clone(),
                ],
            )
            .await
            .unwrap()
            .expect("low-first bulk root");

        let incremental_high_first_store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let mut incremental_high_first = None;
        for event in [
            profile_high.clone(),
            article_high.clone(),
            profile_low.clone(),
            article_low.clone(),
        ] {
            incremental_high_first = Some(
                incremental_high_first_store
                    .add(incremental_high_first.as_ref(), event)
                    .await
                    .unwrap(),
            );
        }
        let incremental_high_first = incremental_high_first.expect("high-first incremental root");

        let incremental_low_first_store = NostrEventStore::new(Arc::new(MemoryStore::new()));
        let mut incremental_low_first = None;
        for event in [
            article_low.clone(),
            profile_low.clone(),
            article_high,
            profile_high,
        ] {
            incremental_low_first = Some(
                incremental_low_first_store
                    .add(incremental_low_first.as_ref(), event)
                    .await
                    .unwrap(),
            );
        }
        let incremental_low_first = incremental_low_first.expect("low-first incremental root");

        assert_eq!(cid_to_pair(&bulk_high_first), cid_to_pair(&bulk_low_first));
        assert_eq!(
            cid_to_pair(&bulk_high_first),
            cid_to_pair(&incremental_high_first)
        );
        assert_eq!(
            cid_to_pair(&bulk_high_first),
            cid_to_pair(&incremental_low_first)
        );
        assert_eq!(
            bulk_high_first_store
                .get_replaceable(Some(&bulk_high_first), &author, 0)
                .await
                .unwrap(),
            Some(profile_low)
        );
        assert_eq!(
            bulk_high_first_store
                .get_parameterized_replaceable(
                    Some(&bulk_high_first),
                    &author,
                    30_023,
                    "article-1",
                )
                .await
                .unwrap(),
            Some(article_low)
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
fn adding_an_existing_event_repairs_a_missing_replaceable_projection() {
    block_on(async {
        let backing = Arc::new(MemoryStore::new());
        let store = NostrEventStore::new(Arc::clone(&backing));
        let author = "a".repeat(64);
        let contact_list = canonical_store_event(&author, 5, 3, Vec::new(), "local contact list");

        let root = store.add(None, contact_list.clone()).await.unwrap();
        let root_without_replaceable = store
            .clear_replaceable_indexes(&root)
            .await
            .expect("clear replaceable projection");

        let repaired_root = store
            .add(Some(&root_without_replaceable), contact_list.clone())
            .await
            .expect("re-adding an existing event must be idempotent");

        assert_eq!(
            store
                .get_by_id(Some(&repaired_root), &contact_list.id)
                .await
                .unwrap(),
            Some(contact_list.clone())
        );
        assert_eq!(
            store
                .get_replaceable(Some(&repaired_root), &author, 3)
                .await
                .unwrap(),
            Some(contact_list)
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
