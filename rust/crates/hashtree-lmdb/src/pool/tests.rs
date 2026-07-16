use super::*;
use std::process::{Child, Command};
use std::thread;
use tempfile::TempDir;

const HELPER_MODE: &str = "HASHTREE_POOL_HELPER_MODE";
const HELPER_CATALOG: &str = "HASHTREE_POOL_HELPER_CATALOG";
const HELPER_READY: &str = "HASHTREE_POOL_HELPER_READY";
const HELPER_HASH: &str = "HASHTREE_POOL_HELPER_HASH";
const PENDING_DATA: &[u8] = b"pool pending crash recovery bytes";

#[test]
#[ignore = "subprocess entry point for pool crash recovery tests"]
fn pool_pending_helper() {
    let Ok(mode) = std::env::var(HELPER_MODE) else {
        return;
    };
    let catalog = PathBuf::from(std::env::var_os(HELPER_CATALOG).expect("catalog path"));
    let ready = PathBuf::from(std::env::var_os(HELPER_READY).expect("ready path"));
    let hash = hashtree_core::from_hex(&std::env::var(HELPER_HASH).expect("hash hex"))
        .expect("32-byte hash");
    let pool = PoolStore::open(catalog, PoolStoreConfig::default()).expect("open pool");
    let target = pool
        .choose_write_member(PENDING_DATA.len() as u64, None)
        .expect("choose member");
    let pending = LocationRecord::Pending {
        member: target,
        size: PENDING_DATA.len() as u64,
    };
    pool.reserve_if_absent(hash, pending)
        .expect("reserve pending location");
    if mode == "written" {
        let store = pool.get_member(target).expect("open target");
        pool.write_verified_member(target, &store, hash, PENDING_DATA)
            .expect("write pending member data");
    }
    fs::write(ready, b"ready").expect("write ready marker");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(unix)]
#[test]
fn process_death_before_or_after_member_write_recovers_pending_location() {
    for mode in ["reserved", "written"] {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open pool");
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("member"),
            1024 * 1024,
        ))
        .expect("add member");
        drop(pool);

        let ready = temp.path().join("ready");
        let hash = sha256(PENDING_DATA);
        let mut child = spawn_pending_helper(mode, &catalog, &ready, hash);
        wait_for_file(&ready, &mut child);
        child.kill().expect("kill pending helper");
        let output = child.wait_with_output().expect("reap pending helper");
        assert!(!output.status.success(), "helper must be killed");

        let recovered = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("reopen pool");
        let inserted = recovered
            .put_sync(hash, PENDING_DATA)
            .expect("recover pending put");
        assert_eq!(inserted, mode == "reserved");
        assert_eq!(
            recovered.get_sync(&hash).expect("read recovered blob"),
            Some(PENDING_DATA.to_vec())
        );
        assert!(matches!(
            recovered.read_location(&hash).expect("location"),
            Some(LocationRecord::Stored { .. })
        ));
    }
}

#[cfg(unix)]
fn spawn_pending_helper(mode: &str, catalog: &Path, ready: &Path, hash: Hash) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("pool::tests::pool_pending_helper")
        .env(HELPER_MODE, mode)
        .env(HELPER_CATALOG, catalog)
        .env(HELPER_READY, ready)
        .env(HELPER_HASH, hashtree_core::to_hex(&hash))
        .env("RUST_TEST_THREADS", "1")
        .spawn()
        .expect("spawn pending helper")
}

#[cfg(unix)]
fn wait_for_file(path: &Path, child: &mut Child) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll helper") {
            panic!("pool helper exited early: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}
