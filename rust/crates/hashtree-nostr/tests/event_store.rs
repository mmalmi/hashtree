use std::sync::Arc;

use futures::executor::block_on;
use hashtree_core::{
    sha256, Cid, DirEntry, HashTree, HashTreeConfig, MemoryStore, Store, TreeVisibility,
};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    build_private_hashtree_root_event, decode_signed_event_json, decode_stored_event_msgpack,
    encode_signed_event_json, encode_stored_event_msgpack, parse_hashtree_root_event,
    parse_verified_hashtree_root_event, read_signed_event_snapshot,
    resolve_self_encrypted_root_cid, store_signed_event_snapshot,
    stored_event_from_nostr_sdk_event, ListEventsOptions, NostrEventStore, StoredNostrEvent,
    VerifiedEvent, VerifiedStoredNostrEvent, HASHTREE_LEGACY_ROOT_KIND, HASHTREE_ROOT_KIND,
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
