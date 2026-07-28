use hashtree_core::{sha256, to_hex};
use hashtree_lmdb::{
    migrate_lmdb_batch, migrate_lmdb_batch_with_max_buffer_bytes,
    reconcile_lmdb_source_entries_with_max_buffer_bytes_and_authorizer,
    reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer, ExternalBlobOptions,
    LmdbBlobReader, LmdbBlobStore, PoolMemberConfig, PoolStore, PoolStoreConfig,
};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

const SOURCE_ENV: &str = "HASHTREE_READER_SOURCE";
const EXTERNAL_ENV: &str = "HASHTREE_READER_EXTERNAL";
const CONTROL_ENV: &str = "HASHTREE_READER_CONTROL";
const FD_SOURCE_ENV: &str = "HASHTREE_READER_FD_SOURCE";
const FD_EXTERNAL_ENV: &str = "HASHTREE_READER_FD_EXTERNAL";
const CANCEL_SOURCE_ENV: &str = "HASHTREE_READER_CANCEL_SOURCE";
const CANCEL_EXTERNAL_ENV: &str = "HASHTREE_READER_CANCEL_EXTERNAL";

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

fn unpacked_external_options(path: PathBuf) -> ExternalBlobOptions {
    ExternalBlobOptions {
        base_path: path,
        min_bytes: 1,
        sync: false,
        pack_target_bytes: None,
    }
}

fn unpacked_external_path(base: &std::path::Path, hash: &[u8; 32]) -> PathBuf {
    let hex = to_hex(hash);
    base.join(&hex[..2]).join(&hex[2..4]).join(&hex[4..])
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
#[ignore = "subprocess entry point for bounded-fd reader tests"]
fn concurrent_external_reader_fd_helper() {
    let Some(source) = std::env::var_os(FD_SOURCE_ENV) else {
        return;
    };
    let external = PathBuf::from(std::env::var_os(FD_EXTERNAL_ENV).expect("external path"));
    let reader = LmdbBlobReader::open_with_external_read_concurrency(
        source,
        Some(unpacked_external_options(external)),
        4,
    )
    .expect("open concurrent reader");
    let hashes = reader.scan_hashes_after(None, 512).expect("scan source");
    let values = reader
        .read_hashes_bounded(&hashes, u64::MAX)
        .expect("read under a low fd limit");
    assert_eq!(values.len(), hashes.len());
    assert_eq!(
        values.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(),
        hashes
    );
}

#[test]
#[ignore = "subprocess entry point for read cancellation tests"]
fn concurrent_external_reader_cancel_helper() {
    let Some(source) = std::env::var_os(CANCEL_SOURCE_ENV) else {
        return;
    };
    let external = PathBuf::from(std::env::var_os(CANCEL_EXTERNAL_ENV).expect("external path"));
    let reader = LmdbBlobReader::open_with_external_read_concurrency(
        source,
        Some(unpacked_external_options(external)),
        1,
    )
    .expect("open cancellation reader");
    let hashes = reader.scan_hashes_after(None, 512).expect("scan source");
    assert!(
        reader.read_hashes_bounded(&hashes, u64::MAX).is_err(),
        "missing first file must fail the page"
    );
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
    assert_eq!(
        replay.verified, 2,
        "replay must hash-verify source and compare committed target bytes"
    );
    assert_eq!(replay.inserted, 0);
    assert_eq!(replay.write_batches, 0);
    assert_eq!(
        replay.peak_buffered_bytes,
        (inline_data().len() + external_data().len()) as u64,
        "unbounded replay verifies both source payloads in one batch"
    );
    let complete = migrate_lmdb_batch(&reader, &pool, first.last_hash, 1).expect("complete scan");
    assert!(complete.source_exhausted);
    assert_eq!(pool.stats().expect("pool stats").count, 2);
    std::fs::write(control.join("stop"), b"stop").expect("stop writer");
    assert!(writer.wait().expect("wait writer").success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generated_reader_survives_tokio_worker_migration_without_bad_rslot() {
    let temp = TempDir::new().expect("generated multithread reader root");
    let source = temp.path().join("source");
    let store = LmdbBlobStore::new(&source).expect("open generated source");
    let entries = (0..128u32)
        .map(|index| {
            let bytes = format!("generated multithread LMDB body {index:08}").into_bytes();
            (sha256(&bytes), bytes)
        })
        .collect::<Vec<_>>();
    store
        .put_many_sync(&entries)
        .expect("write generated multithread corpus");
    store
        .force_sync()
        .expect("sync generated multithread corpus");
    drop(store);

    let reader = Arc::new(LmdbBlobReader::open(&source, None).expect("open production reader"));
    let mut tasks = Vec::new();
    for task_index in 0..32usize {
        let reader = Arc::clone(&reader);
        let entries = entries.clone();
        tasks.push(tokio::spawn(async move {
            for round in 0..256usize {
                tokio::task::yield_now().await;
                let (hash, expected) = &entries[(round + task_index) % entries.len()];
                assert_eq!(
                    reader.get_sync(hash).expect("Tokio worker LMDB read"),
                    Some(expected.clone())
                );
                if round % 16 == 0 {
                    assert_eq!(
                        reader
                            .scan_hashes_after(None, entries.len())
                            .expect("Tokio worker LMDB scan")
                            .len(),
                        entries.len()
                    );
                }
            }
        }));
    }
    for task in tasks {
        task.await.expect("multithread LMDB reader task");
    }
}

#[test]
fn receipt_reconciliation_reads_only_target_missing_source_bodies() {
    let temp = TempDir::new().expect("temp dir");
    let source = temp.path().join("source");
    let external = temp.path().join("external");
    let options = unpacked_external_options(external.clone());
    let source_store = LmdbBlobStore::with_map_size_and_external_blob_options(
        &source,
        64 * 1024 * 1024,
        Some(options.clone()),
    )
    .expect("open generated source");
    let stored_data = b"already Stored in the target".repeat(128);
    let missing_data = b"absent from the target".repeat(128);
    let stored_hash = sha256(&stored_data);
    let missing_hash = sha256(&missing_data);
    source_store
        .put_many_sync(&[
            (stored_hash, stored_data.clone()),
            (missing_hash, missing_data.clone()),
        ])
        .expect("write generated source data");
    source_store.force_sync().expect("sync generated source");
    drop(source_store);

    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open target Pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("member"),
        64 * 1024 * 1024,
    ))
    .expect("add target member");
    pool.put_sync(stored_hash, &stored_data)
        .expect("prepopulate exact Stored target");

    let stored_source_path = unpacked_external_path(&external, &stored_hash);
    std::fs::write(&stored_source_path, vec![0xa5; stored_data.len()])
        .expect("replace Stored source body with same-size corruption");
    let reader = LmdbBlobReader::open(&source, Some(options)).expect("open source reader");
    let mut source_evidence = vec![
        (stored_hash, stored_data.len() as u64),
        (missing_hash, missing_data.len() as u64),
    ];
    source_evidence.sort_unstable_by_key(|(hash, _)| *hash);
    let mut authorizations = Vec::new();
    let batch = reconcile_lmdb_source_entries_with_max_buffer_bytes_and_authorizer(
        &reader,
        &pool,
        &source_evidence,
        usize::MAX,
        &mut |cursor, writes| {
            authorizations.push((cursor, writes));
            Ok(())
        },
    )
    .expect("reconcile without reading exact Stored source body");

    assert_eq!(batch.scanned, 2);
    assert_eq!(batch.source_entries.len(), 2);
    assert_eq!(batch.already_present, 1);
    assert_eq!(batch.verified, 1);
    assert_eq!(batch.inserted, 1);
    assert_eq!(authorizations, vec![(Some(missing_hash), 1)]);
    assert_eq!(
        pool.get_sync(&stored_hash).expect("read Stored target"),
        Some(stored_data)
    );
    assert_eq!(
        pool.get_sync(&missing_hash)
            .expect("read reconciled target"),
        Some(missing_data)
    );
}

#[test]
fn interleaved_receipt_union_groups_source_reads_per_page() {
    let temp = TempDir::new().expect("temp dir");
    let first_path = temp.path().join("first-source");
    let second_path = temp.path().join("second-source");
    let first_store =
        LmdbBlobStore::with_map_size(&first_path, 64 * 1024 * 1024).expect("open first source");
    let second_store =
        LmdbBlobStore::with_map_size(&second_path, 64 * 1024 * 1024).expect("open second source");
    let mut generated = (0..128)
        .map(|index| {
            let bytes = format!("generated alternating source body {index:04}")
                .repeat(8)
                .into_bytes();
            (sha256(&bytes), bytes)
        })
        .collect::<Vec<_>>();
    generated.sort_unstable_by_key(|(hash, _)| *hash);
    let first_items = generated
        .iter()
        .enumerate()
        .filter(|(index, _)| index % 2 == 0)
        .map(|(_, item)| item.clone())
        .collect::<Vec<_>>();
    let second_items = generated
        .iter()
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, item)| item.clone())
        .collect::<Vec<_>>();
    first_store
        .put_many_sync(&first_items)
        .expect("populate first source");
    second_store
        .put_many_sync(&second_items)
        .expect("populate second source");
    first_store.force_sync().expect("sync first source");
    second_store.force_sync().expect("sync second source");
    drop(first_store);
    drop(second_store);

    let first_reader = LmdbBlobReader::open(&first_path, None).expect("open first reader");
    let second_reader = LmdbBlobReader::open(&second_path, None).expect("open second reader");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open target Pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("member"),
        64 * 1024 * 1024,
    ))
    .expect("add target member");
    let entries = generated
        .iter()
        .enumerate()
        .map(|(index, (hash, bytes))| (*hash, bytes.len() as u64, index % 2))
        .collect::<Vec<_>>();
    let mut authorizations = Vec::new();
    let batch = reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer(
        &[&first_reader, &second_reader],
        &pool,
        &entries,
        usize::MAX,
        &mut |cursor, writes| {
            authorizations.push((cursor, writes));
            Ok(())
        },
    )
    .expect("reconcile alternating generated union page");

    assert_eq!(batch.scanned, generated.len());
    assert_eq!(batch.source_read_groups, 2);
    assert_eq!(batch.verified, generated.len());
    assert_eq!(batch.inserted, generated.len());
    assert_eq!(authorizations.len(), 1);
    assert_eq!(
        authorizations[0],
        (generated.last().map(|(hash, _)| *hash), generated.len())
    );
}

#[test]
fn concurrent_external_reader_preserves_order_and_rejects_corrupt_bytes() {
    let temp = TempDir::new().expect("temp dir");
    let source = temp.path().join("source");
    let external = temp.path().join("external");
    let options = unpacked_external_options(external.clone());
    let store = LmdbBlobStore::with_map_size_and_external_blob_options(
        &source,
        64 * 1024 * 1024,
        Some(options.clone()),
    )
    .expect("open source writer");
    let blobs = (0..96)
        .map(|index| {
            let data = format!("real external blob {index:04} ")
                .repeat(128)
                .into_bytes();
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store.put_many_sync(&blobs).expect("populate source"),
        blobs.len()
    );
    drop(store);

    let reader = LmdbBlobReader::open_with_external_read_concurrency(&source, Some(options), 4)
        .expect("open concurrent reader");
    let hashes = reader
        .scan_hashes_after(None, blobs.len())
        .expect("scan source");
    let bounded = reader
        .read_hashes_bounded(&hashes, 10_000)
        .expect("read byte-bounded external files");
    assert_eq!(
        bounded.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(),
        hashes[..bounded.len()]
    );
    assert!(bounded.iter().map(|(_, data)| data.len()).sum::<usize>() <= 10_000);
    let values = reader
        .read_hashes_bounded(&hashes, u64::MAX)
        .expect("read concurrent external files");
    assert_eq!(
        values.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(),
        hashes,
        "concurrent physical reads must preserve durable cursor order"
    );
    for (hash, data) in &values {
        assert_eq!(sha256(data), *hash);
    }

    let grown_hash = *hashes.last().expect("grown-file hash");
    let grown_path = unpacked_external_path(&external, &grown_hash);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&grown_path)
        .expect("open grown source")
        .set_len(1024 * 1024 * 1024)
        .expect("make sparse grown source");
    let error = reader
        .read_hashes_bounded(&[grown_hash], u64::MAX)
        .expect_err("grown file must be rejected before body allocation");
    assert!(error.to_string().contains("file has"));
    let grown_original = blobs
        .iter()
        .find_map(|(hash, data)| (*hash == grown_hash).then_some(data))
        .expect("original grown-file bytes");
    std::fs::write(&grown_path, grown_original).expect("restore grown source");

    let corrupt_hash = hashes[hashes.len() / 2];
    let corrupt_path = unpacked_external_path(&external, &corrupt_hash);
    let corrupt_len = std::fs::metadata(&corrupt_path)
        .expect("corrupt target metadata")
        .len() as usize;
    std::fs::write(&corrupt_path, vec![0xa5; corrupt_len]).expect("corrupt source bytes");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open target pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("target"),
        64 * 1024 * 1024,
    ))
    .expect("add target");
    let error =
        migrate_lmdb_batch_with_max_buffer_bytes(&reader, &pool, None, blobs.len(), usize::MAX)
            .expect_err("same-size corrupt source bytes must fail hash verification");
    assert!(error.to_string().contains("corrupt bytes"));

    let original = blobs
        .iter()
        .find_map(|(hash, data)| (*hash == corrupt_hash).then_some(data))
        .expect("original corrupt-target bytes");
    std::fs::write(&corrupt_path, original).expect("restore source bytes");
    let missing_hash = hashes[hashes.len() / 3];
    std::fs::remove_file(unpacked_external_path(&external, &missing_hash))
        .expect("remove one source file");
    assert!(
        reader.read_hashes_bounded(&hashes, u64::MAX).is_err(),
        "one worker's file error must fail the batch without hanging"
    );
}

#[cfg(unix)]
#[test]
fn external_reader_cancels_before_unstarted_blocking_file() {
    let temp = TempDir::new().expect("temp dir");
    let source = temp.path().join("source");
    let external = temp.path().join("external");
    let store = LmdbBlobStore::with_map_size_and_external_blob_options(
        &source,
        64 * 1024 * 1024,
        Some(unpacked_external_options(external.clone())),
    )
    .expect("open source writer");
    let blobs = (0..16)
        .map(|index| {
            let data = format!("cancel external blob {index:04}")
                .repeat(64)
                .into_bytes();
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store.put_many_sync(&blobs).expect("populate source"),
        blobs.len()
    );
    drop(store);
    let reader = LmdbBlobReader::open(&source, Some(unpacked_external_options(external.clone())))
        .expect("open source reader");
    let hashes = reader
        .scan_hashes_after(None, blobs.len())
        .expect("scan source");
    drop(reader);
    std::fs::remove_file(unpacked_external_path(&external, &hashes[0]))
        .expect("remove first sorted source");
    let blocking_path = unpacked_external_path(&external, &hashes[1]);
    std::fs::remove_file(&blocking_path).expect("remove second sorted source");
    assert!(Command::new("mkfifo")
        .arg(&blocking_path)
        .status()
        .expect("create blocking fifo")
        .success());

    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("concurrent_external_reader_cancel_helper")
        .env(CANCEL_SOURCE_ENV, &source)
        .env(CANCEL_EXTERNAL_ENV, &external)
        .env("RUST_TEST_THREADS", "1")
        .spawn()
        .expect("run cancellation helper");
    for _ in 0..300 {
        if let Some(status) = child.try_wait().expect("poll cancellation helper") {
            assert!(status.success());
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("kill stuck cancellation helper");
    let _ = child.wait();
    panic!("reader did not cancel before opening the next blocking file");
}

#[cfg(unix)]
#[test]
fn concurrent_external_reader_keeps_file_descriptors_bounded() {
    let temp = TempDir::new().expect("temp dir");
    let source = temp.path().join("source");
    let external = temp.path().join("external");
    let store = LmdbBlobStore::with_map_size_and_external_blob_options(
        &source,
        64 * 1024 * 1024,
        Some(unpacked_external_options(external.clone())),
    )
    .expect("open source writer");
    let blobs = (0..128)
        .map(|index| {
            let data = format!("fd-bounded external blob {index:04}")
                .repeat(64)
                .into_bytes();
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store.put_many_sync(&blobs).expect("populate source"),
        blobs.len()
    );
    drop(store);

    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg("ulimit -n 32; exec \"$@\"")
        .arg("bounded-fd-test")
        .arg(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("concurrent_external_reader_fd_helper")
        .env(FD_SOURCE_ENV, &source)
        .env(FD_EXTERNAL_ENV, &external)
        .env("RUST_TEST_THREADS", "1")
        .status()
        .expect("run reader under low fd limit");
    assert!(status.success());
}
