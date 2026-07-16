use hashtree_core::{sha256, StoreError};
use hashtree_lmdb::{LmdbBlobStore, PoolMemberConfig, PoolMemberState, PoolStore, PoolStoreConfig};
use tempfile::TempDir;

fn member(path: impl Into<std::path::PathBuf>, capacity_bytes: u64) -> PoolMemberConfig {
    PoolMemberConfig::new(path.into(), capacity_bytes)
}

#[test]
fn pool_persists_exact_blob_locations_and_reopens() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let first_path = temp.path().join("first");
    let second_path = temp.path().join("second");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    let first = pool.add_member(member(&first_path, 1024 * 1024))?;

    let first_data = b"first member blob".repeat(32);
    let first_hash = sha256(&first_data);
    assert!(pool.put_sync(first_hash, &first_data)?);
    assert_eq!(pool.blob_location(&first_hash)?, Some(first));

    let second = pool.add_member(member(&second_path, 1024 * 1024))?;
    let second_data = b"second member blob".repeat(32);
    let second_hash = sha256(&second_data);
    assert!(pool.put_sync(second_hash, &second_data)?);
    let second_location = pool
        .blob_location(&second_hash)?
        .expect("second blob location");
    assert!(second_location == first || second_location == second);
    assert_eq!(pool.get_sync(&first_hash)?, Some(first_data.clone()));
    assert_eq!(pool.get_sync(&second_hash)?, Some(second_data.clone()));
    drop(pool);

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    assert_eq!(reopened.blob_location(&first_hash)?, Some(first));
    assert_eq!(reopened.blob_location(&second_hash)?, Some(second_location));
    assert_eq!(reopened.get_sync(&first_hash)?, Some(first_data));
    assert_eq!(reopened.get_sync(&second_hash)?, Some(second_data));
    Ok(())
}

#[test]
fn draining_member_moves_and_verifies_every_blob_before_removal() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    let source = pool.add_member(member(temp.path().join("source"), 1024 * 1024))?;

    let blobs = (0..24)
        .map(|index| {
            let data = format!("drain blob {index:04}").repeat(32).into_bytes();
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    for (hash, data) in &blobs {
        assert!(pool.put_sync(*hash, data)?);
        assert_eq!(pool.blob_location(hash)?, Some(source));
    }

    let target = pool.add_member(member(temp.path().join("target"), 1024 * 1024))?;
    pool.begin_drain(source)?;
    assert_eq!(pool.member(source)?.state, PoolMemberState::Draining);

    let mut moved = 0usize;
    while pool.member(source)?.located_blobs > 0 {
        let report = pool.maintain(5)?;
        assert!(
            report.failed.is_empty(),
            "maintenance errors: {:?}",
            report.failed
        );
        assert!(report.moved > 0, "drain must make bounded progress");
        assert!(report.moved <= 5, "maintenance must honor its item bound");
        moved += report.moved;
    }
    assert_eq!(moved, blobs.len());

    for (hash, data) in &blobs {
        assert_eq!(pool.blob_location(hash)?, Some(target));
        assert_eq!(pool.get_sync(hash)?, Some(data.clone()));
    }
    pool.remove_member(source)?;
    assert!(pool.member(source).is_err());
    Ok(())
}

#[test]
fn pool_rejects_mismatched_writes_and_corrupt_member_bytes() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let member_path = temp.path().join("member");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    let owner = pool.add_member(member(&member_path, 1024 * 1024))?;

    let data = b"valid pool bytes".repeat(32);
    let hash = sha256(&data);
    let wrong_hash = sha256(b"different bytes");
    assert!(pool.put_sync(wrong_hash, &data).is_err());
    assert_eq!(pool.blob_location(&wrong_hash)?, None);

    assert!(pool.put_sync(hash, &data)?);
    assert_eq!(pool.blob_location(&hash)?, Some(owner));
    drop(pool);

    let member = LmdbBlobStore::with_exact_map_size_and_external_blob_options(
        &member_path,
        16 * 1024 * 1024,
        None,
    )?;
    assert!(member.delete_sync(&hash)?);
    assert!(member.put_sync(hash, b"corrupt bytes")?);
    drop(member);

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    assert!(reopened.get_sync(&hash).is_err());
    Ok(())
}

#[test]
fn unavailable_member_recovers_only_when_its_identity_returns() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let member_path = temp.path().join("member");
    let displaced = temp.path().join("member-displaced");
    let data = b"member replacement bytes".repeat(32);
    let hash = sha256(&data);

    let pool = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    let member = pool.add_member(PoolMemberConfig::new(member_path.clone(), 1024 * 1024))?;
    assert!(pool.put_sync(hash, &data)?);
    drop(pool);

    std::fs::rename(&member_path, &displaced).expect("displace member");
    std::fs::create_dir(&member_path).expect("leave empty mountpoint");
    let unavailable = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    assert!(!unavailable.member(member)?.available);
    assert!(unavailable.get_sync(&hash).is_err());

    std::fs::remove_dir(&member_path).expect("remove empty mountpoint");
    std::fs::rename(&displaced, &member_path).expect("restore member");
    assert_eq!(unavailable.get_sync(&hash)?, Some(data));
    assert!(unavailable.member(member)?.available);
    Ok(())
}

#[test]
fn explicit_capacity_moves_new_writes_to_available_member() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())?;
    let first_data = b"first capacity blob".repeat(32);
    let second_data = b"second capacity blob".repeat(32);
    let first = pool.add_member(PoolMemberConfig::new(
        temp.path().join("first"),
        first_data.len() as u64,
    ))?;
    assert!(pool.put_sync(sha256(&first_data), &first_data)?);
    let second = pool.add_member(PoolMemberConfig::new(
        temp.path().join("second"),
        second_data.len() as u64 * 2,
    ))?;
    let second_hash = sha256(&second_data);
    assert!(pool.put_sync(second_hash, &second_data)?);
    assert_eq!(pool.blob_location(&second_hash)?, Some(second));
    assert_ne!(first, second);
    Ok(())
}

#[test]
fn valid_put_repairs_a_corrupt_located_copy() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let member_path = temp.path().join("member");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    let owner = pool.add_member(member(&member_path, 1024 * 1024))?;
    let data = b"repairable pool bytes".repeat(32);
    let hash = sha256(&data);
    assert!(pool.put_sync(hash, &data)?);
    drop(pool);

    let raw = LmdbBlobStore::with_exact_map_size_and_external_blob_options(
        &member_path,
        16 * 1024 * 1024,
        None,
    )?;
    assert!(raw.delete_sync(&hash)?);
    assert!(raw.put_sync(hash, b"corrupt bytes")?);
    drop(raw);

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    assert!(reopened.put_sync(hash, &data)?);
    assert_eq!(reopened.blob_location(&hash)?, Some(owner));
    assert_eq!(reopened.get_sync(&hash)?, Some(data));
    Ok(())
}

#[test]
fn batch_put_is_hash_verified_globally_idempotent_and_exact() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())?;
    pool.add_member(member(temp.path().join("member"), 1024 * 1024))?;
    let first = b"batch first".repeat(32);
    let second = b"batch second".repeat(32);
    let first_hash = sha256(&first);
    let second_hash = sha256(&second);
    let items = vec![
        (first_hash, first.clone()),
        (second_hash, second.clone()),
        (first_hash, first.clone()),
    ];

    let report = pool.put_many_report_sync(&items)?;
    assert_eq!(report.total, 3);
    assert_eq!(report.inserted, 2);
    assert_eq!(report.inserted_bytes, (first.len() + second.len()) as u64);
    assert_eq!(report.inserted_hashes, vec![first_hash, second_hash]);
    assert_eq!(pool.put_many_sync(&items)?, 0);
    assert_eq!(pool.stats()?.count, 2);
    assert_eq!(pool.get_sync(&first_hash)?, Some(first));
    assert_eq!(pool.get_sync(&second_hash)?, Some(second));

    let invalid = vec![(sha256(b"not these bytes"), b"bad batch bytes".to_vec())];
    assert!(pool.put_many_sync(&invalid).is_err());
    assert_eq!(pool.stats()?.count, 2);
    Ok(())
}

#[test]
fn adding_capacity_rebalances_existing_blobs_with_bounded_progress() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())?;
    let source = pool.add_member(member(temp.path().join("source"), 1024 * 1024))?;
    let blobs = (0..20)
        .map(|index| {
            let data = format!("rebalance blob {index:04}").repeat(32).into_bytes();
            (sha256(&data), data)
        })
        .collect::<Vec<_>>();
    assert_eq!(pool.put_many_sync(&blobs)?, blobs.len());
    assert_eq!(pool.member(source)?.located_blobs, blobs.len() as u64);

    let target = pool.add_member(member(temp.path().join("target"), 1024 * 1024))?;
    let mut moved = 0usize;
    loop {
        let report = pool.maintain(3)?;
        assert!(
            report.failed.is_empty(),
            "maintenance errors: {:?}",
            report.failed
        );
        assert!(report.moved <= 3, "maintenance must honor its item bound");
        moved += report.moved;
        if report.moved == 0 {
            break;
        }
    }
    assert_eq!(moved, blobs.len() / 2);
    assert_eq!(pool.member(source)?.located_blobs, blobs.len() as u64 / 2);
    assert_eq!(pool.member(target)?.located_blobs, blobs.len() as u64 / 2);
    for (hash, data) in blobs {
        assert_eq!(pool.get_sync(&hash)?, Some(data));
    }
    Ok(())
}

#[test]
fn mutable_pin_metadata_survives_blob_relocation_and_reopen() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let catalog = temp.path().join("catalog");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    let source = pool.add_member(member(temp.path().join("source"), 1024 * 1024))?;
    let data = b"pinned immutable blob".repeat(32);
    let hash = sha256(&data);
    assert!(pool.put_sync(hash, &data)?);
    pool.pin_sync(&hash)?;
    pool.pin_sync(&hash)?;
    assert!(pool.touch_accessed_sync(&hash, 42)?);
    assert_eq!(pool.pin_count_sync(&hash)?, 2);
    assert_eq!(pool.stats()?.pinned_count, 1);
    assert_eq!(pool.stats()?.pinned_bytes, data.len() as u64);

    let target = pool.add_member(member(temp.path().join("target"), 1024 * 1024))?;
    pool.begin_drain(source)?;
    let report = pool.maintain(1)?;
    assert_eq!(report.moved, 1);
    assert_eq!(pool.blob_location(&hash)?, Some(target));
    drop(pool);

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default())?;
    assert_eq!(reopened.pin_count_sync(&hash)?, 2);
    assert_eq!(reopened.last_accessed_at_sync(&hash)?, Some(42));
    assert_eq!(reopened.get_sync(&hash)?, Some(data));
    reopened.unpin_sync(&hash)?;
    reopened.unpin_sync(&hash)?;
    assert_eq!(reopened.pin_count_sync(&hash)?, 0);
    Ok(())
}

#[test]
fn pending_write_recovers_on_another_member_after_member_map_failure() -> Result<(), StoreError> {
    let temp = TempDir::new().expect("temp dir");
    let pool = PoolStore::open(temp.path().join("catalog"), PoolStoreConfig::default())?;
    let first = pool.add_member(
        member(temp.path().join("small-map"), 64 * 1024 * 1024)
            .with_map_size_bytes(16 * 1024 * 1024),
    )?;
    let data = vec![0x5a; 20 * 1024 * 1024];
    let hash = sha256(&data);
    assert!(pool.put_sync(hash, &data).is_err());
    assert_eq!(pool.blob_location(&hash)?, Some(first));

    let second = pool.add_member(
        member(temp.path().join("large-map"), 64 * 1024 * 1024)
            .with_map_size_bytes(64 * 1024 * 1024),
    )?;
    assert!(pool.put_sync(hash, &data)?);
    assert_eq!(pool.blob_location(&hash)?, Some(second));
    assert_eq!(pool.get_sync(&hash)?, Some(data));
    Ok(())
}
