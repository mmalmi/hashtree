use super::*;
use heed::byteorder::BigEndian;
use heed::types::{Bytes as HeedBytes, SerdeBincode, Str, U32, U64};
use heed::{Database, EnvOpenOptions};
use nostr::{EventBuilder, Keys, Tag, Timestamp};
use tempfile::TempDir;

const MAP_SIZE: u64 = 128 * 1024 * 1024;

// Seed old table bytes directly: neither the new normalizer nor replace_state
// may repair this fixture before the CLI's real writable-open path is tested.
fn legacy_alias_rows(path: &Path, root: &str, friend: &str, muted: &str, collision: bool) {
    std::fs::create_dir_all(path).unwrap();
    let mut options = EnvOpenOptions::new();
    options
        .map_size(MAP_SIZE as usize)
        .max_dbs(SOCIALGRAPH_MAX_DBS);
    let env = unsafe { options.flags(EnvFlags::NO_LOCK).open(path).unwrap() };
    let mut tx = env.write_txn().unwrap();
    let metadata: Database<Str, HeedBytes> =
        env.create_database(&mut tx, Some("metadata")).unwrap();
    metadata
        .put(&mut tx, "root", root.to_uppercase().as_bytes())
        .unwrap();
    metadata
        .put(&mut tx, "next_unique_id", &7u32.to_be_bytes())
        .unwrap();
    let forward: Database<Str, U32<BigEndian>> = env
        .create_database(&mut tx, Some("str_to_unique_id"))
        .unwrap();
    let reverse: Database<U32<BigEndian>, Str> = env
        .create_database(&mut tx, Some("unique_id_to_str"))
        .unwrap();
    for (name, id) in [
        (root.to_owned(), 0),
        (friend.to_owned(), 1),
        (muted.to_owned(), 2),
        (root.to_uppercase(), 4),
        (friend.to_uppercase(), 5),
        (muted.to_uppercase(), 6),
    ] {
        let forward_id = if collision && id == 1 { 0 } else { id };
        forward.put(&mut tx, &name, &forward_id).unwrap();
        reverse.put(&mut tx, &id, &name).unwrap();
    }
    for (name, rows) in [
        ("followed_by_user", vec![(0, vec![2]), (4, vec![5])]),
        ("muted_by_user", vec![(0, vec![1]), (4, vec![6])]),
        // Derived rows are deliberately stale and contain duplicate aliases.
        ("followers_by_user", vec![(2, vec![0]), (5, vec![4])]),
        ("user_muted_by", vec![(1, vec![0]), (6, vec![4])]),
        (
            "users_by_follow_distance",
            vec![(0, vec![0, 4]), (7, vec![1, 5]), (9, vec![2, 6])],
        ),
    ] {
        let db: Database<U32<BigEndian>, SerdeBincode<Vec<u32>>> =
            env.create_database(&mut tx, Some(name)).unwrap();
        for (owner, targets) in rows {
            db.put(&mut tx, &owner, &targets).unwrap();
        }
    }
    let distances: Database<U32<BigEndian>, U32<BigEndian>> = env
        .create_database(&mut tx, Some("follow_distance_by_user"))
        .unwrap();
    for (owner, distance) in [(0, 0), (4, 0), (1, 7), (5, 7), (2, 9), (6, 9)] {
        distances.put(&mut tx, &owner, &distance).unwrap();
    }
    for (name, rows) in [
        ("follow_list_created_at", [(0, 10), (4, 20)]),
        ("mute_list_created_at", [(0, 15), (4, 30)]),
    ] {
        let db: Database<U32<BigEndian>, U64<BigEndian>> =
            env.create_database(&mut tx, Some(name)).unwrap();
        for (owner, timestamp) in rows {
            db.put(&mut tx, &owner, &timestamp).unwrap();
        }
    }
    for (kind, timestamp, complete, id) in [(3, 20u64, false, "a1"), (10_000, 30, true, "b2")] {
        let mut bytes = vec![u8::from(complete)];
        bytes.extend_from_slice(&timestamp.to_be_bytes());
        bytes.extend_from_slice(id.repeat(32).as_bytes());
        metadata
            .put(&mut tx, &format!("revision:{kind}:4"), &bytes)
            .unwrap();
    }
    tx.commit().unwrap();
    env.prepare_for_closing().wait();
}

#[test]
fn legacy_alias_migration_precedes_cli_cache_and_list_updates_invalidate_it() {
    let _guard = test_lock_blocking();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("socialgraph");
    let root = Keys::parse("01".repeat(32)).unwrap();
    let friend = Keys::parse("02".repeat(32)).unwrap();
    let muted = Keys::parse("03".repeat(32)).unwrap();
    let root_hex = root.public_key().to_hex();
    legacy_alias_rows(
        &path,
        &root_hex,
        &friend.public_key().to_hex(),
        &muted.public_key().to_hex(),
        false,
    );
    let store = open_test_social_graph_store_at_path(&path, Some(MAP_SIZE)).unwrap();

    let stats = store.stats().unwrap();
    assert_eq!(stats.root.as_deref(), Some(root_hex.as_str()));
    assert_eq!(stats.total_users, 2);
    assert_eq!(stats.total_follows, 1);
    assert_eq!(stats.max_depth, 1);
    assert_eq!(stats.size_by_distance, BTreeMap::from([(0, 1), (1, 1)]));
    assert_eq!(
        store.users_by_follow_distance(0).unwrap(),
        vec![root.public_key().to_bytes()]
    );
    assert_eq!(
        store.users_by_follow_distance(1).unwrap(),
        vec![friend.public_key().to_bytes()]
    );
    assert!(store.users_by_follow_distance(7).unwrap().is_empty());
    {
        let graph = store.graph.lock().unwrap();
        assert_eq!(graph.export_state().unwrap().unique_ids.len(), 3);
        assert_eq!(
            graph.get_muted_by_user(&root_hex.to_uppercase()).unwrap(),
            vec![muted.public_key().to_hex()]
        );
        let follow = graph.get_list_revision(&root_hex, 3).unwrap().unwrap();
        assert_eq!(follow.id, "a1".repeat(32));
        assert!(!follow.complete);
        let mute = graph.get_list_revision(&root_hex, 10_000).unwrap().unwrap();
        assert_eq!(mute.id, "b2".repeat(32));
        assert!(mute.complete);
    }

    // The first queries populated the real CLI cache. Its next read must observe
    // the new list, rather than retaining the migrated generation indefinitely.
    let update = EventBuilder::new(Kind::ContactList, "")
        .tags([Tag::public_key(muted.public_key())])
        .custom_created_at(Timestamp::from_secs(40))
        .sign_with_keys(&root)
        .unwrap();
    ingest_graph_parsed_events(store.as_ref(), std::slice::from_ref(&update)).unwrap();
    assert_eq!(
        store.users_by_follow_distance(1).unwrap(),
        vec![muted.public_key().to_bytes()]
    );
    assert_eq!(store.stats().unwrap().total_users, 2);
    store.set_root(&friend.public_key().to_bytes()).unwrap();
    assert_eq!(store.stats().unwrap().total_users, 1);
    assert_eq!(
        store.users_by_follow_distance(0).unwrap(),
        vec![friend.public_key().to_bytes()]
    );
    store.set_root(&root.public_key().to_bytes()).unwrap();
    assert_eq!(store.stats().unwrap().total_users, 2);
    drop(store);
    let reopened = open_test_social_graph_store_at_path(&path, Some(MAP_SIZE)).unwrap();
    assert_eq!(
        reopened.users_by_follow_distance(1).unwrap(),
        vec![muted.public_key().to_bytes()]
    );
    let graph = reopened.graph.lock().unwrap();
    assert_eq!(
        graph.get_list_revision(&root_hex, 3).unwrap().unwrap().id,
        update.id.to_hex()
    );
    assert_eq!(
        graph
            .get_list_revision(&root_hex, 10_000)
            .unwrap()
            .unwrap()
            .id,
        "b2".repeat(32)
    );
}

#[test]
fn ambiguous_legacy_rows_fail_cli_open_without_replacing_personal_graph() {
    let _guard = test_lock_blocking();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("socialgraph");
    legacy_alias_rows(
        &path,
        &"aa".repeat(32),
        &"bb".repeat(32),
        &"cc".repeat(32),
        true,
    );
    let before = std::fs::read(path.join("data.mdb")).unwrap();
    assert!(open_test_social_graph_store_at_path(&path, Some(MAP_SIZE)).is_err());
    assert_eq!(std::fs::read(path.join("data.mdb")).unwrap(), before);
}
