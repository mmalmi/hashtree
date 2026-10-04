//! Disposable warm/cold pairs on Linux. No production path or cache is targeted.
use super::cold_files::OwnedFiles;
use super::*;

fn config() -> PoolStoreConfig {
    let mut config = PoolStoreConfig {
        catalog_map_size_bytes: 128 * 1024 * 1024,
        physical_space: Some(PhysicalSpaceGuard::new(10 * 1024 * 1024 * 1024).unwrap()),
        ..Default::default()
    };
    config.temperature.enabled = false;
    config
}

fn invariant(row: &serde_json::Value) -> serde_json::Value {
    let phases = row["phases"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, counts)| {
            let counts = counts
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| !key.ends_with("_us") && !key.ends_with("_io"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            (name.clone(), serde_json::Value::Object(counts))
        })
        .collect();
    let mut row = row.as_object().unwrap().clone();
    for field in ["backend", "append_us", "preparation"] {
        row.remove(field);
    }
    row.insert("phases".into(), serde_json::Value::Object(phases));
    serde_json::Value::Object(row)
}

#[tokio::test]
async fn append_io_cold_pool_first_reads() {
    // Bounded test-only dataset choices, never a crawler runtime option.
    let history = match std::env::var("HTREE_APPEND_IO_HISTORY").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("8192") => 8192,
        Ok("32768") => 32768,
        _ => panic!("HTREE_APPEND_IO_HISTORY must be 8192 or 32768"),
    };
    let historical = (0..history)
        .map(|i| {
            let key = Keys::parse(&format!("{:064x}", i % 16 + 1)).unwrap();
            event(&key, i, 10000 + 2 * i as u64, 8)
        })
        .collect::<Vec<_>>();
    let seed = Arc::new(MemoryStore::new());
    let root = NostrEventStore::with_options(
        seed.clone(),
        NostrEventStoreOptions {
            index_commit_batch_size: None,
            ..Default::default()
        },
    )
    .build(None, historical.clone())
    .await
    .unwrap()
    .unwrap();
    let mut blobs = seed
        .keys()
        .into_iter()
        .map(|hash| {
            (
                hash,
                futures::executor::block_on(seed.get(&hash))
                    .unwrap()
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    blobs.sort_by_key(|(hash, _)| *hash);
    let key = Keys::parse(&format!("{:064x}", 9001)).unwrap();
    for tags in [0, 32] {
        let incoming = (0..49)
            .map(|i| {
                event(
                    &key,
                    90000 + i,
                    10001 + 2 * (i * (history - 1) / 49) as u64,
                    tags,
                )
            })
            .collect::<Vec<_>>();
        let mut expected = None;
        // Reverse order for the second tag case; repeat in separate processes
        // for performance comparisons, never average interrupted/noisy runs.
        let order = if tags == 0 {
            [false, true]
        } else {
            [true, false]
        };
        for cold in order {
            let directory = tempfile::tempdir().unwrap();
            let catalog = directory.path().join("catalog");
            let pool = PoolStore::open(&catalog, config()).unwrap();
            pool.add_member(PoolMemberConfig::new(
                directory.path().join("member"),
                2 * 1024 * 1024 * 1024,
            ))
            .unwrap();
            for chunk in blobs.chunks(512) {
                pool.put_many_optimistic_sync(chunk).unwrap();
            }
            pool.force_sync().unwrap();
            drop(pool); // Release all LMDB mappings before file-scoped eviction.
            let mut files = OwnedFiles::open(&directory);
            let prepared = files.prepare(cold);
            let reopen_start = Instant::now();
            let reopen_io = os_io::ProcessIo::sample().unwrap();
            let pool = Arc::new(PoolStore::open(&catalog, config()).unwrap());
            let reopen_us = reopen_start.elapsed().as_micros() as u64;
            let reopen_io = os_io::ProcessIo::sample().unwrap().delta(reopen_io);
            let reopened = files.sample();
            pool.force_sync().unwrap();
            // No transactions/workers exist: Pool reopen and sync have returned,
            // relocation is disabled, and this sole owner has not started reads.
            assert_eq!(Arc::strong_count(&pool), 1);
            let reset = if cold {
                unsafe { files.reset_reopened() }
            } else {
                serde_json::Value::Null
            };
            let mode = if cold {
                "lmdb-pool-cold"
            } else {
                "lmdb-pool-warm"
            };
            let (_, row) = measure(
                pool.clone(),
                &seed,
                &historical,
                &root,
                incoming.clone(),
                mode,
                tags,
                || {
                    serde_json::json!({
                        "cache_mode": mode,
                        "files": ["catalog", "member"],
                        "after_preparation": prepared,
                        "after_reopen": reopened,
                        "reopen_us": reopen_us,
                        "reopen_process_io": reopen_io,
                        "reopen_reset": reset,
                        "at_append_boundary": files.ready(cold),
                    })
                },
            )
            .await;
            if cold {
                let read_bytes: u64 = row["phases"]
                    .as_object()
                    .unwrap()
                    .values()
                    .map(|p| p["process_io"]["read_bytes"].as_u64().unwrap())
                    .sum();
                let read_blocks: u64 = row["phases"]
                    .as_object()
                    .unwrap()
                    .values()
                    .filter_map(|p| p["read_thread_io"]["input_blocks"].as_u64())
                    .sum();
                assert!(
                    read_bytes > 0 && read_blocks > 0,
                    "cold admission requires storage-read evidence in actual Pool gets"
                );
            }
            let logical = invariant(&row);
            if let Some(expected) = &expected {
                assert_eq!(&logical, expected);
            } else {
                expected = Some(logical);
            }
            pool.force_sync().unwrap(); // Checkpoint durability excluded from append time.
            drop(pool);
            drop(files);
            drop(directory);
        }
    }
}
