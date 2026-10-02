#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use hashtree_core::{sha256, StoreError};
use hashtree_lmdb::{
    ExternalBlobOptions, LmdbBlobStore, PhysicalSpaceGuard, PoolMemberConfig, PoolStore,
    PoolStoreConfig, MAX_GUARDED_WRITE_BYTES,
};

const MAP_SIZE: usize = 16 * 1024 * 1024;

fn assert_errno<T: std::fmt::Debug>(result: Result<T, StoreError>, expected: i32) {
    match result {
        Err(StoreError::Io(error)) => assert_eq!(error.raw_os_error(), Some(expected)),
        other => panic!("expected errno {expected}, got {other:?}"),
    }
}

// Compare durable data and external blobs, excluding LMDB's mutable reader table.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, output);
            } else if entry.file_name() != "lock.mdb" {
                output.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    visit(root, root, &mut output);
    output
}

fn raw_refusal_retains_history(external_kind: Option<bool>) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("raw");
    let external = external_kind.map(|packed| ExternalBlobOptions {
        base_path: temp.path().join("external"),
        min_bytes: 64,
        sync: true,
        pack_target_bytes: packed.then_some(1024 * 1024),
    });
    let old = b"historical blob".repeat(4096);
    let old_hash = sha256(&old);
    let store = LmdbBlobStore::with_exact_map_size_and_external_blob_options(
        &path,
        MAP_SIZE,
        external.clone(),
    )
    .unwrap();
    assert_eq!(store.put_many_sync(&[(old_hash, old.clone())]).unwrap(), 1);
    drop(store);
    let before = files(temp.path());
    let guard = PhysicalSpaceGuard::new(u64::MAX).unwrap();
    let store = LmdbBlobStore::with_physical_space_guard(
        &path,
        MAP_SIZE,
        external.clone(),
        None,
        guard.clone(),
    )
    .unwrap();
    assert_eq!(store.get_sync(&old_hash).unwrap(), Some(old.clone()));
    assert!(!store.put_sync(old_hash, &old).unwrap());
    assert_eq!(store.put_many_sync(&[(old_hash, old.clone())]).unwrap(), 0);
    assert!(
        !guard.has_refused(),
        "duplicate/no-op writes require no allocation"
    );
    assert_eq!(files(temp.path()), before);
    let new = b"new blob refused".repeat(4096);
    let new_hash = sha256(&new);
    assert_errno(store.put_many_sync(&[(new_hash, new)]), libc::ENOSPC);
    assert!(guard.has_refused());
    assert_eq!(store.get_sync(&old_hash).unwrap(), Some(old.clone()));
    assert_eq!(store.get_sync(&new_hash).unwrap(), None);
    assert_eq!(store.stats().unwrap().count, 1);
    assert_eq!(files(temp.path()), before);
    drop(store);
    let reopened =
        LmdbBlobStore::with_exact_map_size_and_external_blob_options(&path, MAP_SIZE, external)
            .unwrap();
    assert_eq!(reopened.get_sync(&old_hash).unwrap(), Some(old));
    assert_eq!(reopened.get_sync(&new_hash).unwrap(), None);
}

#[test]
fn raw_capacity_refusal_preserves_history_and_allows_duplicates() {
    raw_refusal_retains_history(None);
}

#[test]
fn loose_external_capacity_refusal_leaves_history_and_no_new_files() {
    raw_refusal_retains_history(Some(false));
}

#[test]
fn packed_external_capacity_refusal_leaves_history_and_no_new_files() {
    raw_refusal_retains_history(Some(true));
}

fn pool_config(guard: Option<PhysicalSpaceGuard>) -> PoolStoreConfig {
    let mut config = PoolStoreConfig {
        physical_space: guard,
        catalog_map_size_bytes: MAP_SIZE as u64,
        ..PoolStoreConfig::default()
    };
    config.temperature.enabled = false;
    config
}

#[test]
fn pool_refusal_preserves_catalog_members_and_duplicate_reads() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("catalog");
    let pool = PoolStore::open(&path, pool_config(None)).unwrap();
    let first = pool
        .add_member(PoolMemberConfig::new(
            temp.path().join("first"),
            1024 * 1024,
        ))
        .unwrap();
    let old = b"pool history".repeat(64);
    let old_hash = sha256(&old);
    assert!(pool.put_sync(old_hash, &old).unwrap());
    assert_eq!(pool.blob_location(&old_hash).unwrap(), Some(first));
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("second"),
        1024 * 1024,
    ))
    .unwrap();
    drop(pool);
    let before = files(temp.path());
    let guard = PhysicalSpaceGuard::new(u64::MAX).unwrap();
    let mut requested = pool_config(Some(guard.clone()));
    requested.temperature.enabled = true;
    let pool = PoolStore::open(&path, requested).unwrap();
    assert_eq!(pool.balance_temperature().unwrap(), Default::default());
    assert!(
        !guard.has_refused(),
        "guarded append must disable unrelated relocation"
    );
    assert!(!pool.put_sync(old_hash, &old).unwrap());
    assert_eq!(pool.get_sync(&old_hash).unwrap(), Some(old.clone()));
    assert!(!guard.has_refused());
    let new = b"new pool data";
    let new_hash = sha256(new);
    assert_errno(pool.put_sync(new_hash, new), libc::ENOSPC);
    assert!(guard.has_refused());
    assert_eq!(pool.blob_location(&new_hash).unwrap(), None);
    assert_eq!(pool.blob_location(&old_hash).unwrap(), Some(first));
    assert_eq!(pool.get_sync(&old_hash).unwrap(), Some(old));
    assert_eq!(
        files(temp.path()),
        before,
        "neither alternate member nor catalog may grow"
    );
}

#[test]
fn pool_member_repair_refusal_preserves_prior_location_and_other_history() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let first_path = temp.path().join("first");
    let pool = PoolStore::open(&catalog, pool_config(None)).unwrap();
    let first = pool
        .add_member(PoolMemberConfig::new(first_path.clone(), 1024 * 1024))
        .unwrap();
    let old = b"unrelated history".repeat(64);
    let old_hash = sha256(&old);
    let repair = b"member repair target".repeat(64);
    let repair_hash = sha256(&repair);
    assert!(pool.put_sync(old_hash, &old).unwrap());
    assert!(pool.put_sync(repair_hash, &repair).unwrap());
    pool.add_member(PoolMemberConfig::new(
        temp.path().join("second"),
        1024 * 1024,
    ))
    .unwrap();
    drop(pool);
    // Existing repair path writes the preferred member before touching the catalog.
    let member =
        LmdbBlobStore::with_exact_map_size_and_external_blob_options(&first_path, MAP_SIZE, None)
            .unwrap();
    assert!(member.delete_sync(&repair_hash).unwrap());
    drop(member);
    let before = files(temp.path());
    let guard = PhysicalSpaceGuard::new(u64::MAX).unwrap();
    let pool = PoolStore::open(&catalog, pool_config(Some(guard.clone()))).unwrap();
    assert_errno(pool.put_sync(repair_hash, &repair), libc::ENOSPC);
    assert!(guard.has_refused());
    assert_eq!(pool.blob_location(&repair_hash).unwrap(), Some(first));
    assert_eq!(pool.get_sync(&old_hash).unwrap(), Some(old));
    assert_eq!(files(temp.path()), before);
}

#[test]
fn oversized_write_refusal_latches_without_permitting_later_smaller_writes() {
    let temp = tempfile::tempdir().unwrap();
    let file = fs::File::create(temp.path().join("file")).unwrap();
    let guard = PhysicalSpaceGuard::new(0).unwrap();
    assert_eq!(
        guard
            .admit_file(&file, 0, MAX_GUARDED_WRITE_BYTES + 1)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EFBIG)
    );
    assert!(guard.has_refused());
    assert_eq!(
        guard.admit_file(&file, 0, 1).unwrap_err().raw_os_error(),
        Some(libc::EFBIG)
    );
    assert_eq!(file.metadata().unwrap().len(), 0);
}

#[test]
fn admitted_raw_guard_persists_then_reopens_history() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("raw");
    let guard = PhysicalSpaceGuard::new(0).unwrap();
    let store =
        LmdbBlobStore::with_physical_space_guard(&path, MAP_SIZE, None, None, guard.clone())
            .unwrap();
    let data = b"guarded bytes".repeat(4096);
    let hash = sha256(&data);
    assert!(store.put_sync(hash, &data).unwrap());
    assert!(!guard.has_refused());
    drop(store);
    let reopened =
        LmdbBlobStore::with_exact_map_size_and_external_blob_options(&path, MAP_SIZE, None)
            .unwrap();
    assert_eq!(reopened.get_sync(&hash).unwrap(), Some(data));
}
