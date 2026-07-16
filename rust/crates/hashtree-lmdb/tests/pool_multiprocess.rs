use hashtree_core::sha256;
use hashtree_lmdb::{PoolMemberConfig, PoolStore, PoolStoreConfig};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

const MODE_ENV: &str = "HASHTREE_POOL_PROCESS_MODE";
const CATALOG_ENV: &str = "HASHTREE_POOL_PROCESS_CATALOG";
const CONTROL_ENV: &str = "HASHTREE_POOL_PROCESS_CONTROL";
const ID_ENV: &str = "HASHTREE_POOL_PROCESS_ID";
const SHARED_DATA: &[u8] = b"multiprocess adaptive pool bytes";
const REFRESH_DATA: &[u8] = b"write after another process adds storage";

#[test]
#[ignore = "subprocess entry point for pool multiprocess tests"]
fn pool_process_helper() {
    let Ok(mode) = std::env::var(MODE_ENV) else {
        return;
    };
    let catalog = PathBuf::from(std::env::var_os(CATALOG_ENV).expect("catalog path"));
    let control = PathBuf::from(std::env::var_os(CONTROL_ENV).expect("control path"));
    let id = std::env::var(ID_ENV).expect("helper id");
    let pool = PoolStore::open(catalog, PoolStoreConfig::default()).expect("open shared pool");
    fs::write(control.join(format!("{id}-ready")), b"ready").expect("write ready");
    wait_for(&control.join("go"));

    match mode.as_str() {
        "put" => {
            let inserted = pool
                .put_sync(sha256(SHARED_DATA), SHARED_DATA)
                .expect("shared put");
            fs::write(
                control.join(format!("{id}-result")),
                if inserted { "inserted" } else { "existing" },
            )
            .expect("write result");
        }
        "refresh" => {
            assert_eq!(pool.members().expect("refreshed members").len(), 2);
            let hash = sha256(REFRESH_DATA);
            assert!(pool.put_sync(hash, REFRESH_DATA).expect("refreshed put"));
            let location = pool
                .blob_location(&hash)
                .expect("location read")
                .expect("location");
            fs::write(control.join(format!("{id}-result")), location.to_string())
                .expect("write location");
        }
        "pin" => {
            let hash = sha256(SHARED_DATA);
            pool.pin_sync(&hash).expect("shared pin");
            fs::write(control.join(format!("{id}-result")), b"pinned").expect("write result");
        }
        other => panic!("unknown helper mode {other}"),
    }
}

#[test]
fn concurrent_process_writes_are_pool_wide_idempotent() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let control = temp.path().join("control");
    fs::create_dir(&control).expect("control dir");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("member"),
        1024 * 1024,
    ))
    .expect("add member");
    drop(pool);

    let children = (0..4)
        .map(|id| spawn_helper("put", &catalog, &control, id.to_string()))
        .collect::<Vec<_>>();
    for id in 0..children.len() {
        wait_for(&control.join(format!("{id}-ready")));
    }
    fs::write(control.join("go"), b"go").expect("release helpers");
    for (id, child) in children.into_iter().enumerate() {
        wait_success(child, &format!("writer {id}"));
    }
    let inserted = (0..4)
        .filter(|id| {
            fs::read_to_string(control.join(format!("{id}-result"))).expect("result") == "inserted"
        })
        .count();
    assert_eq!(inserted, 1);

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("reopen pool");
    let hash = sha256(SHARED_DATA);
    assert_eq!(
        reopened.get_sync(&hash).expect("read shared blob"),
        Some(SHARED_DATA.to_vec())
    );
    assert_eq!(reopened.stats().expect("pool stats").count, 1);
}

#[test]
fn process_open_before_member_add_refreshes_manifest_and_placement() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let control = temp.path().join("control");
    fs::create_dir(&control).expect("control dir");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open pool");
    let seed = b"fill the first member".repeat(32);
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("first"),
        seed.len() as u64,
    ))
    .expect("add first");
    pool.put_sync(sha256(&seed), &seed).expect("fill first");

    let child = spawn_helper("refresh", &catalog, &control, "refresh".to_string());
    wait_for(&control.join("refresh-ready"));
    let second = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("second"),
            1024 * 1024,
        ))
        .expect("add second");
    fs::write(control.join("go"), b"go").expect("release helper");
    wait_success(child, "refresh helper");
    let location = fs::read_to_string(control.join("refresh-result")).expect("location result");
    assert_eq!(location, second.to_string());
    assert_eq!(
        pool.get_sync(&sha256(REFRESH_DATA)).expect("parent read"),
        Some(REFRESH_DATA.to_vec())
    );
}

#[test]
fn concurrent_process_pins_are_catalog_owned_and_exact() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let control = temp.path().join("control");
    fs::create_dir(&control).expect("control dir");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("member"),
        1024 * 1024,
    ))
    .expect("add member");
    pool.put_sync(sha256(SHARED_DATA), SHARED_DATA)
        .expect("seed shared blob");
    drop(pool);

    let children = (0..4)
        .map(|id| spawn_helper("pin", &catalog, &control, format!("pin-{id}")))
        .collect::<Vec<_>>();
    for id in 0..children.len() {
        wait_for(&control.join(format!("pin-{id}-ready")));
    }
    fs::write(control.join("go"), b"go").expect("release helpers");
    for (id, child) in children.into_iter().enumerate() {
        wait_success(child, &format!("pin writer {id}"));
    }

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("reopen pool");
    assert_eq!(
        reopened
            .pin_count_sync(&sha256(SHARED_DATA))
            .expect("pin count"),
        4
    );
    let stats = reopened.stats().expect("pool stats");
    assert_eq!(stats.pinned_count, 1);
    assert_eq!(stats.pinned_bytes, SHARED_DATA.len() as u64);
}

fn spawn_helper(mode: &str, catalog: &Path, control: &Path, id: String) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("pool_process_helper")
        .env(MODE_ENV, mode)
        .env(CATALOG_ENV, catalog)
        .env(CONTROL_ENV, control)
        .env(ID_ENV, id)
        .env("RUST_TEST_THREADS", "1")
        .spawn()
        .expect("spawn pool helper")
}

fn wait_for(path: &Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

fn wait_success(child: Child, label: &str) {
    let output = child.wait_with_output().expect("wait helper");
    assert!(
        output.status.success(),
        "{label} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
