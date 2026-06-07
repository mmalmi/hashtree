use std::sync::Arc;

use futures::executor::block_on;
use hashtree_core::{from_hex, nhash_encode, xor_keys, Cid, MemoryStore};
use hashtree_nostr::{
    parse_tree_event_snapshot_permalink, read_tree_event_snapshot, resolve_snapshot_root_cid,
    serialize_tree_event_snapshot_permalink, store_tree_event_snapshot, StoredNostrEvent,
    TreeEventSnapshotInfo, TreeEventSnapshotPermalink,
};
use nostr_sdk::{PublicKey, ToBech32};

fn event(overrides: Option<Vec<Vec<String>>>) -> StoredNostrEvent {
    StoredNostrEvent {
        id: "1".repeat(64),
        pubkey: "2".repeat(64),
        created_at: 1_700_000_000,
        kind: 30078,
        tags: overrides.unwrap_or_else(|| {
            vec![
                vec!["d".to_string(), "videos/demo".to_string()],
                vec!["l".to_string(), "hashtree".to_string()],
                vec!["hash".to_string(), "3".repeat(64)],
                vec!["key".to_string(), "4".repeat(64)],
            ]
        }),
        content: String::new(),
        sig: "5".repeat(128),
    }
}

fn snapshot(overrides: impl FnOnce(&mut TreeEventSnapshotInfo)) -> TreeEventSnapshotInfo {
    let snapshot_hash = from_hex(&"6".repeat(64)).expect("snapshot hash");
    let mut snapshot = TreeEventSnapshotInfo {
        event: event(None),
        tree_name: "videos/demo".to_string(),
        root_cid: Cid {
            hash: from_hex(&"3".repeat(64)).expect("root hash"),
            key: Some(from_hex(&"4".repeat(64)).expect("root key")),
        },
        visibility: hashtree_core::TreeVisibility::Public,
        labels: vec!["hashtree".to_string()],
        encrypted_key: None,
        key_id: None,
        self_encrypted_key: None,
        self_encrypted_link_key: None,
        snapshot_cid: Cid {
            hash: snapshot_hash,
            key: None,
        },
        snapshot_nhash: nhash_encode(&snapshot_hash).expect("snapshot nhash"),
        npub: PublicKey::from_hex(&"2".repeat(64))
            .expect("pubkey")
            .to_bech32()
            .expect("npub"),
    };
    overrides(&mut snapshot);
    snapshot
}

#[test]
fn stores_and_reads_tree_event_snapshots_with_npub_and_snapshot_nhash() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let event = event(None);

        let snapshot = store_tree_event_snapshot(Arc::clone(&store), &event)
            .await
            .expect("store snapshot")
            .expect("parsed snapshot");
        let restored = read_tree_event_snapshot(store, &snapshot.snapshot_cid, None)
            .await
            .expect("read snapshot")
            .expect("restored snapshot");

        assert!(snapshot.snapshot_nhash.starts_with("nhash1"));
        assert_eq!(
            snapshot.npub,
            PublicKey::from_hex(&event.pubkey)
                .expect("pubkey")
                .to_bech32()
                .expect("npub")
        );
        assert_eq!(restored, snapshot);
    });
}

#[test]
fn resolves_link_visible_snapshot_roots_with_link_keys() {
    block_on(async {
        let link_key = from_hex(&"7".repeat(64)).expect("link key");
        let content_key = from_hex(&"8".repeat(64)).expect("content key");
        let encrypted_key = xor_keys(&content_key, &link_key);
        let snapshot = snapshot(|snapshot| {
            snapshot.visibility = hashtree_core::TreeVisibility::LinkVisible;
            snapshot.root_cid = Cid {
                hash: from_hex(&"3".repeat(64)).expect("root hash"),
                key: None,
            };
            snapshot.encrypted_key = Some(hex::encode(encrypted_key));
        });

        let resolved = resolve_snapshot_root_cid(&snapshot, Some(&"7".repeat(64)))
            .expect("resolve snapshot root");

        assert_eq!(
            resolved,
            Some(Cid {
                hash: from_hex(&"3".repeat(64)).expect("root hash"),
                key: Some(content_key),
            })
        );
    });
}

#[test]
fn serializes_and_parses_tree_event_snapshot_permalinks() {
    let snapshot_nhash =
        nhash_encode(&from_hex(&"6".repeat(64)).expect("snapshot hash")).expect("snapshot nhash");
    let serialized = serialize_tree_event_snapshot_permalink(&TreeEventSnapshotPermalink {
        snapshot_nhash: snapshot_nhash.clone(),
        path: vec!["nested folder".to_string(), "video.mp4".to_string()],
        link_key: Some("a".repeat(64)),
    })
    .expect("serialize permalink");

    assert_eq!(
        serialized,
        format!(
            "{}/nested%20folder/video.mp4?snapshot=1&k={}",
            snapshot_nhash,
            "a".repeat(64)
        )
    );

    assert_eq!(
        parse_tree_event_snapshot_permalink(&format!(
            "https://sites.iris.to/#/{}/index.html?snapshot=1&k={}",
            snapshot_nhash,
            "b".repeat(64)
        )),
        Some(TreeEventSnapshotPermalink {
            snapshot_nhash,
            path: vec!["index.html".to_string()],
            link_key: Some("b".repeat(64)),
        })
    );
}
