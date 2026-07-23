mod common;

use common::htree_bin;
use hashtree_core::sha256;
use hashtree_lmdb::{
    ExternalBlobOptions, LmdbBlobStore, PoolMemberConfig, PoolStore, PoolStoreConfig,
    SHARED_BLOB_POOL_DIR_NAME,
};
use std::process::Command;

#[test]
fn migration_reopens_live_mappings_and_completes_external_blob_copy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_dir = temp.path().join("source");
    let source_external = temp.path().join("source-external");
    let data_dir = temp.path().join("target-data");
    let member_dir = temp.path().join("target-member");
    let member_external = temp.path().join("target-external");
    let cursor = temp.path().join("migration.cursor");

    let source = LmdbBlobStore::with_map_size_and_external_blob_options(
        &source_dir,
        64 * 1024 * 1024,
        Some(ExternalBlobOptions {
            base_path: source_external.clone(),
            min_bytes: 1,
            sync: true,
            pack_target_bytes: None,
        }),
    )
    .expect("open source");
    let blobs = (0..5u8)
        .map(|value| {
            let bytes = vec![value; 4 * 1024 + usize::from(value)];
            (sha256(&bytes), bytes)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        source.put_many_sync(&blobs).expect("populate source"),
        blobs.len()
    );
    source.force_sync().expect("sync source");
    drop(source);

    let pool = PoolStore::open(
        data_dir.join(SHARED_BLOB_POOL_DIR_NAME),
        PoolStoreConfig::default(),
    )
    .expect("open pool");
    pool.add_member(
        PoolMemberConfig::new(member_dir, 64 * 1024 * 1024).with_external_blobs(
            member_external,
            1,
            true,
            None,
        ),
    )
    .expect("add member");
    drop(pool);

    let output = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("storage")
        .arg("pool")
        .arg("migrate-lmdb")
        .arg("--source")
        .arg(&source_dir)
        .arg("--source-external-dir")
        .arg(&source_external)
        .arg("--state-file")
        .arg(&cursor)
        .arg("--batch-size")
        .arg("1")
        .arg("--reopen-batches")
        .arg("2")
        .output()
        .expect("run production migration command");
    assert!(
        output.status.success(),
        "migration failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert_eq!(
        stdout.matches("Migration mappings reopened").count(),
        2,
        "five one-item batches should close and reopen the live LMDB mappings twice\n{stdout}"
    );
    assert!(
        stdout
            .contains("Migration pass: scanned 5, already present 0, verified 5, inserted 5 blobs"),
        "unexpected migration report\n{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).expect("cursor"),
        "complete\n"
    );

    let reopened = PoolStore::open(
        data_dir.join(SHARED_BLOB_POOL_DIR_NAME),
        PoolStoreConfig::default(),
    )
    .expect("reopen pool");
    let stats = reopened.stats().expect("pool stats");
    assert_eq!(stats.count, blobs.len() as u64);
    assert_eq!(
        stats.bytes,
        blobs
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>()
    );
}
