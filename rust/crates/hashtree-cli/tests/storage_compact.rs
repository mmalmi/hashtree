mod common;

use common::htree_bin;
use hashtree_core::store::Store;
use hashtree_lmdb::{compute_sha256, LmdbBlobStore};
use std::process::Command;

#[tokio::test]
async fn compact_handles_existing_large_map_size() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    let env_dir = data_dir.join("blobs");
    std::fs::create_dir_all(&env_dir).expect("env dir");

    let store = LmdbBlobStore::with_map_size(&env_dir, 64 * 1024 * 1024).expect("open lmdb store");
    let data = b"hello".to_vec();
    store
        .put(compute_sha256(&data), data)
        .await
        .expect("insert blob");
    drop(store);

    let before_bytes = std::fs::metadata(env_dir.join("data.mdb"))
        .expect("before data.mdb")
        .len();

    let output = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("storage")
        .arg("compact")
        .arg("--env-dir")
        .arg("blobs")
        .output()
        .expect("run htree storage compact");

    assert!(
        output.status.success(),
        "htree storage compact failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let after_bytes = std::fs::metadata(env_dir.join("data.mdb"))
        .expect("after data.mdb")
        .len();

    assert!(
        after_bytes < before_bytes,
        "expected compaction to shrink LMDB file"
    );
    assert!(!env_dir.join("data.mdb.compact").exists());
    assert!(!env_dir.join("data.mdb.bak").exists());
}

#[tokio::test]
async fn compact_uses_external_scratch_and_keeps_recovery_backup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    let env_dir = data_dir.join("blobs");
    let scratch_dir = tmp.path().join("scratch");
    std::fs::create_dir_all(&env_dir).expect("env dir");

    let store = LmdbBlobStore::with_map_size(&env_dir, 64 * 1024 * 1024).expect("open store");
    let data = b"external scratch compaction".to_vec();
    store
        .put(compute_sha256(&data), data)
        .await
        .expect("insert blob");
    drop(store);
    let before_bytes = std::fs::metadata(env_dir.join("data.mdb"))
        .expect("before data.mdb")
        .len();

    let output = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("storage")
        .arg("compact")
        .arg("--env-dir")
        .arg("blobs")
        .arg("--scratch-dir")
        .arg(&scratch_dir)
        .arg("--keep-backup")
        .output()
        .expect("run scratch compaction");
    assert!(
        output.status.success(),
        "scratch compaction failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let after_bytes = std::fs::metadata(env_dir.join("data.mdb"))
        .expect("after data.mdb")
        .len();
    assert!(after_bytes < before_bytes);
    assert!(!env_dir.join("data.mdb.compact-recovery").exists());
    assert!(!env_dir.join("data.mdb.replacement").exists());

    let runs = std::fs::read_dir(&scratch_dir)
        .expect("scratch directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("scratch runs");
    assert_eq!(runs.len(), 1);
    let backup = runs[0].path().join("data.mdb.bak");
    assert_eq!(
        std::fs::metadata(backup).expect("backup").len(),
        before_bytes
    );
    assert!(!runs[0].path().join("data.mdb.compact").exists());
}
