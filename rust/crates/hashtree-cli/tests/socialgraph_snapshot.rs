use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
use nostr_social_graph::{BinaryBudget, NostrEvent as ExternalNostrEvent, SocialGraph};
use tempfile::TempDir;

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

#[test]
fn snapshot_includes_list_timestamps() {
    let _guard = test_lock();
    let tmp = TempDir::new().unwrap();
    let graph_store = hashtree_cli::socialgraph::open_social_graph_store(tmp.path()).unwrap();

    let root_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let carol_keys = Keys::generate();

    let root_pk = root_keys.public_key();
    let bob_pk = bob_keys.public_key();
    let carol_pk = carol_keys.public_key();

    hashtree_cli::socialgraph::set_social_graph_root(&graph_store, &root_pk.to_bytes());
    std::thread::sleep(Duration::from_millis(100));

    let follow_created_at = 1_700_000_111;
    let mute_created_at = 1_700_000_222;

    let follow_event = event_builder!(Kind::ContactList, "", [Tag::public_key(bob_pk)])
        .custom_created_at(Timestamp::from_secs(follow_created_at))
        .sign_with_keys(&root_keys)
        .unwrap();

    let mute_event = event_builder!(Kind::MuteList, "", [Tag::public_key(carol_pk)])
        .custom_created_at(Timestamp::from_secs(mute_created_at))
        .sign_with_keys(&root_keys)
        .unwrap();

    hashtree_cli::socialgraph::ingest_event(&graph_store, "follow", &follow_event.as_json());
    hashtree_cli::socialgraph::ingest_event(&graph_store, "mute", &mute_event.as_json());
    std::thread::sleep(Duration::from_millis(200));

    let options = hashtree_cli::socialgraph::snapshot::SnapshotOptions::default();
    let chunks = hashtree_cli::socialgraph::snapshot::build_snapshot_chunks(
        &graph_store,
        &root_pk.to_bytes(),
        &options,
    )
    .unwrap();

    let data = flatten_chunks(chunks);

    let parsed = parse_snapshot(&data);
    let root_id = find_id(&parsed.id_to_pubkey, &root_pk.to_bytes()).expect("root id");
    let bob_id = find_id(&parsed.id_to_pubkey, &bob_pk.to_bytes()).expect("bob id");
    let carol_id = find_id(&parsed.id_to_pubkey, &carol_pk.to_bytes()).expect("carol id");

    let (follow_ts, follow_targets) = parsed.follow_lists.get(&root_id).expect("root follow list");
    assert_eq!(*follow_ts, follow_created_at);
    assert!(follow_targets.contains(&bob_id));

    let (mute_ts, mute_targets) = parsed.mute_lists.get(&root_id).expect("root mute list");
    assert_eq!(*mute_ts, mute_created_at);
    assert!(mute_targets.contains(&carol_id));
}

#[test]
fn snapshot_binary_matches_upstream_social_graph_encoding() {
    let _guard = test_lock();
    let tmp = TempDir::new().unwrap();
    let graph_store = hashtree_cli::socialgraph::open_social_graph_store(tmp.path()).unwrap();

    let root_keys = Keys::generate();
    let outsider_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let carol_keys = Keys::generate();

    let root_pk = root_keys.public_key();
    hashtree_cli::socialgraph::set_social_graph_root(&graph_store, &root_pk.to_bytes());

    let outsider_follow = event_builder!(
        Kind::ContactList,
        "",
        [Tag::public_key(carol_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&outsider_keys)
    .unwrap();
    let root_lists = [
        event_builder!(
            Kind::ContactList,
            "",
            [Tag::public_key(bob_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(11))
        .sign_with_keys(&root_keys)
        .unwrap(),
        event_builder!(
            Kind::MuteList,
            "",
            [Tag::public_key(carol_keys.public_key())],
        )
        .custom_created_at(Timestamp::from_secs(12))
        .sign_with_keys(&root_keys)
        .unwrap(),
    ];

    hashtree_cli::socialgraph::ingest_event(
        &graph_store,
        "outsider-follow",
        &outsider_follow.as_json(),
    );
    for event in &root_lists {
        hashtree_cli::socialgraph::ingest_event(&graph_store, "root-list", &event.as_json());
    }

    let options = hashtree_cli::socialgraph::snapshot::SnapshotOptions {
        max_nodes: Some(3),
        max_edges: Some(2),
        max_distance: Some(1),
        max_edges_per_node: Some(2),
    };
    let actual = flatten_chunks(
        hashtree_cli::socialgraph::snapshot::build_snapshot_chunks(
            &graph_store,
            &root_pk.to_bytes(),
            &options,
        )
        .unwrap(),
    );

    let mut expected_graph = SocialGraph::new(&root_pk.to_hex());
    expected_graph.handle_event(&to_external_event(&outsider_follow), true, 0.0);
    for event in &root_lists {
        expected_graph.handle_event(&to_external_event(event), true, 0.0);
    }

    let expected = expected_graph
        .to_binary_with_budget(BinaryBudget {
            max_nodes: options.max_nodes,
            max_edges: options.max_edges,
            max_distance: options.max_distance,
            max_edges_per_node: options.max_edges_per_node,
        })
        .unwrap();

    assert_eq!(actual, expected);
}

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn flatten_chunks(chunks: Vec<Bytes>) -> Vec<u8> {
    let total = chunks.iter().map(|c| c.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    for chunk in chunks {
        out.extend_from_slice(&chunk);
    }
    out
}

fn to_external_event(event: &nostr::Event) -> ExternalNostrEvent {
    ExternalNostrEvent {
        created_at: event.created_at.as_secs(),
        content: event.content.clone(),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        kind: event.kind.as_u16() as u32,
        pubkey: event.pubkey.to_hex(),
        id: event.id.to_hex(),
        sig: event.sig.to_string(),
    }
}

struct ParsedSnapshot {
    id_to_pubkey: HashMap<u32, [u8; 32]>,
    follow_lists: HashMap<u32, (u64, Vec<u32>)>,
    mute_lists: HashMap<u32, (u64, Vec<u32>)>,
}

fn parse_snapshot(data: &[u8]) -> ParsedSnapshot {
    let mut offset = 0usize;
    let _version = read_varint(data, &mut offset);
    let id_count = read_varint(data, &mut offset) as usize;

    let mut id_to_pubkey = HashMap::new();
    for _ in 0..id_count {
        let pk = data[offset..offset + 32].try_into().unwrap();
        offset += 32;
        let id = read_varint(data, &mut offset) as u32;
        id_to_pubkey.insert(id, pk);
    }

    let follow_lists_count = read_varint(data, &mut offset) as usize;
    let mut follow_lists = HashMap::new();
    for _ in 0..follow_lists_count {
        let owner = read_varint(data, &mut offset) as u32;
        let ts = read_varint(data, &mut offset);
        let count = read_varint(data, &mut offset) as usize;
        let mut targets = Vec::with_capacity(count);
        for _ in 0..count {
            targets.push(read_varint(data, &mut offset) as u32);
        }
        follow_lists.insert(owner, (ts, targets));
    }

    let mute_lists_count = read_varint(data, &mut offset) as usize;
    let mut mute_lists = HashMap::new();
    for _ in 0..mute_lists_count {
        let owner = read_varint(data, &mut offset) as u32;
        let ts = read_varint(data, &mut offset);
        let count = read_varint(data, &mut offset) as usize;
        let mut targets = Vec::with_capacity(count);
        for _ in 0..count {
            targets.push(read_varint(data, &mut offset) as u32);
        }
        mute_lists.insert(owner, (ts, targets));
    }

    ParsedSnapshot {
        id_to_pubkey,
        follow_lists,
        mute_lists,
    }
}

fn read_varint(data: &[u8], offset: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = data[*offset];
        *offset += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if (byte & 0x80) == 0 {
            break;
        }
        shift += 7;
    }
    value
}

fn find_id(map: &HashMap<u32, [u8; 32]>, pk: &[u8; 32]) -> Option<u32> {
    map.iter()
        .find_map(|(id, value)| if value == pk { Some(*id) } else { None })
}
