//! Exact-result concurrency sweep on disposable, file-verified cold Pool data.
use super::cold_files::OwnedFiles;
use super::*;
use sha2::{Digest, Sha256};

#[tokio::test]
async fn cold_pool_required_lookup_worker_sweep() {
    let seed = Arc::new(MemoryStore::new());
    let entries = (0..32768u64)
        .map(|n| {
            let hash: [u8; 32] = Sha256::digest(n.to_be_bytes()).into();
            (hex::encode(hash), Cid { hash, key: None })
        })
        .collect::<BTreeMap<_, _>>();
    let root = BTree::new(seed.clone(), BTreeOptions::default())
        .build_links(entries.clone())
        .await
        .unwrap()
        .unwrap();
    let keys = entries
        .keys()
        .step_by(127)
        .take(256)
        .cloned()
        .collect::<Vec<_>>();
    let expected = keys
        .iter()
        .map(|key| (key.clone(), entries[key].clone()))
        .collect::<BTreeMap<_, _>>();
    let directory = tempfile::tempdir().unwrap();
    let mut config = PoolStoreConfig {
        catalog_map_size_bytes: 128 * 1024 * 1024,
        physical_space: Some(PhysicalSpaceGuard::new(10 * 1024 * 1024 * 1024).unwrap()),
        ..Default::default()
    };
    config.temperature.enabled = false;
    let pool = PoolStore::open(directory.path().join("catalog"), config.clone()).unwrap();
    pool.add_member(PoolMemberConfig::new(
        directory.path().join("member"),
        512 * 1024 * 1024,
    ))
    .unwrap();
    let blobs = seed
        .keys()
        .into_iter()
        .map(|hash| {
            let bytes = futures::executor::block_on(seed.get(&hash))
                .unwrap()
                .unwrap();
            (hash, bytes)
        })
        .collect::<Vec<_>>();
    for chunk in blobs.chunks(256) {
        pool.put_many_optimistic_sync(chunk).unwrap();
    }
    pool.force_sync().unwrap();
    drop(pool);
    let mut files = OwnedFiles::open(&directory);
    files.prepare(true);
    let pool = Arc::new(PoolStore::open(directory.path().join("catalog"), config).unwrap());
    pool.force_sync().unwrap();
    for (round, order) in [[1, 2, 4, 8, 16], [16, 8, 4, 2, 1]].into_iter().enumerate() {
        for workers in order {
            for cold in [true, false] {
                assert_eq!(Arc::strong_count(&pool), 1);
                if cold {
                    unsafe {
                        files.reset_reopened();
                    }
                } else {
                    files.prepare(false);
                }
                files.ready(cold);
                let index = BTree::new(pool.clone(), BTreeOptions::default())
                    .with_read_concurrency(workers);
                let io = os_io::ProcessIo::sample().unwrap();
                let start = Instant::now();
                let found = index.get_links_required(&root, keys.clone()).await.unwrap();
                let micros = start.elapsed().as_micros();
                let io = os_io::ProcessIo::sample().unwrap().delta(io);
                assert_eq!(found, expected);
                if cold {
                    assert!(io.read_bytes > 0, "cold run must issue physical reads");
                }
                println!(
                    "lookup-worker-measurement {}",
                    serde_json::json!({
                        "round":round,"workers":workers,"cold":cold,"lookup_us":micros,
                        "physical_read_bytes":io.read_bytes,"keys":keys.len(),"history_keys":entries.len(),
                        "root_hash":hex::encode(root.hash),"exact_results":true,
                    })
                );
                drop(index);
            }
        }
    }
    drop(pool);
    drop(files);
    drop(directory);
}
