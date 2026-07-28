use super::*;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use tempfile::TempDir;
#[cfg(target_os = "linux")]
use {
    crate::{StoreTotals, STORE_TOTALS_KEY},
    heed::{PinnedLmdbFileIdentity, PinnedLmdbIdentity},
    std::fs::File,
    std::os::fd::AsRawFd,
    std::os::unix::fs::MetadataExt,
};

const HELPER_MODE: &str = "HASHTREE_POOL_HELPER_MODE";
const HELPER_CATALOG: &str = "HASHTREE_POOL_HELPER_CATALOG";
const HELPER_READY: &str = "HASHTREE_POOL_HELPER_READY";
const HELPER_HASH: &str = "HASHTREE_POOL_HELPER_HASH";
const HELPER_SOURCE: &str = "HASHTREE_POOL_HELPER_SOURCE";
const HELPER_TARGET: &str = "HASHTREE_POOL_HELPER_TARGET";
const PENDING_DATA: &[u8] = b"pool pending crash recovery bytes";

#[cfg(target_os = "linux")]
fn test_lmdb_identity(path: &Path) -> PinnedLmdbIdentity {
    let data = fs::metadata(path.join("data.mdb")).expect("data metadata");
    let lock = fs::metadata(path.join("lock.mdb")).expect("lock metadata");
    PinnedLmdbIdentity {
        data: PinnedLmdbFileIdentity {
            device: data.dev(),
            inode: data.ino(),
        },
        lock: PinnedLmdbFileIdentity {
            device: lock.dev(),
            inode: lock.ino(),
        },
    }
}

#[cfg(target_os = "linux")]
fn generated_controlled_config(
    pool: &PoolStore,
    catalog: &Path,
    members: &[(PoolMemberId, PathBuf)],
) -> (PathBuf, PoolStoreConfig, Vec<File>) {
    let manifest_sha256 = pool
        .read_manifest_with_identity()
        .expect("read generated controlled manifest")
        .1;
    let mut retained = Vec::with_capacity(members.len() + 1);
    let catalog_file = File::open(catalog).expect("pin generated controlled catalog");
    let catalog_runtime = PathBuf::from(format!("/proc/self/fd/{}", catalog_file.as_raw_fd()));
    retained.push(catalog_file);
    let mut config = PoolStoreConfig::default();
    config.temperature.enabled = false;
    config.catalog_lmdb_identity = Some(test_lmdb_identity(catalog));
    config.expected_manifest_sha256 = Some(manifest_sha256);
    for (id, path) in members {
        let directory = File::open(path).expect("pin generated controlled member");
        let runtime_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        config.member_runtime_paths.push(PoolMemberRuntimePaths {
            id: *id,
            configured_path: path.clone(),
            runtime_path,
            configured_external_path: None,
            runtime_external_path: None,
            lmdb_identity: test_lmdb_identity(path),
        });
        retained.push(directory);
    }
    (catalog_runtime, config, retained)
}

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
    if mode == "cleanup" {
        let source = std::env::var(HELPER_SOURCE)
            .expect("source id")
            .parse()
            .expect("valid source id");
        let target = std::env::var(HELPER_TARGET)
            .expect("target id")
            .parse()
            .expect("valid target id");
        let expected = pool
            .read_location(&hash)
            .expect("source location")
            .expect("present source location");
        let moving = LocationRecord::Moving {
            source,
            target,
            size: PENDING_DATA.len() as u64,
        };
        assert!(pool
            .begin_move_record(hash, expected, moving)
            .expect("begin cleanup test move"));
        let target_store = pool.get_member(target).expect("open move target");
        pool.write_verified_member(target, &target_store, hash, PENDING_DATA)
            .expect("write move target");
        pool.finish_move_record(hash, source, target, PENDING_DATA.len() as u64)
            .expect("commit target and cleanup ownership");
        fs::write(ready, b"ready").expect("write ready marker");
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
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

#[cfg(unix)]
#[test]
fn process_death_after_target_commit_preserves_cleanup_ownership_index() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let mut config = PoolStoreConfig::default();
    config.temperature.enabled = false;
    let pool = PoolStore::open(&catalog, config.clone()).expect("open pool");
    let source = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("source"),
            1024 * 1024,
        ))
        .expect("source");
    let hash = sha256(PENDING_DATA);
    pool.put_sync(hash, PENDING_DATA).expect("seed source");
    let target = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("target"),
            1024 * 1024,
        ))
        .expect("target");
    pool.begin_drain(source).expect("begin drain");
    drop(pool);

    let ready = temp.path().join("cleanup-ready");
    let mut child = spawn_cleanup_helper(&catalog, &ready, hash, source, target);
    wait_for_file(&ready, &mut child);
    child.kill().expect("kill cleanup helper");
    let output = child.wait_with_output().expect("reap cleanup helper");
    assert!(!output.status.success(), "helper must be killed");

    let recovered = PoolStore::open(&catalog, config).expect("reopen pool");
    assert!(
        recovered.remove_member(source).is_err(),
        "cross-process cleanup ownership must block removal"
    );
    let report = recovered.maintain(1).expect("resume source cleanup");
    assert!(report.failed.is_empty(), "{report:?}");
    recovered
        .remove_member(source)
        .expect("remove after resumed cleanup");
    assert_eq!(
        recovered.get_sync(&hash).expect("read moved blob"),
        Some(PENDING_DATA.to_vec())
    );
}

#[test]
fn committed_candidate_lookup_skips_stored_but_retries_pending_locations() {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open pool");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("member"),
        1024 * 1024,
    ))
    .expect("add member");

    let stored_data = b"committed migration target";
    let stored = sha256(stored_data);
    pool.put_sync(stored, stored_data)
        .expect("store committed blob");

    let pending = sha256(PENDING_DATA);
    let target = pool
        .choose_write_member(PENDING_DATA.len() as u64, None)
        .expect("choose pending member");
    pool.reserve_if_absent(
        pending,
        LocationRecord::Pending {
            member: target,
            size: PENDING_DATA.len() as u64,
        },
    )
    .expect("reserve pending location");

    let missing = sha256(b"missing migration target");
    let mut hashes = vec![stored, pending, missing];
    hashes.sort_unstable();
    let committed = pool
        .committed_hashes_in_sorted_candidates(&hashes)
        .expect("lookup committed candidates");
    let status = hashes
        .into_iter()
        .zip(committed)
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(status.get(&stored), Some(&true));
    assert_eq!(status.get(&pending), Some(&false));
    assert_eq!(status.get(&missing), Some(&false));
}

#[test]
fn empty_draining_member_removal_uses_an_exact_index_probe() {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open pool");
    let source = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("source"),
            1024 * 1024,
        ))
        .expect("source");
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("target"),
        1024 * 1024,
    ))
    .expect("target");
    pool.begin_drain(source).expect("begin drain");

    pool.remove_member(source).expect("remove empty member");
    assert!(pool.member(source).is_err());
}

#[test]
fn indexed_member_emptiness_rejects_pending_stored_and_moving_locations() {
    for state in ["pending", "stored", "moving"] {
        let temp = TempDir::new().expect("temp dir");
        let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
            .expect("open pool");
        let source = pool
            .add_member(PoolMemberConfig::new(
                temp.path().join("source"),
                1024 * 1024,
            ))
            .expect("source");
        let target = pool
            .add_member(PoolMemberConfig::new(
                temp.path().join("target"),
                1024 * 1024,
            ))
            .expect("target");
        pool.begin_drain(source).expect("begin drain");
        let hash = sha256(state.as_bytes());
        let location = match state {
            "pending" => LocationRecord::Pending {
                member: source,
                size: 1,
            },
            "stored" => LocationRecord::Stored {
                member: source,
                size: 1,
            },
            "moving" => LocationRecord::Moving {
                source,
                target,
                size: 1,
            },
            _ => unreachable!(),
        };
        let mut wtxn = pool.env.write_txn().expect("catalog write txn");
        pool.set_location_txn(&mut wtxn, hash, Some(location))
            .expect("install location");
        wtxn.commit().expect("commit location");

        let error = pool
            .remove_member(source)
            .expect_err("located member must not be removed");
        assert!(error.to_string().contains("still owns"), "{state}: {error}");
        assert_eq!(
            pool.read_location(&hash).expect("location after rejection"),
            Some(location)
        );
    }
}

#[test]
fn member_location_probe_respects_full_uuid_prefix_boundaries() {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open pool");
    let mut wanted_bytes = [0x40; 16];
    wanted_bytes[15] = 0x80;
    let wanted = PoolMemberId(wanted_bytes);
    let mut lower_bytes = wanted_bytes;
    lower_bytes[15] -= 1;
    let lower = PoolMemberId(lower_bytes);
    let mut upper_bytes = wanted_bytes;
    upper_bytes[15] += 1;
    let upper = PoolMemberId(upper_bytes);
    let hash = sha256(b"member prefix boundary");

    let mut wtxn = pool.env.write_txn().expect("catalog write txn");
    pool.by_member
        .put(&mut wtxn, &member_hash_key(lower, hash), &())
        .expect("lower index key");
    pool.by_member
        .put(&mut wtxn, &member_hash_key(upper, hash), &())
        .expect("upper index key");
    assert!(
        !pool
            .member_has_locations_txn(&wtxn, wanted)
            .expect("probe empty exact prefix"),
        "adjacent UUID prefixes must not alias"
    );
    pool.by_member
        .put(&mut wtxn, &member_hash_key(wanted, hash), &())
        .expect("wanted index key");
    assert!(pool
        .member_has_locations_txn(&wtxn, wanted)
        .expect("probe occupied exact prefix"));
    wtxn.commit().expect("commit boundary keys");
}

#[test]
fn pending_cleanup_ownership_rebuilds_after_crash_and_clears_before_removal() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let mut config = PoolStoreConfig::default();
    config.temperature.enabled = false;
    let pool = PoolStore::open(&catalog, config.clone()).expect("open pool");
    let source = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("source"),
            1024 * 1024,
        ))
        .expect("source");
    let target = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("target"),
            1024 * 1024,
        ))
        .expect("target");
    pool.begin_drain(source).expect("begin drain");
    let hash = sha256(b"pending source cleanup");
    let moving = LocationRecord::Moving {
        source,
        target,
        size: 1,
    };
    let mut wtxn = pool.env.write_txn().expect("catalog write txn");
    pool.set_location_txn(&mut wtxn, hash, Some(moving))
        .expect("install moving location");
    wtxn.commit().expect("commit moving location");
    pool.finish_move_records(&[(hash, source, target, 1)])
        .expect("finish move and retain cleanup ownership");
    assert!(matches!(
        pool.read_location(&hash).expect("stored target location"),
        Some(LocationRecord::Stored { member, size: 1 }) if member == target
    ));
    let rtxn = pool.env.read_txn().expect("catalog read txn");
    assert!(pool
        .member_has_locations_txn(&rtxn, source)
        .expect("cleanup ownership prefix"));
    drop(rtxn);

    // Simulate a cleanup record committed by the pre-index format, then crash.
    let mut wtxn = pool.env.write_txn().expect("legacy cleanup write txn");
    pool.by_member
        .delete(&mut wtxn, &member_hash_key(source, hash))
        .expect("remove cleanup ownership index");
    wtxn.commit().expect("commit legacy cleanup state");
    drop(pool);

    let reopened = PoolStore::open(&catalog, config).expect("reopen after cleanup crash");
    let error = reopened
        .remove_member(source)
        .expect_err("cleanup source must remain configured");
    assert!(error.to_string().contains("still owns"));
    reopened
        .clear_move_cleanup_records(&[(hash, source, target, 1)])
        .expect("clear source cleanup");
    let rtxn = reopened.env.read_txn().expect("catalog read txn");
    assert!(!reopened
        .member_has_locations_txn(&rtxn, source)
        .expect("cleared cleanup ownership prefix"));
    drop(rtxn);
    reopened
        .remove_member(source)
        .expect("remove after cleanup ownership clears");
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
fn writable_physical_stats_sum_real_member_totals_without_catalog_enumeration() {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())
        .expect("open pool");
    let first = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("first"),
            1024 * 1024,
        ))
        .expect("add first member");
    let second = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("second"),
            1024 * 1024,
        ))
        .expect("add second member");

    let data = b"physical pool accounting";
    let hash = sha256(data);
    pool.put_sync(hash, data).expect("write catalog-owned blob");
    let duplicate_member = if pool.blob_location(&hash).expect("location") == Some(first) {
        second
    } else {
        first
    };
    pool.get_member(duplicate_member)
        .expect("open duplicate member")
        .put_sync(hash, data)
        .expect("write real duplicate");

    let logical = pool.stats().expect("logical stats");
    assert_eq!(logical.count, 1);
    assert_eq!(logical.bytes, data.len() as u64);

    let physical = pool
        .writable_physical_stats()
        .expect("physical member stats");
    assert_eq!(physical.count, 2);
    assert_eq!(physical.bytes, (data.len() * 2) as u64);
}

#[test]
fn writable_physical_stats_fail_closed_when_a_member_is_unavailable() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let member = temp.path().join("member");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open pool");
    pool.add_member(PoolMemberConfig::new(member.clone(), 1024 * 1024))
        .expect("add member");
    drop(pool);

    fs::rename(&member, temp.path().join("member-offline")).expect("take member offline");
    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("reopen catalog");
    let error = reopened
        .writable_physical_stats()
        .expect_err("quota accounting must fail closed");
    assert!(
        error.to_string().contains("unavailable"),
        "unexpected error: {error}"
    );
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
fn batch_delete_removes_member_blobs_and_catalog_locations() {
    let temp = TempDir::new().expect("temp dir");
    let pool =
        PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default()).expect("pool");
    let member = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("member"),
            1024 * 1024,
        ))
        .expect("member");
    let first = sha256(b"pool-batch-delete-first");
    let second = sha256(b"pool-batch-delete-second");
    let retained = sha256(b"pool-batch-delete-retained");
    pool.put_sync(first, b"pool-batch-delete-first")
        .expect("put first");
    pool.put_sync(second, b"pool-batch-delete-second")
        .expect("put second");
    pool.put_sync(retained, b"pool-batch-delete-retained")
        .expect("put retained");

    assert_eq!(
        pool.delete_many_sync(&[first, second, first])
            .expect("batch delete"),
        2
    );
    assert!(pool
        .read_location(&first)
        .expect("first location")
        .is_none());
    assert!(pool
        .read_location(&second)
        .expect("second location")
        .is_none());
    assert_eq!(
        pool.read_location(&retained).expect("retained location"),
        Some(LocationRecord::Stored {
            member,
            size: b"pool-batch-delete-retained".len() as u64,
        })
    );
    let member_store = pool.get_member(member).expect("member store");
    assert!(!member_store.exists(&first).expect("first member lookup"));
    assert!(!member_store.exists(&second).expect("second member lookup"));
    assert!(member_store
        .exists(&retained)
        .expect("retained member lookup"));
}

#[cfg(target_os = "linux")]
#[test]
fn exact_offline_stale_pending_cleanup_is_atomic_and_idempotent() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let member_path = temp.path().join("member");
    let mut ordinary_config = PoolStoreConfig::default();
    ordinary_config.temperature.enabled = false;
    let pool = PoolStore::open(&catalog, ordinary_config.clone()).expect("open Pool");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 16 * 1024 * 1024))
        .expect("add member");
    let missing_hash = sha256(b"crash-abandoned Pending without member bytes");
    let present_data = b"Pending whose member write actually committed";
    let present_hash = sha256(present_data);
    pool.get_member(member)
        .expect("member")
        .put_sync(present_hash, present_data)
        .expect("write physically present Pending");
    let mut wtxn = pool.env.write_txn().expect("catalog transaction");
    for (hash, size) in [
        (missing_hash, 91u64),
        (present_hash, present_data.len() as u64),
    ] {
        pool.set_location_txn(
            &mut wtxn,
            hash,
            Some(LocationRecord::Pending { member, size }),
        )
        .expect("write Pending record");
    }
    wtxn.commit().expect("commit Pending records");
    pool.force_sync().expect("sync generated crash state");
    drop(pool);

    let reader = PoolStoreReader::open(&catalog, ordinary_config).expect("manifest reader");
    let manifest_sha256 = reader.manifest_identity().sha256;
    drop(reader);
    let catalog_identity = test_lmdb_identity(&catalog);
    let member_identity = test_lmdb_identity(&member_path);
    let catalog_fd = File::open(&catalog).expect("pin catalog directory");
    let member_fd = File::open(&member_path).expect("pin member directory");
    let catalog_runtime = PathBuf::from(format!("/proc/self/fd/{}", catalog_fd.as_raw_fd()));
    let member_runtime = PathBuf::from(format!("/proc/self/fd/{}", member_fd.as_raw_fd()));
    let mut controlled_config = PoolStoreConfig::default();
    controlled_config.temperature.enabled = false;
    controlled_config.catalog_lmdb_identity = Some(catalog_identity);
    controlled_config.expected_manifest_sha256 = Some(manifest_sha256);
    controlled_config.member_runtime_paths = vec![PoolMemberRuntimePaths {
        id: member,
        configured_path: member_path,
        runtime_path: member_runtime,
        configured_external_path: None,
        runtime_external_path: None,
        lmdb_identity: member_identity,
    }];
    let controlled =
        PoolStore::open(&catalog_runtime, controlled_config).expect("controlled Pool open");
    let mut expected = vec![
        PoolStalePending {
            hash: missing_hash,
            member,
            size: 91,
        },
        PoolStalePending {
            hash: present_hash,
            member,
            size: present_data.len() as u64,
        },
    ];
    expected.sort_unstable_by_key(|item| item.hash);

    let present_error = controlled
        .cleanup_stale_pending_exact_offline_sync(&expected)
        .expect_err("physically present Pending must block the whole cleanup");
    assert!(present_error.to_string().contains("physically present"));
    assert!(controlled
        .read_location(&missing_hash)
        .expect("missing location")
        .is_some());

    controlled
        .get_member(member)
        .expect("controlled member")
        .delete_sync(&present_hash)
        .expect("remove generated member bytes");
    let report = controlled
        .cleanup_stale_pending_exact_offline_sync(&expected)
        .expect("exact offline cleanup");
    assert_eq!(report.requested, 2);
    assert_eq!(
        report.declared_bytes,
        91 + u64::try_from(present_data.len()).expect("size")
    );
    assert!(!report.already_cleaned);
    assert!(controlled
        .read_location(&missing_hash)
        .expect("cleaned missing location")
        .is_none());
    assert!(controlled
        .read_location(&present_hash)
        .expect("cleaned present location")
        .is_none());

    let replay = controlled
        .cleanup_stale_pending_exact_offline_sync(&expected)
        .expect("idempotent exact replay");
    assert!(replay.already_cleaned);
}

#[cfg(target_os = "linux")]
#[test]
fn controlled_writer_resumes_pending_before_and_after_member_write() {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let member_path = temp.path().join("member");
    let mut ordinary_config = PoolStoreConfig::default();
    ordinary_config.temperature.enabled = false;
    let pool = PoolStore::open(&catalog, ordinary_config.clone()).expect("open ordinary Pool");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 16 * 1024 * 1024))
        .expect("add member");

    let absent_data = b"controlled Pending whose member write never started";
    let absent_hash = sha256(absent_data);
    let present_data = b"controlled Pending whose member write already committed";
    let present_hash = sha256(present_data);
    {
        let member_store = pool.get_member(member).expect("open ordinary member");
        assert_eq!(
            member_store
                .blob_size_sync(&absent_hash)
                .expect("absent member size"),
            None
        );
        member_store
            .put_sync(present_hash, present_data)
            .expect("write physically present Pending body");
    }
    let mut wtxn = pool.env.write_txn().expect("catalog write transaction");
    for (hash, size) in [
        (absent_hash, absent_data.len() as u64),
        (present_hash, present_data.len() as u64),
    ] {
        pool.set_location_txn(
            &mut wtxn,
            hash,
            Some(LocationRecord::Pending { member, size }),
        )
        .expect("inject Pending location");
    }
    wtxn.commit().expect("commit Pending locations");
    pool.force_sync().expect("sync generated crash states");
    drop(pool);

    let ordinary_reader =
        PoolStoreReader::open(&catalog, ordinary_config).expect("read exact manifest identity");
    let manifest_sha256 = ordinary_reader.manifest_identity().sha256;
    drop(ordinary_reader);
    let catalog_identity = test_lmdb_identity(&catalog);
    let member_identity = test_lmdb_identity(&member_path);
    let catalog_fd = File::open(&catalog).expect("pin catalog directory");
    let member_fd = File::open(&member_path).expect("pin member directory");
    let catalog_runtime = PathBuf::from(format!("/proc/self/fd/{}", catalog_fd.as_raw_fd()));
    let member_runtime = PathBuf::from(format!("/proc/self/fd/{}", member_fd.as_raw_fd()));
    let mut controlled_config = PoolStoreConfig::default();
    controlled_config.temperature.enabled = false;
    controlled_config.catalog_lmdb_identity = Some(catalog_identity);
    controlled_config.expected_manifest_sha256 = Some(manifest_sha256);
    controlled_config.member_runtime_paths = vec![PoolMemberRuntimePaths {
        id: member,
        configured_path: member_path,
        runtime_path: member_runtime,
        configured_external_path: None,
        runtime_external_path: None,
        lmdb_identity: member_identity,
    }];
    let audit_config = controlled_config.clone();
    let controlled =
        PoolStore::open(&catalog_runtime, controlled_config).expect("open exact controlled writer");

    assert_eq!(
        controlled
            .read_location(&absent_hash)
            .expect("absent-body Pending location"),
        Some(LocationRecord::Pending {
            member,
            size: absent_data.len() as u64,
        })
    );
    assert_eq!(
        controlled
            .read_location(&present_hash)
            .expect("present-body Pending location"),
        Some(LocationRecord::Pending {
            member,
            size: present_data.len() as u64,
        })
    );
    assert!(controlled
        .put_sync(absent_hash, absent_data)
        .expect("repair absent-body Pending"));
    assert!(!controlled
        .put_sync(present_hash, present_data)
        .expect("finalize present-body Pending"));
    controlled.force_sync().expect("sync repaired Pool");
    for (hash, size) in [
        (absent_hash, absent_data.len() as u64),
        (present_hash, present_data.len() as u64),
    ] {
        assert_eq!(
            controlled
                .read_location(&hash)
                .expect("read committed location"),
            Some(LocationRecord::Stored { member, size })
        );
    }
    drop(controlled);

    let reader = ReadOnlyPoolStore::open_controlled(&catalog_runtime, audit_config)
        .expect("open exact read-only verifier");
    let audit = reader
        .validate_committed_catalog()
        .expect("prove committed catalog");
    assert_eq!(audit.stored_locations, 2);
    assert_eq!(
        reader.get_sync(&absent_hash).expect("read repaired body"),
        Some(absent_data.to_vec())
    );
    assert_eq!(
        reader.get_sync(&present_hash).expect("read finalized body"),
        Some(present_data.to_vec())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn controlled_open_is_data_nonmutating_and_rejects_missing_or_stale_member_stats() {
    for state in ["valid", "missing", "stale"] {
        let temp = TempDir::new().expect("create generated controlled-open root");
        let catalog = temp.path().join("catalog");
        let member_path = temp.path().join("member");
        let mut ordinary_config = PoolStoreConfig::default();
        ordinary_config.temperature.enabled = false;
        let pool =
            PoolStore::open(&catalog, ordinary_config).expect("open generated ordinary Pool");
        let member = pool
            .add_member(PoolMemberConfig::new(member_path.clone(), 16 * 1024 * 1024))
            .expect("add generated member");
        let data = format!("generated controlled-open {state}").into_bytes();
        pool.put_sync(sha256(&data), &data)
            .expect("write generated controlled-open blob");

        if state != "valid" {
            let store = pool.get_member(member).expect("open generated member");
            let mut wtxn = store.env.write_txn().expect("open generated stats txn");
            match state {
                "missing" => {
                    store
                        .stats
                        .delete(&mut wtxn, STORE_TOTALS_KEY)
                        .expect("delete generated totals");
                }
                "stale" => {
                    LmdbBlobStore::write_store_totals(
                        store.stats,
                        &mut wtxn,
                        StoreTotals::default(),
                    )
                    .expect("write stale generated totals");
                }
                _ => unreachable!(),
            }
            wtxn.commit().expect("commit generated stats state");
            store.force_sync().expect("sync generated member stats");
        }
        pool.force_sync().expect("sync generated Pool");
        let (catalog_runtime, controlled_config, retained) =
            generated_controlled_config(&pool, &catalog, &[(member, member_path.clone())]);
        drop(pool);
        let catalog_before =
            fs::read(catalog.join("data.mdb")).expect("snapshot generated catalog data");
        let member_before =
            fs::read(member_path.join("data.mdb")).expect("snapshot generated member data");

        let opened = PoolStore::open(&catalog_runtime, controlled_config);
        match state {
            "valid" => {
                drop(opened.expect("open exact nonmutating controlled Pool"));
            }
            "missing" => {
                let error = opened
                    .err()
                    .expect("missing member totals must fail closed");
                assert!(
                    error
                        .to_string()
                        .contains("missing persisted aggregate totals"),
                    "unexpected missing-stat error: {error}"
                );
            }
            "stale" => {
                let error = opened.err().expect("stale member totals must fail closed");
                assert!(
                    error
                        .to_string()
                        .contains("secondary/stat counts are stale"),
                    "unexpected stale-stat error: {error}"
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            fs::read(catalog.join("data.mdb")).expect("reinspect generated catalog data"),
            catalog_before,
            "controlled open changed catalog data for {state} member stats"
        );
        assert_eq!(
            fs::read(member_path.join("data.mdb")).expect("reinspect generated member data"),
            member_before,
            "controlled open changed member data for {state} member stats"
        );
        drop(retained);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn exact_controlled_and_terminal_audits_reject_byte_only_stale_member_totals() {
    let temp = TempDir::new().expect("create generated byte-total audit root");
    let catalog = temp.path().join("catalog");
    let member_path = temp.path().join("member");
    let mut ordinary_config = PoolStoreConfig::default();
    ordinary_config.temperature.enabled = false;
    let pool = PoolStore::open(&catalog, ordinary_config).expect("open generated ordinary Pool");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 16 * 1024 * 1024))
        .expect("add generated member");
    let data = b"generated byte-only stale aggregate";
    pool.put_sync(sha256(data), data)
        .expect("write generated controlled-open blob");

    let store = pool.get_member(member).expect("open generated member");
    let mut wtxn = store.env.write_txn().expect("open generated stats txn");
    LmdbBlobStore::write_store_totals(
        store.stats,
        &mut wtxn,
        StoreTotals {
            count: 1,
            total_bytes: 0,
            pinned_count: 0,
            pinned_bytes: 0,
        },
    )
    .expect("write byte-only stale generated totals");
    wtxn.commit().expect("commit generated stats state");
    store.force_sync().expect("sync generated member stats");
    drop(store);
    pool.force_sync().expect("sync generated Pool");
    let (catalog_runtime, controlled_config, retained) =
        generated_controlled_config(&pool, &catalog, &[(member, member_path.clone())]);
    drop(pool);
    let catalog_before =
        fs::read(catalog.join("data.mdb")).expect("snapshot generated catalog data");
    let member_before =
        fs::read(member_path.join("data.mdb")).expect("snapshot generated member data");

    let controlled = PoolStore::open(&catalog_runtime, controlled_config.clone())
        .expect("constant-time controlled open accepts matching counts");
    let error = controlled
        .validate_controlled_member_state_exact()
        .expect_err("exact pre-mutation audit must reject stale byte total");
    assert!(
        error
            .to_string()
            .contains("persisted aggregate totals are stale"),
        "unexpected exact aggregate error: {error}"
    );
    drop(controlled);

    let terminal = PoolStoreReader::open(&catalog_runtime, controlled_config)
        .expect("open exact read-only terminal reader");
    let error = terminal
        .validate_terminal_catalog_and_payloads()
        .expect_err("terminal audit must reject stale byte total");
    assert!(
        error
            .to_string()
            .contains("persisted aggregate totals are stale"),
        "unexpected terminal aggregate error: {error}"
    );
    drop(terminal);

    assert_eq!(
        fs::read(catalog.join("data.mdb")).expect("reinspect generated catalog data"),
        catalog_before,
        "exact aggregate validation changed catalog data"
    );
    assert_eq!(
        fs::read(member_path.join("data.mdb")).expect("reinspect generated member data"),
        member_before,
        "exact aggregate validation changed member data"
    );
    drop(retained);
}

#[cfg(target_os = "linux")]
#[test]
fn controlled_open_rejects_missing_cleanup_index_without_repairing_it() {
    let temp = TempDir::new().expect("create generated cleanup-index root");
    let catalog = temp.path().join("catalog");
    let source_path = temp.path().join("source");
    let target_path = temp.path().join("target");
    let mut ordinary_config = PoolStoreConfig::default();
    ordinary_config.temperature.enabled = false;
    let pool = PoolStore::open(&catalog, ordinary_config).expect("open generated Pool");
    let source = pool
        .add_member(PoolMemberConfig::new(source_path.clone(), 16 * 1024 * 1024))
        .expect("add generated source member");
    let target = pool
        .add_member(PoolMemberConfig::new(target_path.clone(), 16 * 1024 * 1024))
        .expect("add generated target member");
    let hash = sha256(b"generated missing cleanup index");
    let moving = LocationRecord::Moving {
        source,
        target,
        size: 1,
    };
    let mut wtxn = pool.env.write_txn().expect("open generated catalog txn");
    pool.set_location_txn(&mut wtxn, hash, Some(moving))
        .expect("write generated moving record");
    wtxn.commit().expect("commit generated moving record");
    pool.finish_move_records(&[(hash, source, target, 1)])
        .expect("finish generated move");
    let mut wtxn = pool
        .env
        .write_txn()
        .expect("open generated stale-index txn");
    pool.by_member
        .delete(&mut wtxn, &member_hash_key(source, hash))
        .expect("remove generated cleanup index");
    wtxn.commit().expect("commit generated missing index");
    pool.force_sync().expect("sync generated missing index");
    let (catalog_runtime, controlled_config, retained) = generated_controlled_config(
        &pool,
        &catalog,
        &[(source, source_path), (target, target_path)],
    );
    drop(pool);
    let catalog_before =
        fs::read(catalog.join("data.mdb")).expect("snapshot missing-index catalog");

    let error = PoolStore::open(&catalog_runtime, controlled_config)
        .err()
        .expect("controlled open must not repair a missing cleanup index");
    assert!(
        error
            .to_string()
            .contains("missing move-cleanup member index"),
        "unexpected cleanup-index error: {error}"
    );
    assert_eq!(
        fs::read(catalog.join("data.mdb")).expect("reinspect missing-index catalog"),
        catalog_before,
        "controlled open repaired the missing cleanup index before authorization"
    );
    drop(retained);
}

#[cfg(target_os = "linux")]
#[test]
fn controlled_open_rejects_undersized_catalog_without_resizing_it() {
    let temp = TempDir::new().expect("create generated map-size root");
    let catalog = temp.path().join("catalog");
    let member_path = temp.path().join("member");
    let mut ordinary_config = PoolStoreConfig::default();
    ordinary_config.temperature.enabled = false;
    let pool = PoolStore::open(&catalog, ordinary_config).expect("open generated Pool");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 16 * 1024 * 1024))
        .expect("add generated member");
    pool.force_sync().expect("sync generated map-size Pool");
    let actual_map_size = u64::try_from(pool.env.info().map_size).expect("catalog map size");
    let (catalog_runtime, mut controlled_config, retained) =
        generated_controlled_config(&pool, &catalog, &[(member, member_path)]);
    controlled_config.catalog_map_size_bytes =
        actual_map_size.saturating_add(MIN_MEMBER_MAP_SIZE_BYTES);
    drop(pool);
    let catalog_before = fs::read(catalog.join("data.mdb")).expect("snapshot undersized catalog");

    let error = PoolStore::open(&catalog_runtime, controlled_config)
        .err()
        .expect("controlled open must not resize an undersized catalog");
    assert!(
        error
            .to_string()
            .contains("pre-size it before exact migration"),
        "unexpected map-size error: {error}"
    );
    assert_eq!(
        fs::read(catalog.join("data.mdb")).expect("reinspect undersized catalog"),
        catalog_before,
        "controlled open resized catalog data before authorization"
    );
    drop(retained);
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

fn spawn_cleanup_helper(
    catalog: &Path,
    ready: &Path,
    hash: Hash,
    source: PoolMemberId,
    target: PoolMemberId,
) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .arg("--ignored")
        .arg("--exact")
        .arg("pool::tests::pool_pending_helper")
        .env(HELPER_MODE, "cleanup")
        .env(HELPER_CATALOG, catalog)
        .env(HELPER_READY, ready)
        .env(HELPER_HASH, hashtree_core::to_hex(&hash))
        .env(HELPER_SOURCE, source.to_string())
        .env(HELPER_TARGET, target.to_string())
        .env("RUST_TEST_THREADS", "1")
        .spawn()
        .expect("spawn cleanup helper")
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
