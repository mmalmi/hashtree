use super::*;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use tempfile::TempDir;

const HELPER_MODE: &str = "HASHTREE_POOL_HELPER_MODE";
const HELPER_CATALOG: &str = "HASHTREE_POOL_HELPER_CATALOG";
const HELPER_READY: &str = "HASHTREE_POOL_HELPER_READY";
const HELPER_HASH: &str = "HASHTREE_POOL_HELPER_HASH";
const PENDING_DATA: &[u8] = b"pool pending crash recovery bytes";

#[derive(serde::Serialize)]
struct LegacyPoolMemberConfig {
    path: PathBuf,
    capacity_bytes: u64,
    map_size_bytes: u64,
    external_blob_dir: Option<PathBuf>,
    external_blob_min_bytes: Option<u64>,
    external_blob_sync: bool,
    external_pack_target_bytes: Option<u64>,
    max_read_concurrency: u32,
    max_write_concurrency: u32,
}

#[derive(serde::Serialize)]
struct LegacyMemberRecord {
    id: PoolMemberId,
    state: PoolMemberState,
    config: LegacyPoolMemberConfig,
}

#[derive(serde::Serialize)]
struct LegacyPoolManifest {
    version: u32,
    generation: u64,
    members: Vec<LegacyMemberRecord>,
}

#[test]
fn manifests_without_temperature_watermarks_use_safe_defaults() {
    let legacy = LegacyPoolManifest {
        version: 1,
        generation: 7,
        members: vec![LegacyMemberRecord {
            id: PoolMemberId([7; 16]),
            state: PoolMemberState::Active,
            config: LegacyPoolMemberConfig {
                path: PathBuf::from("member"),
                capacity_bytes: 1_000,
                map_size_bytes: MIN_MEMBER_MAP_SIZE_BYTES,
                external_blob_dir: None,
                external_blob_min_bytes: None,
                external_blob_sync: true,
                external_pack_target_bytes: None,
                max_read_concurrency: 8,
                max_write_concurrency: 4,
            },
        }],
    };
    let bytes = rmp_serde::to_vec_named(&legacy).expect("encode legacy manifest");
    let decoded = decode_manifest(&bytes).expect("decode legacy manifest");
    assert_eq!(decoded.generation, 7);
    assert_eq!(
        decoded.members[0].config.temperature_low_watermark_percent,
        70
    );
    assert_eq!(
        decoded.members[0].config.temperature_high_watermark_percent,
        85
    );
}

#[test]
fn drop_closes_pool_catalog_and_member_environments() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let member = temp.path().join("member");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open pool");
    pool.add_member(PoolMemberConfig::new(member.clone(), 1024 * 1024))
        .expect("add member");
    let data = b"close pool environments";
    pool.put_sync(sha256(data), data).expect("write blob");
    let canonical_catalog = fs::canonicalize(&catalog).expect("canonical catalog");
    let canonical_member = fs::canonicalize(&member).expect("canonical member");

    drop(pool);

    assert!(
        heed::env_closing_event(canonical_catalog).is_none(),
        "pool catalog must close"
    );
    assert!(
        heed::env_closing_event(canonical_member).is_none(),
        "pool member must close"
    );
}

#[test]
#[ignore = "subprocess entry point for pool crash recovery tests"]
fn pool_pending_helper() {
    let Ok(mode) = std::env::var(HELPER_MODE) else {
        return;
    };
    let catalog = PathBuf::from(std::env::var_os(HELPER_CATALOG).expect("catalog path"));
    let ready = PathBuf::from(std::env::var_os(HELPER_READY).expect("ready path"));
    let hash = hashtree_core::from_hex(&std::env::var(HELPER_HASH).expect("hash hex"))
        .expect("32-byte hash");
    let pool = PoolStore::open(catalog, PoolStoreConfig::default()).expect("open pool");
    let target = pool
        .choose_write_member(PENDING_DATA.len() as u64, None)
        .expect("choose member");
    let pending = LocationRecord::Pending {
        member: target,
        size: PENDING_DATA.len() as u64,
    };
    pool.reserve_if_absent(hash, pending)
        .expect("reserve pending location");
    if mode == "written" {
        let store = pool.get_member(target).expect("open target");
        pool.write_verified_member(target, &store, hash, PENDING_DATA)
            .expect("write pending member data");
    }
    fs::write(ready, b"ready").expect("write ready marker");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(unix)]
#[test]
fn process_death_before_or_after_member_write_recovers_pending_location() {
    for mode in ["reserved", "written"] {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open pool");
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("member"),
            1024 * 1024,
        ))
        .expect("add member");
        drop(pool);

        let ready = temp.path().join("ready");
        let hash = sha256(PENDING_DATA);
        let mut child = spawn_pending_helper(mode, &catalog, &ready, hash);
        wait_for_file(&ready, &mut child);
        child.kill().expect("kill pending helper");
        let output = child.wait_with_output().expect("reap pending helper");
        assert!(!output.status.success(), "helper must be killed");

        let recovered = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("reopen pool");
        let inserted = recovered
            .put_sync(hash, PENDING_DATA)
            .expect("recover pending put");
        assert_eq!(inserted, mode == "reserved");
        assert_eq!(
            recovered.get_sync(&hash).expect("read recovered blob"),
            Some(PENDING_DATA.to_vec())
        );
        assert!(matches!(
            recovered.read_location(&hash).expect("location"),
            Some(LocationRecord::Stored { .. })
        ));
    }
}

#[test]
fn sampled_reads_stay_in_memory_until_one_bounded_flush() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.read_sample_rate = 4;
    config.temperature.access_flush_batch = 2;
    config.temperature.candidate_capacity = 4;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("open pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("member"),
        1024 * 1024,
    ))
    .expect("add member");
    let data = b"sampled temperature access";
    let hash = sha256(data);
    pool.put_sync(hash, data).expect("put");
    pool.touch_accessed_sync(&hash, 42)
        .expect("seed access time");

    for _ in 0..3 {
        assert_eq!(pool.get_sync(&hash).expect("get"), Some(data.to_vec()));
    }
    assert_eq!(pool.last_accessed_at_sync(&hash).expect("access"), Some(42));
    assert_eq!(
        pool.temperature.lock().expect("temperature").samples.len(),
        0
    );

    assert_eq!(
        pool.get_sync(&hash).expect("sampled get"),
        Some(data.to_vec())
    );
    assert_eq!(pool.last_accessed_at_sync(&hash).expect("access"), Some(42));
    assert_eq!(
        pool.temperature.lock().expect("temperature").samples.len(),
        1
    );
    let flushed = pool
        .flush_sampled_accesses(unix_timestamp_now())
        .expect("flush samples");
    assert_eq!(flushed.len(), 1);
    assert_ne!(pool.last_accessed_at_sync(&hash).expect("access"), Some(42));
}

#[test]
fn incremental_temperature_cursor_is_bounded_and_persists_across_reopen() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let mut config = PoolStoreConfig::default();
    config.temperature.scan_items_per_cycle = 2;
    let pool = PoolStore::open(&catalog, config.clone()).expect("open pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("member"),
        1024 * 1024,
    ))
    .expect("add member");
    for value in 0..5u8 {
        let data = vec![value; 32];
        pool.put_sync(sha256(&data), &data).expect("put");
    }

    let first = pool
        .scan_temperature_candidates(unix_timestamp_now())
        .expect("first scan");
    let second = pool
        .scan_temperature_candidates(unix_timestamp_now())
        .expect("second scan");
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    assert!(first
        .iter()
        .all(|candidate| second.iter().all(|other| candidate.hash != other.hash)));
    drop(pool);

    let reopened = PoolStore::open(&catalog, config).expect("reopen pool");
    let third = reopened
        .scan_temperature_candidates(unix_timestamp_now())
        .expect("resumed scan");
    assert_eq!(third.len(), 2);
    assert!(third.iter().any(|candidate| {
        first
            .iter()
            .chain(&second)
            .all(|other| candidate.hash != other.hash)
    }));
}

#[test]
fn temperature_lease_has_exact_multiprocess_ownership_and_expiry() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let first = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("first pool");
    let second = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("second pool");
    assert!(first
        .try_acquire_temperature_lease(100)
        .expect("first lease"));
    assert!(!second
        .try_acquire_temperature_lease(100)
        .expect("contended lease"));
    assert!(first
        .renew_temperature_lease(200)
        .expect("renew owned lease"));
    assert!(!second
        .try_acquire_temperature_lease(250)
        .expect("renewed lease remains contended"));
    first.release_temperature_lease().expect("release lease");
    assert!(second
        .try_acquire_temperature_lease(100)
        .expect("replacement lease"));
    assert!(first
        .try_acquire_temperature_lease(1_000)
        .expect("expired lease"));
}

#[test]
fn long_temperature_cycle_heartbeats_its_process_lease() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let mut config = PoolStoreConfig::default();
    config.temperature.interval = Duration::from_secs(60 * 60);
    config.temperature.lease_duration = Duration::from_secs(1);
    let first = PoolStore::open(&catalog, config.clone()).expect("first pool");
    let second = PoolStore::open(&catalog, config).expect("second pool");
    let now = unix_timestamp_now();
    assert!(first
        .try_acquire_temperature_lease(now)
        .expect("first lease"));
    let heartbeat = super::temperature_worker::TemperatureLeaseHeartbeat::start(
        std::sync::Arc::downgrade(&first.inner),
        Duration::from_secs(1),
    )
    .expect("start lease heartbeat");
    thread::sleep(Duration::from_millis(1_600));
    assert!(!second
        .try_acquire_temperature_lease(unix_timestamp_now())
        .expect("heartbeat preserves ownership"));
    drop(heartbeat);
}

#[test]
fn hot_blob_promotes_to_measurably_faster_member() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.read_sample_rate = 1;
    config.temperature.minimum_residence = Duration::ZERO;
    config.temperature.scan_items_per_cycle = 16;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let slow = pool
        .add_member(PoolMemberConfig::new(temp.path().join("slow"), 1024 * 1024))
        .expect("slow member");
    let data = b"hot promotion candidate".repeat(32);
    let hash = sha256(&data);
    pool.put_sync(hash, &data).expect("seed slow");
    let fast = pool
        .add_member(PoolMemberConfig::new(temp.path().join("fast"), 1024 * 1024))
        .expect("fast member");
    pool.record_read(slow, Duration::from_millis(100), true);
    pool.record_read(fast, Duration::from_millis(10), true);
    assert_eq!(pool.get_sync(&hash).expect("hot read"), Some(data.clone()));

    let report = pool.balance_temperature().expect("temperature balance");
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(pool.blob_location(&hash).expect("location"), Some(fast));
    assert_eq!(pool.get_sync(&hash).expect("read promoted"), Some(data));
}

#[test]
fn cold_blob_demotes_to_capacity_and_does_not_thrash() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.minimum_residence = Duration::ZERO;
    config.temperature.scan_items_per_cycle = 16;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let data = b"cold demotion candidate".repeat(32);
    let hash = sha256(&data);
    let small = pool
        .add_member(
            PoolMemberConfig::new(temp.path().join("small-fast"), data.len() as u64)
                .with_temperature_watermarks(25, 50),
        )
        .expect("small member");
    pool.put_sync(hash, &data).expect("seed small member");
    let large = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("large"),
            data.len() as u64 * 16,
        ))
        .expect("large member");

    let first = pool.balance_temperature().expect("cold balance");
    assert_eq!(first.moved, 1, "{first:?}");
    assert_eq!(pool.blob_location(&hash).expect("location"), Some(large));
    assert_eq!(pool.get_sync(&hash).expect("read"), Some(data));
    let second = pool.balance_temperature().expect("repeat balance");
    assert_eq!(second.moved, 0, "{second:?}");
    assert_ne!(pool.blob_location(&hash).expect("location"), Some(small));
}

#[test]
fn promotion_preserves_configured_fast_member_headroom() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.read_sample_rate = 1;
    config.temperature.minimum_residence = Duration::ZERO;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let data = b"headroom promotion candidate".repeat(32);
    let hash = sha256(&data);
    let slow = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("slow"),
            data.len() as u64 * 4,
        ))
        .expect("slow member");
    pool.put_sync(hash, &data).expect("seed slow");
    let fast = pool
        .add_member(
            PoolMemberConfig::new(temp.path().join("fast"), data.len() as u64)
                .with_temperature_watermarks(25, 50),
        )
        .expect("fast member");
    pool.record_read(slow, Duration::from_millis(100), true);
    pool.record_read(fast, Duration::from_millis(5), true);
    pool.get_sync(&hash).expect("hot read");

    let report = pool.balance_temperature().expect("balance");
    assert_eq!(report.moved, 0, "{report:?}");
    assert_eq!(pool.blob_location(&hash).expect("location"), Some(slow));
}

#[test]
fn high_watermark_pressure_demotes_until_low_watermark() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.minimum_residence = Duration::ZERO;
    config.temperature.scan_items_per_cycle = 32;
    config.temperature.max_moves_per_cycle = 32;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let blobs = (0..10u8)
        .map(|value| {
            let data = vec![value; 100];
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    let source = pool
        .add_member(
            PoolMemberConfig::new(temp.path().join("source"), 1_000)
                .with_temperature_watermarks(40, 80),
        )
        .expect("source member");
    pool.put_many_sync(&blobs).expect("seed source");
    let target = pool
        .add_member(PoolMemberConfig::new(temp.path().join("capacity"), 10_000))
        .expect("target member");

    let report = pool.balance_temperature().expect("balance");
    assert_eq!(report.moved, 6, "{report:?}");
    assert_eq!(pool.member(source).expect("source").logical_bytes, 400);
    assert_eq!(pool.member(target).expect("target").logical_bytes, 600);
}

#[test]
fn interrupted_streamed_move_resumes_from_persisted_state() {
    for scenario in ["present", "deleted", "unavailable", "corrupt-target"] {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let source_path = temp.path().join("source");
        let mut config = PoolStoreConfig::default();
        config.temperature.minimum_residence = Duration::ZERO;
        config.temperature.copy_chunk_bytes = 4 * 1024;
        let pool = PoolStore::open(&catalog, config.clone()).expect("pool");
        let source = pool
            .add_member(PoolMemberConfig::new(source_path.clone(), 8 * 1024 * 1024))
            .expect("source");
        let data = vec![0x5a; 2 * 1024 * 1024];
        let hash = sha256(&data);
        pool.put_sync(hash, &data).expect("seed source");
        let target = pool
            .add_member(
                PoolMemberConfig::new(temp.path().join("target-db"), 8 * 1024 * 1024)
                    .with_external_blobs(temp.path().join("target-blobs"), 1, true, None),
            )
            .expect("target");
        let stored = pool
            .read_location(&hash)
            .expect("location")
            .expect("stored");
        let moving = LocationRecord::Moving {
            source,
            target,
            size: data.len() as u64,
        };
        assert!(pool
            .begin_move_record(hash, stored, moving)
            .expect("persist move"));
        let source_store = pool.get_member(source).expect("source store");
        let target_store = pool.get_member(target).expect("target store");
        source_store
            .copy_blob_to_sync(
                &target_store,
                &hash,
                data.len() as u64,
                config.temperature.copy_chunk_bytes,
            )
            .expect("stream target copy");
        if scenario == "corrupt-target" {
            target_store
                .delete_sync(&hash)
                .expect("delete valid target");
            target_store
                .put_sync(hash, &vec![0xa5; data.len()])
                .expect("install corrupt target");
        }
        if scenario == "deleted" {
            source_store.delete_sync(&hash).expect("delete source");
        }
        drop(source_store);
        drop(target_store);
        drop(pool);
        if scenario == "unavailable" {
            std::fs::rename(&source_path, temp.path().join("source-displaced"))
                .expect("displace source member");
            std::fs::create_dir(&source_path).expect("leave empty source mountpoint");
        }

        let reopened = PoolStore::open(&catalog, config).expect("reopen");
        let report = reopened.balance_temperature().expect("resume balance");
        assert_eq!(report.resumed, 1, "{report:?}");
        assert_eq!(
            reopened.blob_location(&hash).expect("location"),
            Some(target)
        );
        assert_eq!(reopened.get_sync(&hash).expect("read"), Some(data));
        assert!(reopened.active_moves(10).expect("active moves").is_empty());
    }
}

#[test]
fn automatic_worker_promotes_hot_blob_without_application_maintenance_calls() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.interval = Duration::from_millis(10);
    config.temperature.read_sample_rate = 1;
    config.temperature.minimum_residence = Duration::ZERO;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let slow = pool
        .add_member(PoolMemberConfig::new(temp.path().join("slow"), 1024 * 1024))
        .expect("slow");
    let data = b"automatic hot promotion".repeat(32);
    let hash = sha256(&data);
    pool.put_sync(hash, &data).expect("seed slow");
    let fast = pool
        .add_member(PoolMemberConfig::new(temp.path().join("fast"), 1024 * 1024))
        .expect("fast");
    pool.record_read(slow, Duration::from_millis(100), true);
    pool.record_read(fast, Duration::from_millis(5), true);
    pool.get_sync(&hash).expect("hot read");

    for _ in 0..100 {
        if pool.blob_location(&hash).expect("location") == Some(fast) {
            assert_eq!(pool.get_sync(&hash).expect("read"), Some(data));
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("automatic temperature worker did not promote the hot blob");
}

#[test]
fn foreground_member_load_throttles_temperature_moves() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.read_sample_rate = 1;
    config.temperature.minimum_residence = Duration::ZERO;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let mut slow_config = PoolMemberConfig::new(temp.path().join("slow"), 1024 * 1024);
    slow_config.max_read_concurrency = 1;
    let slow = pool.add_member(slow_config).expect("slow");
    let data = b"foreground throttled candidate".repeat(32);
    let hash = sha256(&data);
    pool.put_sync(hash, &data).expect("seed slow");
    let fast = pool
        .add_member(PoolMemberConfig::new(temp.path().join("fast"), 1024 * 1024))
        .expect("fast");
    pool.record_read(slow, Duration::from_millis(100), true);
    pool.record_read(fast, Duration::from_millis(5), true);
    pool.get_sync(&hash).expect("hot read");
    let gate = pool.member_gate(slow, false).expect("read gate");
    let _foreground = gate.acquire().expect("foreground permit");

    let report = pool.balance_temperature().expect("balance");
    assert!(report.throttled, "{report:?}");
    assert_eq!(report.moved, 0);
    assert_eq!(pool.blob_location(&hash).expect("location"), Some(slow));
}

#[test]
fn temperature_cycle_honors_byte_item_and_concurrency_budgets() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.minimum_residence = Duration::ZERO;
    config.temperature.scan_items_per_cycle = 32;
    config.temperature.max_moves_per_cycle = 7;
    config.temperature.max_bytes_per_cycle = 250;
    config.temperature.max_concurrent_moves = 2;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let blobs = (0..8u8)
        .map(|value| {
            let data = vec![value; 100];
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    pool.add_member(
        PoolMemberConfig::new(temp.path().join("source"), 800).with_temperature_watermarks(20, 50),
    )
    .expect("source");
    pool.put_many_sync(&blobs).expect("seed source");
    pool.add_member(PoolMemberConfig::new(temp.path().join("target"), 8_000))
        .expect("target");

    let report = pool.balance_temperature().expect("balance");
    assert_eq!(report.attempted_moves, 2, "{report:?}");
    assert_eq!(report.moved, 2, "{report:?}");
    assert_eq!(report.bytes_moved, 200);
    assert_eq!(report.peak_concurrent_moves, 2);
}

#[test]
fn minimum_residence_blocks_immediate_cold_relocation() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.minimum_residence = Duration::from_secs(60);
    config.temperature.scan_items_per_cycle = 8;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let data = vec![0x7b; 1_000];
    let hash = sha256(&data);
    let source = pool
        .add_member(
            PoolMemberConfig::new(temp.path().join("source"), 1_000)
                .with_temperature_watermarks(25, 50),
        )
        .expect("source");
    pool.put_sync(hash, &data).expect("seed source");
    let target = pool
        .add_member(PoolMemberConfig::new(temp.path().join("target"), 8_000))
        .expect("target");
    let placed = pool
        .last_accessed
        .get(&pool.env.read_txn().expect("read txn"), &hash)
        .expect("access read")
        .and_then(super::temperature::AccessRecord::decode)
        .expect("access record")
        .placed_at;

    let early = pool
        .balance_temperature_at(placed.saturating_add(59))
        .expect("early balance");
    assert_eq!(early.moved, 0, "{early:?}");
    assert_eq!(pool.blob_location(&hash).expect("location"), Some(source));
    let mature = pool
        .balance_temperature_at(placed.saturating_add(60))
        .expect("mature balance");
    assert_eq!(mature.moved, 1, "{mature:?}");
    assert_eq!(pool.blob_location(&hash).expect("location"), Some(target));
}

#[test]
fn deleting_a_moving_blob_clears_its_resumption_record() {
    let temp = TempDir::new().expect("temp dir");
    let pool =
        PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default()).expect("pool");
    let source = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("source"),
            1024 * 1024,
        ))
        .expect("source");
    let data = b"delete an interrupted move";
    let hash = sha256(data);
    pool.put_sync(hash, data).expect("seed source");
    let target = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("target"),
            1024 * 1024,
        ))
        .expect("target");
    let stored = pool
        .read_location(&hash)
        .expect("location")
        .expect("stored");
    assert!(pool
        .begin_move_record(
            hash,
            stored,
            LocationRecord::Moving {
                source,
                target,
                size: data.len() as u64,
            },
        )
        .expect("begin move"));
    assert_eq!(pool.active_moves(10).expect("moves").len(), 1);
    assert!(pool.delete_sync(&hash).expect("delete"));
    assert!(pool.active_moves(10).expect("moves").is_empty());
}

#[test]
fn streamed_move_rejects_corrupt_source_before_target_commit() {
    let temp = TempDir::new().expect("temp dir");
    let mut config = PoolStoreConfig::default();
    config.temperature.copy_chunk_bytes = 7;
    let pool = PoolStore::open(temp.path().join("catalog"), config).expect("pool");
    let source = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("source"),
            1024 * 1024,
        ))
        .expect("source");
    let data = b"hash verified streaming move";
    let corrupt = b"same length corrupt payload!";
    assert_eq!(data.len(), corrupt.len());
    let hash = sha256(data);
    pool.put_sync(hash, data).expect("seed source");
    let target = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("target"),
            1024 * 1024,
        ))
        .expect("target");
    let source_store = pool.get_member(source).expect("source store");
    source_store
        .delete_sync(&hash)
        .expect("delete valid source");
    source_store
        .put_sync(hash, corrupt)
        .expect("install corrupt source");
    pool.begin_drain(source).expect("begin drain");

    let report = pool.maintain(1).expect("maintenance report");
    assert_eq!(report.moved, 0, "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(matches!(
        pool.read_location(&hash).expect("location"),
        Some(LocationRecord::Moving {
            source: actual_source,
            target: actual_target,
            ..
        }) if actual_source == source && actual_target == target
    ));
    assert_eq!(
        pool.get_member(target)
            .expect("target store")
            .blob_size_sync(&hash)
            .expect("target size"),
        None
    );
}

#[test]
fn normal_writes_preserve_member_high_watermark_when_capacity_exists() {
    let temp = TempDir::new().expect("temp dir");
    let pool =
        PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default()).expect("pool");
    let fast = pool
        .add_member(
            PoolMemberConfig::new(temp.path().join("fast"), 1_000)
                .with_temperature_watermarks(25, 50),
        )
        .expect("fast");
    let fill = vec![0x81; 500];
    pool.put_sync(sha256(&fill), &fill).expect("fill fast");
    assert_eq!(
        pool.blob_location(&sha256(&fill)).expect("location"),
        Some(fast)
    );
    let capacity = pool
        .add_member(PoolMemberConfig::new(temp.path().join("capacity"), 10_000))
        .expect("capacity");
    pool.record_write(fast, Duration::from_millis(1), 500, true);
    pool.record_write(capacity, Duration::from_millis(100), 500, true);
    let next = vec![0x82; 100];
    let next_hash = sha256(&next);
    pool.put_sync(next_hash, &next)
        .expect("write with headroom");
    assert_eq!(
        pool.blob_location(&next_hash).expect("location"),
        Some(capacity)
    );
}

#[cfg(unix)]
fn spawn_pending_helper(mode: &str, catalog: &Path, ready: &Path, hash: Hash) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("pool::tests::pool_pending_helper")
        .env(HELPER_MODE, mode)
        .env(HELPER_CATALOG, catalog)
        .env(HELPER_READY, ready)
        .env(HELPER_HASH, hashtree_core::to_hex(&hash))
        .env("RUST_TEST_THREADS", "1")
        .spawn()
        .expect("spawn pending helper")
}

#[cfg(unix)]
fn wait_for_file(path: &Path, child: &mut Child) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll helper") {
            panic!("pool helper exited early: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}
