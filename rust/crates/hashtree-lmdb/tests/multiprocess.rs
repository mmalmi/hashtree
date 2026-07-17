#[path = "multiprocess/support.rs"]
mod support;

use hashtree_core::Store;
use hashtree_lmdb::{
    compute_sha256, open_shared_lmdb_blob_store, ConfiguredLmdbBlobStore, LmdbBlobStore, PoolStore,
    PoolStoreConfig, SHARED_BLOB_MIN_MAP_SIZE_BYTES, SHARED_BLOB_POOL_DIR_NAME,
};
use std::fs;
use support::*;
use tempfile::TempDir;

#[test]
fn concurrent_processes_read_one_committed_blob() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("blobs");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");
    let hash = compute_sha256(SHARED_DATA);
    let store = LmdbBlobStore::new(&db).expect("open LMDB store");
    assert!(store.put_sync(hash, SHARED_DATA).expect("seed blob"));
    drop(store);

    let readers = (0..4)
        .map(|id| spawn_helper("read-shared", &db, &control, id))
        .collect::<Vec<_>>();
    for id in 0..readers.len() {
        wait_for(&control.join(format!("{id}-ready")));
    }
    fs::write(control.join("go"), b"go").expect("release readers");
    for (id, child) in readers.into_iter().enumerate() {
        wait_success(child, &format!("reader {id}"));
    }
}

#[test]
fn concurrent_process_writes_are_hash_verified_and_idempotent() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("blobs");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");

    let writers = (0..4)
        .map(|id| spawn_helper("write-shared", &db, &control, id))
        .collect::<Vec<_>>();
    for id in 0..writers.len() {
        wait_for(&control.join(format!("{id}-ready")));
    }
    fs::write(control.join("go"), b"go").expect("release writers");
    for (id, child) in writers.into_iter().enumerate() {
        wait_success(child, &format!("writer {id}"));
    }

    let inserted = (0..4)
        .map(|id| fs::read_to_string(control.join(format!("{id}-result"))).expect("write result"))
        .filter(|result| result == "inserted")
        .count();
    assert_eq!(inserted, 1, "exactly one process must insert the hash");

    let store = LmdbBlobStore::new(&db).expect("reopen shared store");
    let hash = compute_sha256(SHARED_DATA);
    let data = store
        .get_sync(&hash)
        .expect("read shared blob")
        .expect("blob");
    assert_eq!(
        compute_sha256(&data),
        hash,
        "stored bytes must match their key"
    );
    assert_eq!(data, SHARED_DATA);
    let stats = store.stats().expect("shared stats");
    assert_eq!(stats.count, 1);
    assert_eq!(stats.total_bytes, SHARED_DATA.len() as u64);
}

#[test]
fn killed_writer_rolls_back_and_next_process_can_write_and_reopen() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("blobs");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");

    let committed_hash = compute_sha256(COMMITTED_DATA);
    let store = LmdbBlobStore::new(&db).expect("open LMDB store");
    assert!(store
        .put_sync(committed_hash, COMMITTED_DATA)
        .expect("seed committed blob"));
    store.force_sync().expect("sync committed blob");
    drop(store);

    let mut writer = spawn_helper("hold-uncommitted-write", &db, &control, "dead");
    wait_for(&control.join("dead-ready"));
    writer.kill().expect("kill writer with open transaction");
    let output = writer.wait_with_output().expect("reap killed writer");
    assert!(
        !output.status.success(),
        "killed writer must not exit successfully"
    );

    let store = LmdbBlobStore::new(&db).expect("reopen after writer death");
    assert_eq!(
        store
            .get_sync(&committed_hash)
            .expect("read committed blob"),
        Some(COMMITTED_DATA.to_vec())
    );
    let aborted_hash = compute_sha256(ABORTED_DATA);
    assert_eq!(
        store.get_sync(&aborted_hash).expect("read aborted blob"),
        None
    );
    assert!(store
        .put_sync(aborted_hash, ABORTED_DATA)
        .expect("write after writer death"));
    store.force_sync().expect("sync replacement write");
    drop(store);

    let reopened = LmdbBlobStore::new(&db).expect("second reopen after writer death");
    assert_eq!(
        reopened
            .get_sync(&aborted_hash)
            .expect("read replacement blob"),
        Some(ABORTED_DATA.to_vec())
    );
}

#[test]
fn committed_child_write_is_visible_after_process_exit_and_reopen() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("blobs");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");

    wait_success(
        spawn_helper("write-committed", &db, &control, "writer"),
        "committed child writer",
    );
    wait_success(
        spawn_helper("read-committed", &db, &control, "reader"),
        "reopened child reader",
    );
    let store = open_shared_lmdb_blob_store(&db, SHARED_BLOB_MIN_MAP_SIZE_BYTES)
        .expect("parent canonical reopen");
    let hash = compute_sha256(COMMITTED_DATA);
    assert_eq!(
        store.get_sync(&hash).expect("parent read"),
        Some(COMMITTED_DATA.to_vec())
    );
}

#[test]
fn concurrent_shared_openers_finish_an_incomplete_pool_initialization() {
    let temp = TempDir::new().expect("temp dir");
    let data_dir = temp.path().join("data");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");
    let empty = PoolStore::open(
        data_dir.join(SHARED_BLOB_POOL_DIR_NAME),
        PoolStoreConfig::default(),
    )
    .expect("create catalog before its initial member");
    assert!(empty.members().expect("empty members").is_empty());
    drop(empty);

    let openers = (0..4)
        .map(|id| spawn_helper("initialize-shared-pool", &data_dir, &control, id))
        .collect::<Vec<_>>();
    for id in 0..openers.len() {
        wait_for(&control.join(format!("{id}-ready")));
    }
    fs::write(control.join("go"), b"go").expect("release openers");
    for (id, child) in openers.into_iter().enumerate() {
        wait_success(child, &format!("shared opener {id}"));
    }

    let reopened = open_shared_lmdb_blob_store(&data_dir, SHARED_BLOB_MIN_MAP_SIZE_BYTES)
        .expect("reopen initialized shared pool");
    let ConfiguredLmdbBlobStore::Pool(pool) = reopened else {
        panic!("incomplete shared pool must remain a pool");
    };
    let members = pool.members().expect("initialized members");
    assert_eq!(
        members.len(),
        1,
        "exactly one initial member must be committed"
    );
    let marker = fs::read_to_string(members[0].path.join(".hashtree-pool-member-v1"))
        .expect("member marker");
    assert_eq!(marker.trim(), members[0].id.to_string());
    let hash = compute_sha256(SHARED_DATA);
    assert_eq!(
        pool.get_sync(&hash).expect("read concurrent write"),
        Some(SHARED_DATA.to_vec())
    );
}

#[test]
fn resized_environment_is_adopted_when_an_existing_process_reopens() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("blobs");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");

    wait_success(
        spawn_helper_with_env(
            "open-map",
            &db,
            &control,
            "small",
            &[(TEST_MAP_SIZE_ENV, (1024 * 1024).to_string())],
        ),
        "small-map opener",
    );
    wait_success(
        spawn_helper_with_env(
            "write-resized",
            &db,
            &control,
            "large",
            &[(TEST_MAP_SIZE_ENV, (8 * 1024 * 1024).to_string())],
        ),
        "large-map writer",
    );
    wait_success(
        spawn_helper_with_env(
            "read-resized",
            &db,
            &control,
            "reopened",
            &[(TEST_MAP_SIZE_ENV, (1024 * 1024).to_string())],
        ),
        "small-request reopen",
    );
}

#[test]
fn process_open_during_live_resize_must_exit_before_replacement_reopens() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("blobs");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");

    let stale = spawn_helper_with_env(
        "hold-small-map",
        &db,
        &control,
        "stale",
        &[(TEST_MAP_SIZE_ENV, (1024 * 1024).to_string())],
    );
    wait_for(&control.join("stale-ready"));
    wait_success(
        spawn_helper_with_env(
            "write-resized",
            &db,
            &control,
            "large",
            &[(TEST_MAP_SIZE_ENV, (8 * 1024 * 1024).to_string())],
        ),
        "live large-map writer",
    );
    fs::write(control.join("go"), b"go").expect("release stale map holder");
    wait_success(stale, "stale map holder");
    wait_success(
        spawn_helper_with_env(
            "read-resized",
            &db,
            &control,
            "replacement",
            &[(TEST_MAP_SIZE_ENV, (1024 * 1024).to_string())],
        ),
        "replacement after stale map holder exited",
    );
}

#[test]
fn pins_and_gc_remain_consistent_with_a_stale_process_handle() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("blobs");
    let control = temp.path().join("control");
    fs::create_dir_all(&control).expect("create control dir");

    let gc = spawn_helper_with_env(
        "stale-gc",
        &db,
        &control,
        "gc",
        &[(TEST_MAX_BYTES_ENV, "12".to_string())],
    );
    wait_for(&control.join("gc-ready"));
    wait_success(spawn_helper("put-pin", &db, &control, "owner"), "pin owner");
    fs::write(control.join("go"), b"go").expect("release stale GC handle");
    wait_success(gc, "stale GC process");

    let store = LmdbBlobStore::new(&db).expect("reopen after GC");
    let pinned_hash = compute_sha256(PINNED_DATA);
    let unpinned_hash = compute_sha256(UNPINNED_DATA);
    assert_eq!(
        store.get_sync(&pinned_hash).expect("read pinned blob"),
        Some(PINNED_DATA.to_vec()),
        "GC must preserve another process's pin"
    );
    assert_eq!(
        store.get_sync(&unpinned_hash).expect("read unpinned blob"),
        None,
        "the stale handle must account for the other process before GC"
    );
    assert_eq!(store.pin_count(&pinned_hash), 1);
    let stats = store.stats().expect("stats after cross-process GC");
    assert_eq!(stats.count, 1);
    assert_eq!(stats.total_bytes, PINNED_DATA.len() as u64);
    assert_eq!(stats.pinned_count, 1);
    assert_eq!(stats.pinned_bytes, PINNED_DATA.len() as u64);
}
