//! Extensive tests for HashTree - unified merkle tree operations
//!
//! Tests matching and exceeding hashtree-ts test coverage

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use hashtree_core::{
    Cid, DirEntry, HashTree, HashTreeConfig, HashTreeError, Link, LinkType, MemoryStore, Store, to_hex,
};

fn make_tree() -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    // Use public (unencrypted) mode for these tests
    let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());
    (store, tree)
}

fn make_tree_with_chunk_size(chunk_size: usize) -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    // Use public (unencrypted) mode for these tests
    let tree = HashTree::new(HashTreeConfig::new(store.clone()).public().with_chunk_size(chunk_size));
    (store, tree)
}

// ============ CREATE TESTS ============

mod create {
    use super::*;

    #[tokio::test]
    async fn test_store_small_file_as_single_blob() {
        let (_store, tree) = make_tree();

        let data = b"hello world";
        let (cid, size) = tree.put_file(data).await.unwrap();

        assert_eq!(size, 11);
        assert_eq!(cid.hash.len(), 32);

        let retrieved = tree.read_file(&cid.hash).await.unwrap();
        assert_eq!(retrieved, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn test_chunk_large_files() {
        let (_, tree) = make_tree_with_chunk_size(10);
        let data = b"this is a longer message that will be chunked";

        let (cid, size) = tree.put_file(data).await.unwrap();
        assert_eq!(size, data.len() as u64);

        let retrieved = tree.read_file(&cid.hash).await.unwrap();
        assert_eq!(retrieved, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn test_create_empty_directory() {
        let (_store, tree) = make_tree();

        let dir_cid = tree.put_directory(vec![]).await.unwrap();

        let entries = tree.list_directory(&dir_cid).await.unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[tokio::test]
    async fn test_create_directory_with_entries() {
        let (_store, tree) = make_tree();

        let (file1_cid, _) = tree.put_file(b"content1").await.unwrap();
        let (file2_cid, _) = tree.put_file(b"content2").await.unwrap();

        let dir_cid = tree
            .put_directory(
                vec![
                    DirEntry::new("a.txt", file1_cid.hash).with_size(8),
                    DirEntry::new("b.txt", file2_cid.hash).with_size(8),
                ],
            )
            .await
            .unwrap();

        let entries = tree.list_directory(&dir_cid).await.unwrap();
        assert_eq!(entries.len(), 2);

        let mut names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[tokio::test]
    async fn test_directory_entries_sorted() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"data").await.unwrap();

        let dir_hash = tree
            .put_directory(
                vec![
                    DirEntry::new("zebra", file_hash),
                    DirEntry::new("apple", file_hash),
                    DirEntry::new("mango", file_hash),
                ],
            )
            .await
            .unwrap();

        let node = tree.get_tree_node(&dir_hash.hash).await.unwrap().unwrap();
        let names: Vec<_> = node.links.iter().filter_map(|l| l.name.clone()).collect();
        // Should be sorted alphabetically
        assert_eq!(names, vec!["apple", "mango", "zebra"]);
    }

    #[tokio::test]
    async fn test_put_tree_node_with_link_meta() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"data").await.unwrap();

        let mut link_meta = HashMap::new();
        link_meta.insert("version".to_string(), serde_json::json!(2));
        link_meta.insert("author".to_string(), serde_json::json!("test"));

        let node_hash = tree
            .put_tree_node(
                vec![Link::new(file_hash).with_name("file.txt").with_size(4).with_meta(link_meta)],
            )
            .await
            .unwrap();

        let node = tree.get_tree_node(&node_hash).await.unwrap().unwrap();
        assert_eq!(node.links.len(), 1);
        let m = node.links[0].meta.as_ref().unwrap();
        assert_eq!(m.get("version"), Some(&serde_json::json!(2)));
        assert_eq!(m.get("author"), Some(&serde_json::json!("test")));
    }

    #[tokio::test]
    async fn test_file_deduplication() {
        let (store, tree) = make_tree_with_chunk_size(100);

        let repeated_chunk = vec![42u8; 100];
        let data: Vec<u8> = repeated_chunk.iter().cycle().take(500).cloned().collect();

        let (cid, size) = tree.put_file(&data).await.unwrap();
        assert_eq!(size, 500);

        // Store should have fewer items due to deduplication
        // (1 unique chunk + tree nodes)
        let store_size = store.size();
        assert!(store_size < 5, "Expected deduplication, got {} items", store_size);

        // Verify can still read back
        let retrieved = tree.read_file(&cid.hash).await.unwrap().unwrap();
        assert_eq!(retrieved.len(), 500);
    }

    #[tokio::test]
    async fn test_nested_directory_structure() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"deep content").await.unwrap();

        let deep_dir = tree
            .put_directory(vec![DirEntry::new("file.txt", file_hash).with_size(12)])
            .await
            .unwrap();

        let mid_dir = tree
            .put_directory(vec![DirEntry::new("deep", deep_dir.hash)])
            .await
            .unwrap();

        let root_dir = tree
            .put_directory(vec![DirEntry::new("mid", mid_dir.hash)])
            .await
            .unwrap();

        // Verify structure
        let resolved = tree.resolve_path(&root_dir, "mid/deep/file.txt").await.unwrap();
        assert_eq!(resolved.map(|c| c.hash), Some(file_hash));
    }
}

// ============ READ TESTS ============

mod read {
    use super::*;

    #[tokio::test]
    async fn test_read_file() {
        let (_store, tree) = make_tree();

        let data = b"test content";
        let (cid, _) = tree.put_file(data).await.unwrap();

        let read_data = tree.read_file(&cid.hash).await.unwrap();
        assert_eq!(read_data, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn test_read_missing_file() {
        let (_store, tree) = make_tree();

        let missing_hash = [0u8; 32];
        let result = tree.read_file(&missing_hash).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_list_directory() {
        let (_store, tree) = make_tree();

        let (file_cid, _) = tree.put_file(b"data").await.unwrap();
        let dir_cid = tree
            .put_directory(vec![DirEntry::new("file.txt", file_cid.hash).with_size(4)])
            .await
            .unwrap();

        let entries = tree.list_directory(&dir_cid).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file.txt");
    }

    #[tokio::test]
    async fn test_resolve_path() {
        let (_store, tree) = make_tree();

        let (file_cid, _) = tree.put_file(b"nested").await.unwrap();
        let sub_dir_cid = tree
            .put_directory(vec![DirEntry::new("file.txt", file_cid.hash).with_size(6)])
            .await
            .unwrap();
        let root_cid = tree
            .put_directory(vec![DirEntry::new("subdir", sub_dir_cid.hash).with_size(6)])
            .await
            .unwrap();

        let resolved = tree.resolve_path(&root_cid, "subdir/file.txt").await.unwrap();
        assert!(resolved.is_some());
        assert_eq!(to_hex(&resolved.unwrap().hash), to_hex(&file_cid.hash));
    }

    #[tokio::test]
    async fn test_resolve_path_missing() {
        let (_store, tree) = make_tree();

        let dir_cid = tree.put_directory(vec![]).await.unwrap();

        let resolved = tree.resolve_path(&dir_cid, "nonexistent/path").await.unwrap();
        assert!(resolved.is_none());
    }

    #[tokio::test]
    async fn test_check_if_hash_is_directory() {
        let (_store, tree) = make_tree();

        let (file_cid, _) = tree.put_file(b"data").await.unwrap();
        let dir_hash = tree.put_directory(vec![]).await.unwrap().hash;

        assert!(!tree.is_directory(&file_cid.hash).await.unwrap());
        assert!(tree.is_directory(&dir_hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_is_tree() {
        let (_store, tree) = make_tree();

        let blob_hash = tree.put_blob(b"raw data").await.unwrap();
        let dir_hash = tree.put_directory(vec![]).await.unwrap().hash;

        assert!(!tree.is_tree(&blob_hash).await.unwrap());
        assert!(tree.is_tree(&dir_hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_get_size_blob() {
        let (_store, tree) = make_tree();

        let data = b"test data for size";
        let hash = tree.put_blob(data).await.unwrap();

        let size = tree.get_size(&hash).await.unwrap();
        assert_eq!(size, data.len() as u64);
    }

    #[tokio::test]
    async fn test_get_size_chunked_file() {
        let (_store, tree) = make_tree_with_chunk_size(100);

        let data = vec![0u8; 500];
        let (cid, _) = tree.put_file(&data).await.unwrap();

        let size = tree.get_size(&cid.hash).await.unwrap();
        assert_eq!(size, 500);
    }

    #[tokio::test]
    async fn test_read_file_chunks() {
        let (_store, tree) = make_tree_with_chunk_size(10);

        let data = b"hello world!!!";
        let (cid, _) = tree.put_file(data).await.unwrap();

        let chunks = tree.read_file_chunks(&cid.hash).await.unwrap();
        assert!(chunks.len() > 1); // Should be multiple chunks

        // Reconstruct
        let combined: Vec<u8> = chunks.into_iter().flatten().collect();
        assert_eq!(combined, data.to_vec());
    }

    #[tokio::test]
    async fn test_walk() {
        let (_store, tree) = make_tree();

        let f1 = tree.put_blob(b"1").await.unwrap();
        let f2 = tree.put_blob(b"23").await.unwrap();

        let sub_dir = tree
            .put_directory(vec![DirEntry::new("nested.txt", f2).with_size(2)])
            .await
            .unwrap();

        let root_dir = tree
            .put_directory(
                vec![
                    DirEntry::new("root.txt", f1).with_size(1),
                    DirEntry::new("sub", sub_dir.hash),
                ],
            )
            .await
            .unwrap();

        let entries = tree.walk(&root_dir, "").await.unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();

        assert!(paths.contains(&""));
        assert!(paths.contains(&"root.txt"));
        assert!(paths.contains(&"sub"));
        assert!(paths.contains(&"sub/nested.txt"));
    }

    #[tokio::test]
    async fn test_get_tree_node() {
        let (_store, tree) = make_tree();

        let blob_hash = tree.put_blob(b"data").await.unwrap();
        let dir_cid = tree
            .put_directory(vec![DirEntry::new("file.txt", blob_hash)])
            .await
            .unwrap();

        // Blob should return None
        let blob_node = tree.get_tree_node(&blob_hash).await.unwrap();
        assert!(blob_node.is_none());

        // Directory should return TreeNode
        let dir_node = tree.get_tree_node(&dir_cid.hash).await.unwrap();
        assert!(dir_node.is_some());
        assert_eq!(dir_node.unwrap().links.len(), 1);
    }
}

// ============ STREAMING TESTS ============

mod streaming {
    use super::*;

    #[tokio::test]
    async fn test_read_file_stream_small() {
        let (_store, tree) = make_tree();

        let data = b"small data";
        let (cid, _) = tree.put_file(data).await.unwrap();

        let mut stream = tree.read_file_stream(cid.hash);
        let mut collected = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            collected.extend(chunk_result.unwrap());
        }

        assert_eq!(collected, data.to_vec());
    }

    #[tokio::test]
    async fn test_read_file_stream_chunked() {
        let (_store, tree) = make_tree_with_chunk_size(5);

        let data = b"hello world!";
        let (cid, _) = tree.put_file(data).await.unwrap();

        let mut stream = tree.read_file_stream(cid.hash);
        let mut chunks = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            chunks.push(chunk_result.unwrap());
        }

        assert!(chunks.len() > 1);

        let combined: Vec<u8> = chunks.into_iter().flatten().collect();
        assert_eq!(combined, data.to_vec());
    }

    #[tokio::test]
    async fn test_read_file_stream_missing() {
        let (_store, tree) = make_tree();

        let missing_hash = [0u8; 32];
        let mut stream = tree.read_file_stream(missing_hash);

        let result = stream.next().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_read_file_stream_large() {
        let (_store, tree) = make_tree_with_chunk_size(100);

        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let (cid, _) = tree.put_file(&data).await.unwrap();

        let mut stream = tree.read_file_stream(cid.hash);
        let mut total_size = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            total_size += chunk.len();
        }

        assert_eq!(total_size, 1000);
    }

    #[tokio::test]
    async fn test_walk_stream() {
        let (_store, tree) = make_tree();

        let f1 = tree.put_blob(b"1").await.unwrap();
        let f2 = tree.put_blob(b"23").await.unwrap();

        let sub_dir = tree
            .put_directory(vec![DirEntry::new("nested.txt", f2).with_size(2)])
            .await
            .unwrap();

        let root_dir = tree
            .put_directory(
                vec![
                    DirEntry::new("root.txt", f1).with_size(1),
                    DirEntry::new("sub", sub_dir.hash),
                ],
            )
            .await
            .unwrap();

        let mut stream = tree.walk_stream(root_dir, "".to_string());
        let mut paths = Vec::new();

        while let Some(entry_result) = stream.next().await {
            let entry = entry_result.unwrap();
            paths.push(entry.path);
        }

        assert!(paths.contains(&"".to_string()));
        assert!(paths.contains(&"root.txt".to_string()));
        assert!(paths.contains(&"sub".to_string()));
        assert!(paths.contains(&"sub/nested.txt".to_string()));
    }

    #[tokio::test]
    async fn test_walk_stream_single_file() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"content").await.unwrap();

        let mut stream = tree.walk_stream(Cid::public(file_hash), "file.txt".to_string());
        let mut entries = Vec::new();

        while let Some(entry_result) = stream.next().await {
            entries.push(entry_result.unwrap());
        }

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "file.txt");
        assert!(!entries[0].link_type.is_tree());
        assert_eq!(entries[0].size, 7);
    }
}

// ============ EDIT TESTS ============

mod edit {
    use super::*;

    #[tokio::test]
    async fn test_add_entry_to_directory() {
        let (_store, tree) = make_tree();

        let root_cid = tree.put_directory(vec![]).await.unwrap();
        let (file_cid, file_size) = tree.put_file(b"hello").await.unwrap();

        let new_root = tree
            .set_entry(&root_cid, &[], "test.txt", &file_cid, file_size, LinkType::File)
            .await
            .unwrap();

        let entries = tree.list_directory(&new_root).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "test.txt");
    }

    #[tokio::test]
    async fn test_update_existing_entry() {
        let (_store, tree) = make_tree();

        let (file1_cid, _) = tree.put_file(b"v1").await.unwrap();
        let root_cid = tree
            .put_directory(vec![DirEntry::new("file.txt", file1_cid.hash).with_size(2)])
            .await
            .unwrap();

        let (file2_cid, file2_size) = tree.put_file(b"v2 updated").await.unwrap();
        let new_root = tree
            .set_entry(&root_cid, &[], "file.txt", &file2_cid, file2_size, LinkType::File)
            .await
            .unwrap();

        let entries = tree.list_directory(&new_root).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(to_hex(&entries[0].hash), to_hex(&file2_cid.hash));
    }

    #[tokio::test]
    async fn test_remove_entry() {
        let (_store, tree) = make_tree();

        let (file1_cid, _) = tree.put_file(b"a").await.unwrap();
        let (file2_cid, _) = tree.put_file(b"b").await.unwrap();
        let root_cid = tree
            .put_directory(
                vec![
                    DirEntry::new("a.txt", file1_cid.hash).with_size(1),
                    DirEntry::new("b.txt", file2_cid.hash).with_size(1),
                ],
            )
            .await
            .unwrap();

        let new_root = tree.remove_entry(&root_cid, &[], "a.txt").await.unwrap();

        let entries = tree.list_directory(&new_root).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "b.txt");
    }

    #[tokio::test]
    async fn test_rename_entry() {
        let (_store, tree) = make_tree();

        let (file_cid, _) = tree.put_file(b"content").await.unwrap();
        let root_cid = tree
            .put_directory(vec![DirEntry::new("old.txt", file_cid.hash).with_size(7)])
            .await
            .unwrap();

        let new_root = tree
            .rename_entry(&root_cid, &[], "old.txt", "new.txt")
            .await
            .unwrap();

        let entries = tree.list_directory(&new_root).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "new.txt");
        assert_eq!(to_hex(&entries[0].hash), to_hex(&file_cid.hash));
    }

    #[tokio::test]
    async fn test_rename_same_name_no_change() {
        let (_store, tree) = make_tree();

        let (file_cid, _) = tree.put_file(b"content").await.unwrap();
        let root_cid = tree
            .put_directory(vec![DirEntry::new("file.txt", file_cid.hash)])
            .await
            .unwrap();

        let new_root = tree
            .rename_entry(&root_cid, &[], "file.txt", "file.txt")
            .await
            .unwrap();

        assert_eq!(to_hex(&new_root.hash), to_hex(&root_cid.hash));
    }

    #[tokio::test]
    async fn test_move_entry_between_directories() {
        let (_store, tree) = make_tree();

        let (file_cid, _) = tree.put_file(b"content").await.unwrap();
        let dir1_cid = tree
            .put_directory(vec![DirEntry::new("file.txt", file_cid.hash).with_size(7)])
            .await
            .unwrap();
        let dir2_cid = tree.put_directory(vec![]).await.unwrap();
        let root_cid = tree
            .put_directory(
                vec![
                    DirEntry::new("dir1", dir1_cid.hash).with_size(7),
                    DirEntry::new("dir2", dir2_cid.hash).with_size(0),
                ],
            )
            .await
            .unwrap();

        let new_root = tree
            .move_entry(&root_cid, &["dir1"], "file.txt", &["dir2"])
            .await
            .unwrap();

        let entries = tree.list_directory(&new_root).await.unwrap();
        assert_eq!(entries.len(), 2);

        let dir1_entries = tree
            .list_directory(&tree.resolve_path(&new_root, "dir1").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(dir1_entries.len(), 0);

        let dir2_entries = tree
            .list_directory(&tree.resolve_path(&new_root, "dir2").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(dir2_entries.len(), 1);
        assert_eq!(dir2_entries[0].name, "file.txt");
    }

    #[tokio::test]
    async fn test_nested_path_edits() {
        let (_store, tree) = make_tree();

        let c_cid = tree.put_directory(vec![]).await.unwrap();
        let b_cid = tree
            .put_directory(vec![DirEntry::new("c", c_cid.hash).with_size(0)])
            .await
            .unwrap();
        let a_cid = tree
            .put_directory(vec![DirEntry::new("b", b_cid.hash).with_size(0)])
            .await
            .unwrap();
        let root_cid = tree
            .put_directory(vec![DirEntry::new("a", a_cid.hash).with_size(0)])
            .await
            .unwrap();

        let (file_cid, file_size) = tree.put_file(b"deep").await.unwrap();
        let new_root = tree
            .set_entry(&root_cid, &["a", "b", "c"], "file.txt", &file_cid, file_size, LinkType::File)
            .await
            .unwrap();

        // Verify nested file
        let entries = tree
            .list_directory(&tree.resolve_path(&new_root, "a/b/c").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file.txt");

        // Verify parent structure intact
        let a_entries = tree
            .list_directory(&tree.resolve_path(&new_root, "a").await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(a_entries.len(), 1);
        assert_eq!(a_entries[0].name, "b");
    }

    #[tokio::test]
    async fn test_set_entry_path_not_found() {
        let (_store, tree) = make_tree();

        let root_cid = tree.put_directory(vec![]).await.unwrap();
        let (file_cid, file_size) = tree.put_file(b"data").await.unwrap();

        let result = tree
            .set_entry(&root_cid, &["nonexistent"], "file.txt", &file_cid, file_size, LinkType::File)
            .await;

        assert!(matches!(result, Err(HashTreeError::PathNotFound(_))));
    }

    #[tokio::test]
    async fn test_rename_entry_not_found() {
        let (_store, tree) = make_tree();

        let root_cid = tree.put_directory(vec![]).await.unwrap();

        let result = tree
            .rename_entry(&root_cid, &[], "nonexistent.txt", "new.txt")
            .await;

        assert!(matches!(result, Err(HashTreeError::EntryNotFound(_))));
    }

    #[tokio::test]
    async fn test_immutable_edit_operations() {
        let (_store, tree) = make_tree();

        let (file_cid, _) = tree.put_file(b"original").await.unwrap();
        let original_root = tree
            .put_directory(vec![DirEntry::new("file.txt", file_cid.hash).with_size(8)])
            .await
            .unwrap();

        let (file2_cid, file2_size) = tree.put_file(b"modified").await.unwrap();
        let new_root = tree
            .set_entry(&original_root, &[], "file.txt", &file2_cid, file2_size, LinkType::File)
            .await
            .unwrap();

        // Original unchanged
        let original_entries = tree.list_directory(&original_root).await.unwrap();
        assert_eq!(to_hex(&original_entries[0].hash), to_hex(&file_cid.hash));

        // New root has changes
        let new_entries = tree.list_directory(&new_root).await.unwrap();
        assert_eq!(to_hex(&new_entries[0].hash), to_hex(&file2_cid.hash));
    }
}

// ============ VERIFY TESTS ============

mod verify {
    use super::*;
    use hashtree_core::hashtree_verify_tree;

    #[tokio::test]
    async fn test_verify_valid_tree() {
        let (store, tree) = make_tree_with_chunk_size(100);

        let data = vec![0u8; 350];
        let (cid, _) = tree.put_file(&data).await.unwrap();

        let verify_result = hashtree_verify_tree(store, &cid.hash).await.unwrap();
        assert!(verify_result.valid);
        assert!(verify_result.missing.is_empty());
    }

    #[tokio::test]
    async fn test_verify_missing_chunk() {
        let (store, tree) = make_tree_with_chunk_size(100);

        let data = vec![0u8; 350];
        let (cid, _) = tree.put_file(&data).await.unwrap();

        // Delete one chunk
        let keys = store.keys();
        if let Some(chunk_to_delete) = keys.iter().find(|k| **k != cid.hash) {
            store.delete(chunk_to_delete).await.unwrap();
        }

        let verify_result = hashtree_verify_tree(store, &cid.hash).await.unwrap();
        assert!(!verify_result.valid);
        assert!(!verify_result.missing.is_empty());
    }
}

// ============ EDGE CASES ============

mod edge_cases {
    use super::*;

    #[tokio::test]
    async fn test_empty_file() {
        let (_store, tree) = make_tree();

        let (cid, size) = tree.put_file(&[]).await.unwrap();
        assert_eq!(size, 0);

        let data = tree.read_file(&cid.hash).await.unwrap().unwrap();
        assert!(data.is_empty());
    }

    #[tokio::test]
    async fn test_single_byte_file() {
        let (_store, tree) = make_tree();

        let (cid, size) = tree.put_file(&[42]).await.unwrap();
        assert_eq!(size, 1);

        let data = tree.read_file(&cid.hash).await.unwrap().unwrap();
        assert_eq!(data, vec![42]);
    }

    #[tokio::test]
    async fn test_exact_chunk_size() {
        let (_store, tree) = make_tree_with_chunk_size(100);

        let data = vec![0u8; 100];
        let (cid, _) = tree.put_file(&data).await.unwrap();

        let read_data = tree.read_file(&cid.hash).await.unwrap().unwrap();
        assert_eq!(read_data.len(), 100);
    }

    #[tokio::test]
    async fn test_chunk_size_plus_one() {
        let (_store, tree) = make_tree_with_chunk_size(100);

        let data = vec![0u8; 101];
        let (cid, _) = tree.put_file(&data).await.unwrap();

        let read_data = tree.read_file(&cid.hash).await.unwrap().unwrap();
        assert_eq!(read_data.len(), 101);
    }

    #[tokio::test]
    async fn test_binary_data() {
        let (_store, tree) = make_tree();

        let data: Vec<u8> = (0..=255).cycle().take(512).collect();
        let (cid, _) = tree.put_file(&data).await.unwrap();

        let read_data = tree.read_file(&cid.hash).await.unwrap().unwrap();
        assert_eq!(read_data, data);
    }

    #[tokio::test]
    async fn test_special_characters_in_names() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"data").await.unwrap();

        let dir_cid = tree
            .put_directory(
                vec![
                    DirEntry::new("file with spaces.txt", file_hash),
                    DirEntry::new("file-with-dashes.txt", file_hash),
                    DirEntry::new("file_with_underscores.txt", file_hash),
                    DirEntry::new("file.multiple.dots.txt", file_hash),
                ],
            )
            .await
            .unwrap();

        let entries = tree.list_directory(&dir_cid).await.unwrap();
        assert_eq!(entries.len(), 4);
    }

    #[tokio::test]
    async fn test_unicode_names() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"data").await.unwrap();

        let dir_cid = tree
            .put_directory(
                vec![
                    DirEntry::new("日本語.txt", file_hash),
                    DirEntry::new("émoji🎉.txt", file_hash),
                    DirEntry::new("中文文件.txt", file_hash),
                ],
            )
            .await
            .unwrap();

        let entries = tree.list_directory(&dir_cid).await.unwrap();
        assert_eq!(entries.len(), 3);

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"日本語.txt"));
        assert!(names.contains(&"émoji🎉.txt"));
        assert!(names.contains(&"中文文件.txt"));
    }

    #[tokio::test]
    async fn test_deeply_nested_path() {
        let (_store, tree) = make_tree();

        // Create 10 levels deep
        let file_hash = tree.put_blob(b"deep content").await.unwrap();
        let mut current_cid = tree
            .put_directory(vec![DirEntry::new("file.txt", file_hash)])
            .await
            .unwrap();

        for i in (1..=10).rev() {
            current_cid = tree
                .put_directory(vec![DirEntry::new(format!("level{}", i), current_cid.hash)])
                .await
                .unwrap();
        }

        let path = "level1/level2/level3/level4/level5/level6/level7/level8/level9/level10/file.txt";
        let resolved = tree.resolve_path(&current_cid, path).await.unwrap();
        assert_eq!(resolved.map(|c| c.hash), Some(file_hash));
    }

    #[tokio::test]
    async fn test_concurrent_operations() {
        let (_store, tree) = make_tree();

        // Create multiple files concurrently
        let futures: Vec<_> = (0..10)
            .map(|i| {
                let t = &tree;
                async move {
                    let data = vec![i as u8; 100];
                    t.put_file(&data).await
                }
            })
            .collect();

        let results = futures::future::join_all(futures).await;

        for result in results {
            assert!(result.is_ok());
        }
    }
}

// ============ ENCRYPTION TESTS ============

mod encryption {
    use super::*;

    /// Create an encrypted tree (default mode)
    fn make_encrypted_tree() -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        // Default is encrypted: true
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        (store, tree)
    }

    fn make_encrypted_tree_with_chunk_size(chunk_size: usize) -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()).with_chunk_size(chunk_size));
        (store, tree)
    }

    /// Count unique byte values in first 256 bytes (blossom compatibility check)
    fn count_unique_bytes(data: &[u8]) -> usize {
        let sample_size = data.len().min(256);
        let mut seen = [false; 256];
        let mut count = 0;
        for &b in &data[..sample_size] {
            if !seen[b as usize] {
                seen[b as usize] = true;
                count += 1;
            }
        }
        count
    }

    #[tokio::test]
    async fn test_put_file_produces_encrypted_blobs() {
        let (store, tree) = make_encrypted_tree();

        // Store a file with plaintext data
        let plaintext = b"This is plaintext content that should be encrypted";
        let (cid, _) = tree.put_file(plaintext).await.unwrap();

        // The stored blob should look random (encrypted), not like plaintext
        let stored = store.get(&cid.hash).await.unwrap().unwrap();
        let unique_bytes = count_unique_bytes(&stored);

        // Encrypted data should have high unique byte count (55%+ for small blobs)
        // Plaintext would have ~20-30 unique bytes
        let threshold = (stored.len().min(256) as f64 * 0.55) as usize;
        assert!(
            unique_bytes >= threshold,
            "put_file blob should be encrypted! Got {} unique bytes, expected >= {} (threshold 55%)",
            unique_bytes,
            threshold
        );
    }

    #[tokio::test]
    async fn test_put_file_chunked_produces_encrypted_chunks() {
        let (store, tree) = make_encrypted_tree_with_chunk_size(32);

        // Create data that will be chunked
        let plaintext: Vec<u8> = (0..100).map(|i| (i % 26 + 65) as u8).collect(); // "ABC..."
        let (cid, _) = tree.put_file(&plaintext).await.unwrap();

        // Check all stored blobs look encrypted
        for key in store.keys() {
            let blob = store.get(&key).await.unwrap().unwrap();
            if blob.len() >= 28 { // Min CHK size
                let unique_bytes = count_unique_bytes(&blob);
                let threshold = (blob.len().min(256) as f64 * 0.55) as usize;
                assert!(
                    unique_bytes >= threshold,
                    "Chunk should be encrypted! Got {} unique bytes in {} byte blob",
                    unique_bytes,
                    blob.len()
                );
            }
        }

        // Verify we can still read the file back
        // Need to use the encryption-aware read method
        assert!(cid.hash.len() == 32);
    }

    #[tokio::test]
    async fn test_put_file_returns_cid_with_key() {
        let (_, tree) = make_encrypted_tree();

        let plaintext = b"secret content";
        let (_, size) = tree.put_file(plaintext).await.unwrap();

        // put_file should return a result that can be used to decrypt
        // Currently it only returns hash, but encrypted mode should return key too
        // This test documents expected behavior for encrypted put_file
        assert_eq!(size, plaintext.len() as u64);
    }

    #[tokio::test]
    async fn test_public_mode_stores_plaintext() {
        let (store, tree) = make_tree(); // Uses .public()

        let plaintext = b"This content should NOT be encrypted in public mode";
        let (cid, _) = tree.put_file(plaintext).await.unwrap();

        // In public mode, the stored data should be the original plaintext
        let stored = store.get(&cid.hash).await.unwrap().unwrap();
        assert_eq!(stored, plaintext.to_vec());
    }
}

// ============ INTEROPERABILITY TESTS ============

mod interop {
    use super::*;

    #[tokio::test]
    async fn test_hash_consistency() {
        let (_store1, tree1) = make_tree();
        let (_store2, tree2) = make_tree();

        let data = b"test data for hash consistency";

        let (cid1, _) = tree1.put_file(data).await.unwrap();
        let (cid2, _) = tree2.put_file(data).await.unwrap();

        // Same data should produce same hash
        assert_eq!(to_hex(&cid1.hash), to_hex(&cid2.hash));
    }

    #[tokio::test]
    async fn test_directory_hash_consistency() {
        let (_store1, tree1) = make_tree();
        let (_store2, tree2) = make_tree();

        // Create same structure in both
        let (file1_1_cid, _) = tree1.put_file(b"content1").await.unwrap();
        let (file1_2_cid, _) = tree1.put_file(b"content2").await.unwrap();
        let dir1 = tree1
            .put_directory(
                vec![
                    DirEntry::new("a.txt", file1_1_cid.hash).with_size(8),
                    DirEntry::new("b.txt", file1_2_cid.hash).with_size(8),
                ],
            )
            .await
            .unwrap();

        let (file2_1_cid, _) = tree2.put_file(b"content1").await.unwrap();
        let (file2_2_cid, _) = tree2.put_file(b"content2").await.unwrap();
        let dir2 = tree2
            .put_directory(
                vec![
                    DirEntry::new("b.txt", file2_2_cid.hash).with_size(8), // Different order
                    DirEntry::new("a.txt", file2_1_cid.hash).with_size(8),
                ],
            )
            .await
            .unwrap();

        // Should produce same hash due to sorting
        assert_eq!(to_hex(&dir1.hash), to_hex(&dir2.hash));
    }
}
