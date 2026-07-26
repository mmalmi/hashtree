#![cfg(feature = "lmdb")]

use anyhow::Result;
use hashtree_cli::storage::{HashtreeStore, LocalStore, PRIORITY_OTHER};
use hashtree_config::StorageBackend;
use hashtree_core::{sha256, types::Hash};
use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::TempDir;

fn payload(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut bytes = vec![0u8; len];
    for byte in &mut bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    bytes
}

#[test]
fn concurrent_cached_writes_recover_an_already_over_quota_store() -> Result<()> {
    const MAX_BYTES: u64 = 64 * 1024;
    const WRITERS: usize = 32;

    let temp = TempDir::new()?;
    drop(LocalStore::new_unbounded_with_lmdb_map_size(
        temp.path().join("blobs"),
        &StorageBackend::Lmdb,
        Some(16 * 1024 * 1024),
    )?);
    let store = Arc::new(HashtreeStore::with_options_and_backend(
        temp.path(),
        None,
        MAX_BYTES,
        true,
        &StorageBackend::Lmdb,
    )?);

    // Seed all durable/protected classes before deliberately putting the raw
    // writable tier over quota. `put_blob` is the low-level tree-building path
    // and intentionally permits this transient state.
    let owner = [0x42; 32];
    let owned = payload(1, 4 * 1024);
    let owned_hash = sha256(&owned);
    store.put_owned_blob(&owned, &owner)?;

    let pinned = payload(2, 4 * 1024);
    let pinned_hash = sha256(&pinned);
    store.put_blob(&pinned)?;
    store.pin(&pinned_hash)?;

    let indexed = payload(3, 4 * 1024);
    let indexed_hash = sha256(&indexed);
    store.put_blob(&indexed)?;
    store.index_tree(
        &indexed_hash,
        "test-owner",
        Some("protected"),
        PRIORITY_OTHER,
        None,
    )?;

    let disposable: Vec<(Hash, Vec<u8>)> = (0..256)
        .map(|index| {
            let bytes = payload(10_000 + index, 1024);
            (sha256(&bytes), bytes)
        })
        .collect();
    for (_, bytes) in &disposable {
        store.put_blob(bytes)?;
    }
    assert!(
        store.router().writable_stats()?.total_bytes > MAX_BYTES,
        "fixture must begin over quota"
    );
    let cleanup_epoch_before = store.cache_cleanup_epoch_count();

    let start = Arc::new(Barrier::new(WRITERS + 1));
    let workers = (0..WRITERS)
        .map(|index| {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let bytes = payload(1_000_000 + index as u64, 2 * 1024);
                start.wait();
                store.put_cached_blob(&bytes)
            })
        })
        .collect::<Vec<_>>();
    start.wait();

    let mut succeeded = 0usize;
    let mut rejected = 0usize;
    for worker in workers {
        match worker.join().expect("cache writer panicked") {
            Ok(_) => succeeded += 1,
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("quota cleanup is in progress")
                        || message.contains("quota cleanup is temporarily unavailable")
                        || message.contains("no disposable cache space is available"),
                    "unexpected cache-writer failure: {message}"
                );
                rejected += 1;
            }
        }
    }
    assert!(
        succeeded >= 1,
        "cleanup leader did not admit its cache write"
    );
    assert!(
        rejected >= 1,
        "concurrent followers did not encounter the active cleanup"
    );
    assert_eq!(
        store.cache_cleanup_epoch_count() - cleanup_epoch_before,
        1,
        "concurrent cache writers should coalesce into one cleanup epoch"
    );

    let stats = store.router().writable_stats()?;
    assert!(
        stats.total_bytes <= MAX_BYTES,
        "concurrent cleanup must converge under quota: used={} max={MAX_BYTES}",
        stats.total_bytes
    );
    assert!(store.blob_exists(&owned_hash)?, "owned blob was evicted");
    assert!(
        store.is_blob_owner(&owned_hash, &owner)?,
        "owned blob metadata was lost"
    );
    assert!(store.blob_exists(&pinned_hash)?, "pinned blob was evicted");
    assert!(
        store.blob_exists(&indexed_hash)?,
        "indexed tree blob was evicted"
    );
    assert!(
        store.get_tree_meta(&indexed_hash)?.is_some(),
        "indexed tree metadata was lost"
    );
    assert!(
        disposable
            .iter()
            .any(|(hash, _)| !store.blob_exists(hash).unwrap_or(false)),
        "over-quota recovery should remove disposable cache entries"
    );

    Ok(())
}
