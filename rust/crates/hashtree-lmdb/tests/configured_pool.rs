use hashtree_core::sha256;
use hashtree_lmdb::{
    open_configured_lmdb_blob_store, open_shared_lmdb_blob_store, ConfiguredLmdbBlobStore,
    SHARED_BLOB_MIN_MAP_SIZE_BYTES,
};
use tempfile::TempDir;

const SINGLE_PATH_ENV: &str = "HASHTREE_CONFIGURED_SINGLE_PATH";
const SINGLE_DATA: &[u8] = b"existing single LMDB bytes";

#[test]
#[ignore = "subprocess entry point for configured-store tests"]
fn configured_pool_helper() {
    let Some(path) = std::env::var_os(SINGLE_PATH_ENV) else {
        return;
    };
    let single = open_configured_lmdb_blob_store(path, Some(SHARED_BLOB_MIN_MAP_SIZE_BYTES * 4))
        .expect("create single store");
    assert!(single
        .put_sync(sha256(SINGLE_DATA), SINGLE_DATA)
        .expect("single write"));
}

#[test]
fn fresh_shared_store_uses_and_reopens_one_pool_member() {
    let temp = TempDir::new().expect("temp dir");
    let data = b"fresh shared pool bytes".repeat(32);
    let hash = sha256(&data);
    let configured = open_shared_lmdb_blob_store(temp.path(), SHARED_BLOB_MIN_MAP_SIZE_BYTES * 4)
        .expect("open fresh shared store");
    let ConfiguredLmdbBlobStore::Pool(pool) = configured else {
        panic!("fresh shared storage must initialize a pool");
    };
    assert_eq!(pool.members().expect("members").len(), 1);
    assert!(pool.put_sync(hash, &data).expect("pool write"));
    drop(pool);

    let reopened = open_shared_lmdb_blob_store(temp.path(), SHARED_BLOB_MIN_MAP_SIZE_BYTES * 4)
        .expect("reopen shared pool");
    assert!(matches!(reopened, ConfiguredLmdbBlobStore::Pool(_)));
    assert_eq!(reopened.get_sync(&hash).expect("pool read"), Some(data));
}

#[test]
fn existing_single_lmdb_is_not_silently_reclassified_as_a_pool() {
    let temp = TempDir::new().expect("temp dir");
    let blob_path = temp.path().join("blobs");
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("configured_pool_helper")
        .env(SINGLE_PATH_ENV, &blob_path)
        .env("RUST_TEST_THREADS", "1")
        .status()
        .expect("run single-store helper");
    assert!(status.success());
    let hash = sha256(SINGLE_DATA);

    let configured = open_shared_lmdb_blob_store(temp.path(), SHARED_BLOB_MIN_MAP_SIZE_BYTES * 4)
        .expect("open existing single store");
    assert!(matches!(configured, ConfiguredLmdbBlobStore::Single(_)));
    assert_eq!(
        configured.get_sync(&hash).expect("single read"),
        Some(SINGLE_DATA.to_vec())
    );
}
