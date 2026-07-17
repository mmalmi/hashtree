use hashtree_core::Store;
use hashtree_lmdb::{
    compute_sha256, open_shared_lmdb_blob_store, LmdbBlobStore, SHARED_BLOB_MIN_MAP_SIZE_BYTES,
};
use heed::types::Bytes;
use heed::{Database, EnvOpenOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELPER_MODE_ENV: &str = "HASHTREE_LMDB_MULTIPROCESS_HELPER_MODE";
const DB_PATH_ENV: &str = "HASHTREE_LMDB_MULTIPROCESS_DB_PATH";
const CONTROL_PATH_ENV: &str = "HASHTREE_LMDB_MULTIPROCESS_CONTROL_PATH";
const HELPER_ID_ENV: &str = "HASHTREE_LMDB_MULTIPROCESS_HELPER_ID";
pub const TEST_MAP_SIZE_ENV: &str = "HASHTREE_LMDB_MULTIPROCESS_MAP_SIZE";
pub const TEST_MAX_BYTES_ENV: &str = "HASHTREE_LMDB_MULTIPROCESS_MAX_BYTES";
const HELPER_TIMEOUT: Duration = Duration::from_secs(20);

pub const SHARED_DATA: &[u8] = b"one immutable blob shared by several writers";
pub const COMMITTED_DATA: &[u8] = b"committed before the other writer died";
pub const ABORTED_DATA: &[u8] = b"uncommitted transaction from a killed writer";
pub const PINNED_DATA: &[u8] = b"pinned-1";
pub const UNPINNED_DATA: &[u8] = b"evict-me";

fn path_env(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("missing {name}")))
}

fn helper_id() -> String {
    std::env::var(HELPER_ID_ENV).unwrap_or_else(|_| "helper".to_string())
}

fn marker(control: &Path, name: &str) -> PathBuf {
    control.join(format!("{}-{name}", helper_id()))
}

pub fn wait_for(path: &Path) {
    let deadline = Instant::now() + HELPER_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn spawn_helper(mode: &str, db: &Path, control: &Path, id: impl ToString) -> Child {
    spawn_helper_with_env(mode, db, control, id, &[])
}

pub fn spawn_helper_with_env(
    mode: &str,
    db: &Path,
    control: &Path,
    id: impl ToString,
    env: &[(&str, String)],
) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("multiprocess test binary path"));
    command
        .arg("--ignored")
        .arg("--exact")
        .arg("support::multiprocess_helper")
        .env(HELPER_MODE_ENV, mode)
        .env(DB_PATH_ENV, db)
        .env(CONTROL_PATH_ENV, control)
        .env(HELPER_ID_ENV, id.to_string())
        .env("RUST_TEST_THREADS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    command.spawn().expect("spawn LMDB multiprocess helper")
}

fn assert_success(output: Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn wait_success(child: Child, context: &str) {
    assert_success(child.wait_with_output().expect("wait for helper"), context);
}

#[tokio::test]
#[ignore = "subprocess entry point for LMDB multiprocess tests"]
async fn multiprocess_helper() {
    let Some(mode) = std::env::var_os(HELPER_MODE_ENV) else {
        return;
    };
    let mode = mode.to_string_lossy();
    let db = path_env(DB_PATH_ENV);
    let control = path_env(CONTROL_PATH_ENV);

    match mode.as_ref() {
        "read-shared" => {
            let store = LmdbBlobStore::new(&db).expect("reader open");
            fs::write(marker(&control, "ready"), b"ready").expect("reader ready");
            wait_for(&control.join("go"));
            let hash = compute_sha256(SHARED_DATA);
            for _ in 0..128 {
                let data = store
                    .get_sync(&hash)
                    .expect("reader get")
                    .expect("shared blob");
                assert_eq!(compute_sha256(&data), hash);
                assert_eq!(data, SHARED_DATA);
            }
        }
        "write-shared" => {
            let store = LmdbBlobStore::new(&db).expect("writer open");
            fs::write(marker(&control, "ready"), b"ready").expect("writer ready");
            wait_for(&control.join("go"));
            let hash = compute_sha256(SHARED_DATA);
            assert_eq!(compute_sha256(SHARED_DATA), hash);
            let inserted = store.put_sync(hash, SHARED_DATA).expect("shared put");
            fs::write(
                marker(&control, "result"),
                if inserted { b"inserted" } else { b"existing" },
            )
            .expect("write insertion result");
        }
        "hold-uncommitted-write" => {
            fs::create_dir_all(&db).expect("create raw LMDB dir");
            let env = unsafe {
                EnvOpenOptions::new()
                    .map_size(64 * 1024 * 1024)
                    .max_dbs(5)
                    .open(&db)
                    .expect("raw LMDB open")
            };
            let mut wtxn = env.write_txn().expect("raw write transaction");
            let blobs: Database<Bytes, Bytes> = env
                .open_database(&wtxn, Some("blobs"))
                .expect("open blobs database")
                .expect("blobs database");
            let hash = compute_sha256(ABORTED_DATA);
            blobs
                .put(&mut wtxn, &hash, ABORTED_DATA)
                .expect("uncommitted put");
            fs::write(marker(&control, "ready"), b"ready").expect("writer ready");
            loop {
                thread::park();
            }
        }
        "write-committed" => {
            let store = open_shared_lmdb_blob_store(&db, SHARED_BLOB_MIN_MAP_SIZE_BYTES)
                .expect("committed writer canonical open");
            let hash = compute_sha256(COMMITTED_DATA);
            assert!(store.put_sync(hash, COMMITTED_DATA).expect("committed put"));
        }
        "read-committed" => {
            let store = open_shared_lmdb_blob_store(&db, SHARED_BLOB_MIN_MAP_SIZE_BYTES)
                .expect("committed reader canonical reopen");
            let hash = compute_sha256(COMMITTED_DATA);
            assert_eq!(
                store.get_sync(&hash).expect("committed reader get"),
                Some(COMMITTED_DATA.to_vec())
            );
        }
        "initialize-shared-pool" => {
            fs::write(marker(&control, "ready"), b"ready").expect("opener ready");
            wait_for(&control.join("go"));
            let store = open_shared_lmdb_blob_store(&db, SHARED_BLOB_MIN_MAP_SIZE_BYTES)
                .expect("concurrent canonical open");
            let hash = compute_sha256(SHARED_DATA);
            let _ = store
                .put_sync(hash, SHARED_DATA)
                .expect("concurrent canonical write");
        }
        "open-map" => {
            let map_size = env_usize(TEST_MAP_SIZE_ENV);
            let store = LmdbBlobStore::with_map_size(&db, map_size).expect("open requested map");
            assert!(store.map_size_bytes() >= map_size);
        }
        "write-resized" => {
            let map_size = env_usize(TEST_MAP_SIZE_ENV);
            let store = LmdbBlobStore::with_map_size(&db, map_size).expect("resize map");
            let data = resized_data();
            let hash = compute_sha256(&data);
            assert!(store.put_sync(hash, &data).expect("write resized blob"));
            store.force_sync().expect("sync resized blob");
        }
        "read-resized" => {
            let map_size = env_usize(TEST_MAP_SIZE_ENV);
            let existing_size = fs::metadata(db.join("data.mdb"))
                .expect("resized data file")
                .len();
            let store = LmdbBlobStore::with_map_size(&db, map_size).expect("reopen resized map");
            assert!(store.map_size_bytes() as u64 >= existing_size);
            let data = resized_data();
            let hash = compute_sha256(&data);
            assert_eq!(
                store.get_sync(&hash).expect("read resized blob"),
                Some(data)
            );
        }
        "hold-small-map" => {
            let map_size = env_usize(TEST_MAP_SIZE_ENV);
            let store = LmdbBlobStore::with_map_size(&db, map_size).expect("open small map");
            fs::write(marker(&control, "ready"), b"ready").expect("small map ready");
            wait_for(&control.join("go"));
            let data = resized_data();
            let hash = compute_sha256(&data);
            let error = store
                .get_sync(&hash)
                .expect_err("a live holder cannot adopt another process's larger map safely");
            assert!(
                error.to_string().contains("MDB_MAP_RESIZED")
                    || error.to_string().contains("MapResized"),
                "unexpected stale-map error: {error}"
            );
        }
        "put-pin" => {
            let store = LmdbBlobStore::new(&db).expect("pin owner open");
            let hash = compute_sha256(PINNED_DATA);
            assert!(store.put_sync(hash, PINNED_DATA).expect("put pinned blob"));
            store.pin(&hash).await.expect("pin blob");
        }
        "stale-gc" => {
            let max_bytes = env_usize(TEST_MAX_BYTES_ENV) as u64;
            let store = LmdbBlobStore::with_max_bytes(&db, max_bytes).expect("stale GC open");
            fs::write(marker(&control, "ready"), b"ready").expect("GC ready");
            wait_for(&control.join("go"));
            let hash = compute_sha256(UNPINNED_DATA);
            assert!(store
                .put_sync(hash, UNPINNED_DATA)
                .expect("put unpinned blob"));
            let _ = store.evict_if_needed().await.expect("explicit GC");
        }
        other => panic!("unknown multiprocess helper mode: {other}"),
    }
}

fn env_usize(name: &str) -> usize {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("missing {name}"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be numeric"))
}

fn resized_data() -> Vec<u8> {
    vec![42u8; 2 * 1024 * 1024]
}
