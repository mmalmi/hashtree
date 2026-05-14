#![cfg(feature = "lmdb")]

use anyhow::Result;
use hashtree_cli::HashtreeStore;
use hashtree_config::StorageBackend;
use hashtree_core::{sha256, types::Hash};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn payload(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut out = vec![0u8; len];
    for byte in &mut out {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    out
}

#[test]
#[ignore = "stress benchmark; set HTREE_ACCESS_STRESS_* env vars for longer runs"]
fn mixed_blob_reader_writer_access_stress_lmdb() -> Result<()> {
    let blob_count = env_usize("HTREE_ACCESS_STRESS_BLOBS", 512);
    let blob_size = env_usize("HTREE_ACCESS_STRESS_BLOB_BYTES", 16 * 1024);
    let readers = env_usize("HTREE_ACCESS_STRESS_READERS", 8);
    let writers = env_usize("HTREE_ACCESS_STRESS_WRITERS", 2);
    let duration = Duration::from_secs(env_u64("HTREE_ACCESS_STRESS_SECONDS", 2));
    let max_bytes = env_u64("HTREE_ACCESS_STRESS_MAX_BYTES", 128 * 1024 * 1024);

    assert!(blob_count > 0, "HTREE_ACCESS_STRESS_BLOBS must be nonzero");

    let temp = TempDir::new()?;
    let store = Arc::new(HashtreeStore::with_options_and_backend(
        temp.path(),
        None,
        max_bytes,
        true,
        &StorageBackend::Lmdb,
    )?);

    let mut seeded = Vec::<Hash>::with_capacity(blob_count);
    for index in 0..blob_count {
        let data = payload(index as u64, blob_size);
        let hash = sha256(&data);
        store.put_cached_blob(&data)?;
        seeded.push(hash);
    }
    let seeded = Arc::new(seeded);

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let misses = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let start = Instant::now();

    let mut handles = Vec::new();
    for reader_id in 0..readers {
        let store = Arc::clone(&store);
        let hashes = Arc::clone(&seeded);
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        let misses = Arc::clone(&misses);
        let errors = Arc::clone(&errors);
        handles.push(std::thread::spawn(move || {
            let mut cursor = reader_id;
            while !stop.load(Ordering::Relaxed) {
                let hash = hashes[cursor % hashes.len()];
                match store.get_blob(&hash) {
                    Ok(Some(_)) => {
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(None) => {
                        misses.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                cursor = cursor.wrapping_add(readers.max(1));
            }
        }));
    }

    for writer_id in 0..writers {
        let store = Arc::clone(&store);
        let stop = Arc::clone(&stop);
        let writes = Arc::clone(&writes);
        let errors = Arc::clone(&errors);
        handles.push(std::thread::spawn(move || {
            let mut index = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let data = payload(1_000_000 + writer_id as u64 + index * 97, blob_size);
                match store.put_cached_blob(&data) {
                    Ok(_) => {
                        writes.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                index = index.wrapping_add(1);
            }
        }));
    }

    while start.elapsed() < duration {
        std::thread::sleep(Duration::from_millis(50));
        let _ = store.get_storage_stats()?;
    }
    stop.store(true, Ordering::Relaxed);

    for handle in handles {
        handle.join().expect("stress worker panicked");
    }

    let stats = store.get_storage_stats()?;
    eprintln!(
        "mixed_blob_reader_writer_access_stress_lmdb: reads={} writes={} misses={} errors={} stored_objects={} total_bytes={}",
        reads.load(Ordering::Relaxed),
        writes.load(Ordering::Relaxed),
        misses.load(Ordering::Relaxed),
        errors.load(Ordering::Relaxed),
        stats.total_dags,
        stats.total_bytes
    );

    assert!(reads.load(Ordering::Relaxed) > 0, "expected blob reads");
    assert!(writes.load(Ordering::Relaxed) > 0, "expected blob writes");
    assert_eq!(errors.load(Ordering::Relaxed), 0, "stress workers failed");
    Ok(())
}
