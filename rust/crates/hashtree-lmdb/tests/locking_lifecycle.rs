//! Run with `--features heed/posix-sem` on macOS to exercise the iOS backend.
#![cfg(all(unix, not(target_os = "ios")))]

use futures::executor::block_on;
use hashtree_core::{sha256, Store};
use hashtree_lmdb::LmdbBlobStore;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

const MAP_BYTES: usize = 16 * 1024 * 1024;
const DATA: &[u8] = b"durable ciphertext fixture";
const CHILD_ENV: &str = "HASHTREE_LOCKING_CHILD";
const PATH_ENV: &str = "HASHTREE_LOCKING_PATH";

fn open(path: &Path) -> LmdbBlobStore {
    LmdbBlobStore::with_exact_map_size_and_external_blob_options(path, MAP_BYTES, None)
        .expect("open bounded store with locking enabled")
}

struct BoundedChild(Child);

impl BoundedChild {
    fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.0.try_wait().expect("poll helper") {
                assert!(status.success(), "helper failed: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "LMDB helper deadlocked");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for BoundedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn child(mode: &str, path: &Path) -> BoundedChild {
    BoundedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "locking_child", "--ignored", "--nocapture"])
            .env(CHILD_ENV, mode)
            .env(PATH_ENV, path)
            .spawn()
            .expect("spawn isolated locking helper"),
    )
}

#[test]
#[ignore = "bounded subprocess entry point"]
fn locking_child() {
    let mode = std::env::var(CHILD_ENV).expect("child mode");
    let path = PathBuf::from(std::env::var_os(PATH_ENV).expect("child path"));
    let hash = sha256(DATA);
    match mode.as_str() {
        "write" => {
            let store = open(&path);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !path.join("go").exists() {
                assert!(Instant::now() < deadline, "start barrier timed out");
                thread::sleep(Duration::from_millis(5));
            }
            for _ in 0..32 {
                store.put_sync(hash, DATA).expect("idempotent write");
                block_on(store.pin(&hash)).expect("transactional pin");
            }
            store.force_sync().expect("durable pins and data");
        }
        "crash" => {
            let store = open(&path);
            store.put_sync(hash, DATA).unwrap();
            block_on(store.pin(&hash)).unwrap();
            store.force_sync().unwrap();
            drop(store);
            // Reopen with the same backend and die while owning its writer lock.
            // No other process has this environment open. _exit skips destructors.
            let env = unsafe {
                heed::EnvOpenOptions::new()
                    .map_size(MAP_BYTES)
                    .max_dbs(8)
                    .open(&path)
            }
            .unwrap();
            let mut txn = env.write_txn().unwrap();
            let db = env
                .create_database::<heed::types::Bytes, heed::types::Bytes>(
                    &mut txn,
                    Some("uncommitted-crash-fixture"),
                )
                .unwrap();
            db.put(&mut txn, b"partial", b"must not survive").unwrap();
            unsafe { libc::_exit(0) }
        }
        "recover" => {
            let store = open(&path);
            assert_eq!(store.get_sync(&hash).unwrap().as_deref(), Some(DATA));
            assert_eq!(store.pin_count(&hash), 1);
            assert_eq!(store.map_size_bytes(), MAP_BYTES);
            let next = b"write after abandoned writer";
            store.put_sync(sha256(next), next).unwrap();
            store.force_sync().unwrap();
            drop(store);
            let env = unsafe {
                heed::EnvOpenOptions::new()
                    .map_size(MAP_BYTES)
                    .max_dbs(8)
                    .open(&path)
            }
            .unwrap();
            let txn = env.read_txn().unwrap();
            assert!(env
                .open_database::<heed::types::Bytes, heed::types::Bytes>(
                    &txn,
                    Some("uncommitted-crash-fixture"),
                )
                .unwrap()
                .is_none());
        }
        _ => panic!("unknown helper mode"),
    }
}

#[test]
fn concurrent_process_writes_and_pins_are_serialized() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let store = open(&path);
    let mut children: Vec<_> = (0..4).map(|_| child("write", &path)).collect();
    std::fs::write(path.join("go"), b"start").unwrap();
    for child in &mut children {
        child.wait();
    }
    assert_eq!(
        store.get_sync(&sha256(DATA)).unwrap().as_deref(),
        Some(DATA)
    );
    assert_eq!(store.pin_count(&sha256(DATA)), 128);
    assert_eq!(store.stats().unwrap().count, 1);
    assert_eq!(store.map_size_bytes(), MAP_BYTES);
    drop(store);
    let reopened = open(&path);
    assert_eq!(reopened.pin_count(&sha256(DATA)), 128);
}

#[test]
fn last_process_crash_reopens_with_durable_data_and_fresh_locks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    child("crash", &path).wait();
    child("recover", &path).wait();
}
