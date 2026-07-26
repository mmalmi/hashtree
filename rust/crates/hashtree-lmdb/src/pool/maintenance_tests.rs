use super::maintenance_batch::MovePlan;
use super::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn count_files_under(path: &Path) -> usize {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                count_files_under(&path)
            } else {
                1
            }
        })
        .sum()
}

fn config_without_temperature() -> PoolStoreConfig {
    let mut config = PoolStoreConfig::default();
    config.temperature.enabled = false;
    config
}

fn pool_without_temperature(path: &Path) -> PoolStore {
    PoolStore::open(path, config_without_temperature()).expect("open pool")
}

#[test]
fn draining_to_a_packed_member_commits_one_verified_batch_pack() {
    let temp = TempDir::new().expect("temp dir");
    let pool = pool_without_temperature(&temp.path().join("catalog"));
    let source_external = temp.path().join("source-external");
    let source = pool
        .add_member(
            PoolMemberConfig::new(temp.path().join("source"), 1024 * 1024).with_external_blobs(
                source_external.clone(),
                1,
                true,
                Some(1024 * 1024),
            ),
        )
        .expect("source member");
    let blobs = (0..4u8)
        .map(|value| {
            let data = vec![value; 4096];
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    pool.put_many_sync(&blobs).expect("seed source");
    assert_eq!(count_files_under(&source_external.join("packs")), 1);
    let target_external = temp.path().join("target-external");
    let target = pool
        .add_member(
            PoolMemberConfig::new(temp.path().join("target"), 1024 * 1024).with_external_blobs(
                target_external.clone(),
                1,
                true,
                Some(1024 * 1024),
            ),
        )
        .expect("target member");
    pool.begin_drain(source).expect("begin drain");

    let report = pool.maintain(blobs.len()).expect("maintain");

    assert_eq!(report.moved, blobs.len(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(count_files_under(&target_external.join("packs")), 1);
    let source_store = pool.get_member(source).expect("source store");
    for (hash, data) in blobs {
        assert_eq!(pool.blob_location(&hash).expect("location"), Some(target));
        assert_eq!(pool.get_sync(&hash).expect("pool read"), Some(data));
        assert_eq!(
            source_store.blob_size_sync(&hash).expect("source lookup"),
            None
        );
    }
}

#[test]
fn packed_drain_resumes_after_every_durable_move_boundary() {
    for boundary in [
        "moving-catalog",
        "target-commit",
        "target-verified",
        "stored-catalog",
        "source-deleted",
    ] {
        let temp = TempDir::new().expect("temp dir");
        let catalog = temp.path().join("catalog");
        let pool = pool_without_temperature(&catalog);
        let source = pool
            .add_member(PoolMemberConfig::new(
                temp.path().join("source"),
                1024 * 1024,
            ))
            .expect("source member");
        let blobs = (0..3u8)
            .map(|value| {
                let data = vec![value.saturating_add(1); 8192];
                (sha256(&data), data)
            })
            .collect::<Vec<_>>();
        pool.put_many_sync(&blobs).expect("seed source");
        let target_external = temp.path().join("target-external");
        let target = pool
            .add_member(
                PoolMemberConfig::new(temp.path().join("target"), 1024 * 1024).with_external_blobs(
                    target_external.clone(),
                    1,
                    true,
                    Some(1024 * 1024),
                ),
            )
            .expect("target member");
        pool.begin_drain(source).expect("begin drain");
        let plans = blobs
            .iter()
            .map(|(hash, data)| {
                let expected = pool
                    .read_location(hash)
                    .expect("location")
                    .expect("stored location");
                MovePlan {
                    hash: *hash,
                    source,
                    target,
                    size: data.len() as u64,
                    expected,
                }
            })
            .collect::<Vec<_>>();
        let transitions = plans
            .iter()
            .map(|plan| (plan.hash, plan.expected, plan.moving()))
            .collect::<Vec<_>>();
        assert_eq!(
            pool.begin_move_records(&transitions)
                .expect("begin move records")
                .len(),
            blobs.len()
        );

        if boundary != "moving-catalog" {
            let target_store = pool.get_member(target).expect("target store");
            let refs = blobs
                .iter()
                .map(|(hash, data)| (*hash, data.as_slice()))
                .collect::<Vec<_>>();
            target_store
                .put_many_refs_report_sync(&refs)
                .expect("commit target pack");
            if boundary != "target-commit" {
                for (hash, data) in &blobs {
                    target_store
                        .verify_blob_streaming(hash, data.len() as u64, 1024)
                        .expect("verify target");
                }
            }
            if matches!(boundary, "stored-catalog" | "source-deleted") {
                let catalog_plans = plans
                    .iter()
                    .copied()
                    .map(MovePlan::catalog_tuple)
                    .collect::<Vec<_>>();
                pool.finish_move_records(&catalog_plans)
                    .expect("finish catalog batch");
                if boundary == "source-deleted" {
                    let hashes = blobs.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
                    pool.get_member(source)
                        .expect("source store")
                        .delete_many_sync(&hashes)
                        .expect("delete source batch");
                    let error = pool
                        .remove_member(source)
                        .expect_err("cleanup intent must block member removal");
                    assert!(
                        error.to_string().contains("pending source cleanup"),
                        "{error}"
                    );
                }
            }
        }
        drop(pool);

        let reopened =
            PoolStore::open(&catalog, config_without_temperature()).expect("reopen pool");
        let report = reopened
            .maintain_with_batch_items(blobs.len(), blobs.len())
            .expect("resume maintenance");
        assert_eq!(report.moved, blobs.len(), "{boundary}: {report:?}");
        assert!(report.failed.is_empty(), "{boundary}: {report:?}");
        assert!(
            reopened
                .active_moves(blobs.len())
                .expect("active moves")
                .is_empty(),
            "{boundary}"
        );
        assert!(
            reopened
                .active_move_cleanups(blobs.len())
                .expect("active cleanups")
                .is_empty(),
            "{boundary}"
        );
        let source_store = reopened.get_member(source).expect("source store");
        for (hash, data) in &blobs {
            assert_eq!(
                reopened.blob_location(hash).expect("location"),
                Some(target),
                "{boundary}"
            );
            assert_eq!(
                reopened.get_sync(hash).expect("pool read"),
                Some(data.clone()),
                "{boundary}"
            );
            assert_eq!(
                source_store.blob_size_sync(hash).expect("source lookup"),
                None,
                "{boundary}"
            );
        }
        assert_eq!(
            count_files_under(&target_external.join("packs")),
            1,
            "{boundary}"
        );
        reopened
            .remove_member(source)
            .expect("remove fully cleaned source");
    }
}

#[test]
fn maintenance_batch_items_bounds_each_target_pack_commit() {
    let temp = TempDir::new().expect("temp dir");
    let pool = pool_without_temperature(&temp.path().join("catalog"));
    let source = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("source"),
            1024 * 1024,
        ))
        .expect("source member");
    let blobs = (0..5u8)
        .map(|value| {
            let data = vec![value.saturating_add(1); 4096];
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    pool.put_many_sync(&blobs).expect("seed source");
    let target_external = temp.path().join("target-external");
    pool.add_member(
        PoolMemberConfig::new(temp.path().join("target"), 1024 * 1024).with_external_blobs(
            target_external.clone(),
            1,
            true,
            Some(1024 * 1024),
        ),
    )
    .expect("target member");
    pool.begin_drain(source).expect("begin drain");

    let report = pool
        .maintain_with_batch_items(blobs.len(), 2)
        .expect("maintain");

    assert_eq!(report.moved, blobs.len(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(count_files_under(&target_external.join("packs")), 3);
}
