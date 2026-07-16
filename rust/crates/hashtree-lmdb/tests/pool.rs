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
