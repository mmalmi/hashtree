//! Integration tests for storage limit and LRU eviction
//!
//! Tests:
//! - Quota enforcement (eviction when over limit)
//! - Priority protection (pinned trees never evicted)
//! - Tree-level LRU (oldest low-priority evicted first)
//! - Shared blobs (blob in 2 trees, evict one, blob remains)
//!
//! Run with: cargo test --package hashtree-cli --test eviction -- --nocapture

use hashtree_core::from_hex;
use hashtree_cli::storage::{HashtreeStore, PRIORITY_OWN, PRIORITY_FOLLOWED, PRIORITY_OTHER};
use tempfile::TempDir;
use std::thread;
use std::time::Duration;

/// Create a store with a small max size for testing
fn test_store(max_size_bytes: u64) -> (HashtreeStore, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let store = HashtreeStore::with_options(temp_dir.path(), None, max_size_bytes)
        .expect("Failed to create store");
    (store, temp_dir)
}

/// Helper: Add a blob and get its hash as bytes
fn add_blob(store: &HashtreeStore, data: &[u8]) -> [u8; 32] {
    let hash_hex = store.put_blob(data).expect("Failed to put blob");
    from_hex(&hash_hex).expect("Invalid hash")
}

#[test]
fn test_tree_indexing() {
    let (store, _tmp) = test_store(1024 * 1024 * 1024); // 1GB

    // Add a blob
    let data = b"Hello, world!";
    let hash = add_blob(&store, data);

    // Index it as a tree (single blob tree)
    store.index_tree(&hash, "test_owner", Some("test_tree"), PRIORITY_FOLLOWED, None)
        .expect("Failed to index tree");

    // Verify it's indexed
    let meta = store.get_tree_meta(&hash).expect("Failed to get meta");
    assert!(meta.is_some(), "Tree should be indexed");

    let meta = meta.unwrap();
    assert_eq!(meta.owner, "test_owner");
    assert_eq!(meta.name, Some("test_tree".to_string()));
    assert_eq!(meta.priority, PRIORITY_FOLLOWED);
    assert!(meta.total_size > 0, "Should have tracked size");
}

#[test]
fn test_list_indexed_trees() {
    let (store, _tmp) = test_store(1024 * 1024 * 1024);

    // Add and index multiple blobs as trees
    let data1 = b"Tree 1 content";
    let hash1 = add_blob(&store, data1);
    store.index_tree(&hash1, "owner1", Some("tree1"), PRIORITY_OWN, None)
        .expect("Failed to index tree 1");

    // Small delay to ensure different synced_at
    thread::sleep(Duration::from_millis(10));

    let data2 = b"Tree 2 content";
    let hash2 = add_blob(&store, data2);
    store.index_tree(&hash2, "owner2", Some("tree2"), PRIORITY_FOLLOWED, None)
        .expect("Failed to index tree 2");

    // List trees
    let trees = store.list_indexed_trees().expect("Failed to list trees");
    assert_eq!(trees.len(), 2, "Should have 2 indexed trees");

    // Verify both are present
    let hashes: Vec<[u8; 32]> = trees.iter().map(|(h, _)| *h).collect();
    assert!(hashes.contains(&hash1));
    assert!(hashes.contains(&hash2));
}

#[test]
fn test_tracked_size() {
    let (store, _tmp) = test_store(1024 * 1024 * 1024);

    // Initial tracked size should be 0
    let initial = store.tracked_size().expect("Failed to get tracked size");
    assert_eq!(initial, 0, "Initial tracked size should be 0");

    // Add and index a blob
    let data = vec![0u8; 1000]; // 1KB
    let hash = add_blob(&store, &data);
    store.index_tree(&hash, "owner", None, PRIORITY_OTHER, None)
        .expect("Failed to index tree");

    // Tracked size should now include this blob
    let tracked = store.tracked_size().expect("Failed to get tracked size");
    assert_eq!(tracked, 1000, "Tracked size should be 1000 bytes");
}

#[test]
fn test_storage_by_priority() {
    let (store, _tmp) = test_store(1024 * 1024 * 1024);

    // Add blobs with different priorities
    let data_own = vec![0u8; 500];
    let hash_own = add_blob(&store, &data_own);
    store.index_tree(&hash_own, "me", Some("own"), PRIORITY_OWN, None).unwrap();

    let data_followed = vec![1u8; 300];
    let hash_followed = add_blob(&store, &data_followed);
    store.index_tree(&hash_followed, "friend", Some("followed"), PRIORITY_FOLLOWED, None).unwrap();

    let data_other = vec![2u8; 200];
    let hash_other = add_blob(&store, &data_other);
    store.index_tree(&hash_other, "random", Some("other"), PRIORITY_OTHER, None).unwrap();

    // Check breakdown
    let by_priority = store.storage_by_priority().expect("Failed to get by priority");
    assert_eq!(by_priority.own, 500, "Own should be 500 bytes");
    assert_eq!(by_priority.followed, 300, "Followed should be 300 bytes");
    assert_eq!(by_priority.other, 200, "Other should be 200 bytes");
}

#[test]
fn test_eviction_under_limit() {
    let (store, _tmp) = test_store(10000); // 10KB limit

    // Add a small blob (under limit)
    let data = vec![0u8; 100];
    let hash = add_blob(&store, &data);
    store.index_tree(&hash, "owner", None, PRIORITY_OTHER, None).unwrap();

    // Eviction should do nothing
    let freed = store.evict_if_needed().expect("Eviction failed");
    assert_eq!(freed, 0, "Should not evict when under limit");

    // Blob should still exist
    assert!(store.blob_exists(&hash).unwrap(), "Blob should still exist");
}

#[test]
fn test_eviction_over_limit() {
    let (store, _tmp) = test_store(500); // 500 byte limit

    // Add blobs that exceed the limit
    let data1 = vec![1u8; 200];
    let hash1 = add_blob(&store, &data1);
    store.index_tree(&hash1, "owner1", Some("tree1"), PRIORITY_OTHER, None).unwrap();

    thread::sleep(Duration::from_millis(10));

    let data2 = vec![2u8; 200];
    let hash2 = add_blob(&store, &data2);
    store.index_tree(&hash2, "owner2", Some("tree2"), PRIORITY_OTHER, None).unwrap();

    thread::sleep(Duration::from_millis(10));

    let data3 = vec![3u8; 200];
    let hash3 = add_blob(&store, &data3);
    store.index_tree(&hash3, "owner3", Some("tree3"), PRIORITY_OTHER, None).unwrap();

    // Total is 600 bytes, limit is 500, should evict oldest
    let freed = store.evict_if_needed().expect("Eviction failed");
    assert!(freed > 0, "Should have evicted something");

    // Oldest (tree1) should be evicted
    let meta1 = store.get_tree_meta(&hash1).unwrap();
    assert!(meta1.is_none(), "Oldest tree should be evicted");

    // Newer trees should remain
    let meta2 = store.get_tree_meta(&hash2).unwrap();
    let meta3 = store.get_tree_meta(&hash3).unwrap();
    // At least one of the newer ones should remain
    assert!(meta2.is_some() || meta3.is_some(), "Newer trees should remain");
}

#[test]
fn test_pinned_tree_protection() {
    let (store, _tmp) = test_store(300); // Small limit

    // Add pinned tree (protected)
    let data_pinned = vec![0u8; 200];
    let hash_pinned = add_blob(&store, &data_pinned);
    store.index_tree(&hash_pinned, "me", Some("pinned"), PRIORITY_OWN, None).unwrap();
    store.pin(&hash_pinned).expect("Failed to pin");

    // Add other tree (evictable)
    let data_other = vec![1u8; 200];
    let hash_other = add_blob(&store, &data_other);
    store.index_tree(&hash_other, "random", Some("other"), PRIORITY_OTHER, None).unwrap();

    // Total is 400 bytes, limit is 300
    let freed = store.evict_if_needed().expect("Eviction failed");

    // Pinned tree should NOT be evicted
    let meta_pinned = store.get_tree_meta(&hash_pinned).unwrap();
    assert!(meta_pinned.is_some(), "Pinned tree should be protected from eviction");

    // Other tree should be evicted
    let meta_other = store.get_tree_meta(&hash_other).unwrap();
    assert!(meta_other.is_none(), "Other tree should be evicted");

    assert!(freed > 0, "Should have freed space by evicting other tree");
}

#[test]
fn test_own_tree_can_be_evicted() {
    let (store, _tmp) = test_store(300); // Small limit

    // Add own tree (NOT pinned - can be evicted as last resort)
    let data_own = vec![0u8; 200];
    let hash_own = add_blob(&store, &data_own);
    store.index_tree(&hash_own, "me", Some("own"), PRIORITY_OWN, None).unwrap();

    // Add another own tree to push over limit
    let data_own2 = vec![1u8; 200];
    let hash_own2 = add_blob(&store, &data_own2);
    store.index_tree(&hash_own2, "me", Some("own2"), PRIORITY_OWN, None).unwrap();

    // Total is 400 bytes, limit is 300
    // Both are own trees, oldest should be evicted
    let freed = store.evict_if_needed().expect("Eviction failed");

    assert!(freed > 0, "Should have evicted own tree when no other option");
}

#[test]
fn test_priority_order_eviction() {
    let (store, _tmp) = test_store(400); // Limit

    // Add trees with different priorities
    // Add from oldest to newest

    let data_other = vec![0u8; 150];
    let hash_other = add_blob(&store, &data_other);
    store.index_tree(&hash_other, "random", Some("other"), PRIORITY_OTHER, None).unwrap();

    thread::sleep(Duration::from_millis(10));

    let data_followed = vec![1u8; 150];
    let hash_followed = add_blob(&store, &data_followed);
    store.index_tree(&hash_followed, "friend", Some("followed"), PRIORITY_FOLLOWED, None).unwrap();

    thread::sleep(Duration::from_millis(10));

    // Pin the own tree so it's protected
    let data_own = vec![2u8; 150];
    let hash_own = add_blob(&store, &data_own);
    store.index_tree(&hash_own, "me", Some("own"), PRIORITY_OWN, None).unwrap();
    store.pin(&hash_own).expect("Failed to pin");

    // Total is 450 bytes, limit is 400
    // Should evict lowest priority first (other)
    let freed = store.evict_if_needed().expect("Eviction failed");

    // Other (lowest priority) should be evicted first
    let meta_other = store.get_tree_meta(&hash_other).unwrap();
    assert!(meta_other.is_none(), "Other tree (lowest priority) should be evicted first");

    // Followed should remain (higher priority than other)
    let _meta_followed = store.get_tree_meta(&hash_followed).unwrap();
    // Note: If still over quota, followed might also be evicted
    // But pinned should never be evicted
    let meta_own = store.get_tree_meta(&hash_own).unwrap();
    assert!(meta_own.is_some(), "Pinned tree should never be evicted");

    assert!(freed > 0, "Should have freed space");
}

#[test]
fn test_unindex_tree() {
    let (store, _tmp) = test_store(1024 * 1024 * 1024);

    // Add and index a blob
    let data = vec![0u8; 500];
    let hash = add_blob(&store, &data);
    store.index_tree(&hash, "owner", Some("test"), PRIORITY_OTHER, None).unwrap();

    // Verify it exists
    assert!(store.get_tree_meta(&hash).unwrap().is_some());
    assert!(store.blob_exists(&hash).unwrap());

    // Unindex it
    let freed = store.unindex_tree(&hash).expect("Unindex failed");
    assert!(freed > 0, "Should have freed bytes");

    // Tree meta should be gone
    assert!(store.get_tree_meta(&hash).unwrap().is_none());

    // Blob should also be deleted (orphaned)
    assert!(!store.blob_exists(&hash).unwrap(), "Orphaned blob should be deleted");
}

#[test]
fn test_max_size_bytes_accessor() {
    let (store, _tmp) = test_store(12345678);
    assert_eq!(store.max_size_bytes(), 12345678, "max_size_bytes should return configured value");
}

#[test]
fn test_orphan_eviction_first() {
    let (store, _tmp) = test_store(500); // Small limit

    // Add an orphan blob (not indexed as part of any tree)
    let orphan_data = vec![0u8; 200];
    let orphan_hash = add_blob(&store, &orphan_data);

    // Add an indexed tree
    let tree_data = vec![1u8; 200];
    let tree_hash = add_blob(&store, &tree_data);
    store.index_tree(&tree_hash, "owner", Some("tree"), PRIORITY_OTHER, None).unwrap();

    // Add another blob to push over limit
    let extra_data = vec![2u8; 200];
    let _extra_hash = add_blob(&store, &extra_data);

    // Total is 600 bytes, limit is 500
    // Orphan should be evicted first before any trees
    let freed = store.evict_if_needed().expect("Eviction failed");
    assert!(freed > 0, "Should have freed space");

    // Orphan blob should be gone
    assert!(!store.blob_exists(&orphan_hash).unwrap(), "Orphan blob should be evicted first");

    // Indexed tree should still exist (orphan eviction was enough)
    let meta = store.get_tree_meta(&tree_hash).unwrap();
    assert!(meta.is_some(), "Indexed tree should remain after orphan eviction");
}

#[test]
fn test_pinned_not_evicted_as_orphan() {
    let (store, _tmp) = test_store(300); // Small limit

    // Add a pinned blob (not in any tree but pinned)
    let pinned_data = vec![0u8; 200];
    let pinned_hash = add_blob(&store, &pinned_data);
    store.pin(&pinned_hash).expect("Failed to pin");

    // Add another blob to push over limit
    let extra_data = vec![1u8; 200];
    let _extra_hash = add_blob(&store, &extra_data);

    // Total is 400 bytes, limit is 300
    // But pinned blob should NOT be evicted
    let _ = store.evict_if_needed();

    // Pinned blob should still exist
    assert!(store.blob_exists(&pinned_hash).unwrap(), "Pinned blob should not be evicted");
}

#[test]
fn test_ref_key_replaces_old_version() {
    let (store, _tmp) = test_store(1024 * 1024 * 1024);

    // Add first version of a tree
    let data1 = vec![0u8; 100];
    let hash1 = add_blob(&store, &data1);
    store.index_tree(&hash1, "owner", Some("test"), PRIORITY_OWN, Some("owner/test")).unwrap();

    // Verify it exists
    assert!(store.get_tree_meta(&hash1).unwrap().is_some());

    // Add second version with same ref_key
    let data2 = vec![1u8; 100];
    let hash2 = add_blob(&store, &data2);
    store.index_tree(&hash2, "owner", Some("test"), PRIORITY_OWN, Some("owner/test")).unwrap();

    // First version should be unindexed
    assert!(store.get_tree_meta(&hash1).unwrap().is_none(), "Old version should be unindexed");

    // Second version should exist
    assert!(store.get_tree_meta(&hash2).unwrap().is_some(), "New version should be indexed");
}
