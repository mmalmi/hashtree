use hashtree_core::sha256;
use hashtree_lmdb::{
    migrate_lmdb_batch, migrate_lmdb_batch_with_max_buffer_bytes, ExternalBlobOptions,
    LmdbBlobReader, LmdbBlobStore, PoolMemberConfig, PoolStore, PoolStoreConfig,
};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

const SOURCE_ENV: &str = "HASHTREE_READER_SOURCE";
const EXTERNAL_ENV: &str = "HASHTREE_READER_EXTERNAL";
const CONTROL_ENV: &str = "HASHTREE_READER_CONTROL";

fn inline_data() -> Vec<u8> {
    b"read-only inline migration data".repeat(32)
}

fn external_data() -> Vec<u8> {
    b"read-only external migration data".repeat(256)
}

fn external_options(path: PathBuf) -> ExternalBlobOptions {
    ExternalBlobOptions {
        base_path: path,
        min_bytes: 1024,
        sync: true,
        pack_target_bytes: Some(64 * 1024),
    }
}

#[test]
#[ignore = "subprocess entry point for read-only reader tests"]
fn read_only_reader_helper() {
    let Some(source) = std::env::var_os(SOURCE_ENV) else {
        return;
    };
    let external = PathBuf::from(std::env::var_os(EXTERNAL_ENV).expect("external path"));
    let store = LmdbBlobStore::with_map_size_and_external_blob_options(
        source,
        64 * 1024 * 1024,
        Some(external_options(external)),
    )
    .expect("open source writer");
    let inline = inline_data();
    let external = external_data();
    assert!(
        store
            .put_many_sync(&[(sha256(&inline), inline), (sha256(&external), external)])
            .expect("write source batch")
            > 0
    );
    store.force_sync().expect("sync source");
    if let Some(control) = std::env::var_os(CONTROL_ENV).map(PathBuf::from) {
        std::fs::write(control.join("ready"), b"ready").expect("writer ready");
        while !control.join("stop").exists() {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn read_only_reader_adopts_existing_map_and_reads_inline_and_external_blobs() {
    let temp = TempDir::new().expect("temp dir");
    let source = temp.path().join("source");
    let external = temp.path().join("external");
    let control = temp.path().join("control");
    std::fs::create_dir(&control).expect("control dir");
    let mut writer = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("read_only_reader_helper")
        .env(SOURCE_ENV, &source)
        .env(EXTERNAL_ENV, &external)
        .env(CONTROL_ENV, &control)
        .env("RUST_TEST_THREADS", "1")
        .spawn()
        .expect("run source helper");
    for _ in 0..300 {
        if control.join("ready").exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        control.join("ready").exists(),
        "writer did not become ready"
    );

    let reader = LmdbBlobReader::open(&source, Some(external_options(external)))
        .expect("open read-only source");
    let inline = inline_data();
    let external = external_data();
    assert_eq!(
        reader.get_sync(&sha256(&inline)).expect("inline read"),
        Some(inline)
    );
    assert_eq!(
        reader.get_sync(&sha256(&external)).expect("external read"),
        Some(external)
    );
    assert_eq!(reader.scan_hashes_after(None, 8).expect("scan").len(), 2);
    assert!(reader.map_size_bytes() >= 64 * 1024 * 1024);

    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open target pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("target"),
        64 * 1024 * 1024,
    ))
    .expect("add target");
    let first = migrate_lmdb_batch_with_max_buffer_bytes(&reader, &pool, None, 2, 1024)
        .expect("byte-bounded migration batch");
    assert_eq!(first.verified, 2);
    assert_eq!(first.inserted, 2);
    assert_eq!(first.write_batches, 2);
    assert_eq!(
        first.peak_buffered_bytes,
        external_data().len() as u64,
        "migration should retain at most one oversized blob"
    );
    let replay = migrate_lmdb_batch(&reader, &pool, None, 2).expect("replay migration batch");
    assert_eq!(replay.scanned, 2);
    assert_eq!(replay.already_present, 2);
    assert_eq!(replay.verified, 0);
    assert_eq!(replay.inserted, 0);
    assert_eq!(replay.write_batches, 0);
    assert_eq!(replay.peak_buffered_bytes, 0);
    let complete = migrate_lmdb_batch(&reader, &pool, first.last_hash, 1).expect("complete scan");
    assert!(complete.source_exhausted);
    assert_eq!(pool.stats().expect("pool stats").count, 2);
    std::fs::write(control.join("stop"), b"stop").expect("stop writer");
    assert!(writer.wait().expect("wait writer").success());
}
