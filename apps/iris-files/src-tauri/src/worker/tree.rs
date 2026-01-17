//! Tree operations using hashtree-core
//!
//! Provides read/write/list operations for content-addressed merkle trees.

use hashtree_core::{try_decode_tree_node, Cid, HashTree, HashTreeConfig, LinkType, Store};
use hashtree_core::crypto::decrypt_chk;
use std::collections::HashSet;
use std::sync::Arc;

use super::combined_store::CombinedStore;
use super::store::BlobStore;
use super::types::{WorkerCid, WorkerDirEntry};

/// Block from tree walk
pub struct WalkBlock {
    pub hash: [u8; 32],
    pub data: Vec<u8>,
}

/// Tree manager for worker operations
pub struct TreeManager {
    tree: HashTree<CombinedStore>,
    combined_store: Arc<CombinedStore>,
    store: Arc<BlobStore>,
}

impl TreeManager {
    pub fn new(store: Arc<BlobStore>) -> Self {
        // Create combined store with Blossom fallback
        let combined_store = Arc::new(CombinedStore::new(store.inner()));
        let config = HashTreeConfig::new(combined_store.clone()).public();
        let tree = HashTree::new(config);
        Self { tree, combined_store, store }
    }

    /// Update Blossom read servers for remote fetching
    pub async fn set_blossom_servers(&self, read_servers: Vec<String>) {
        self.combined_store.set_blossom_servers(read_servers, None).await;
    }

    /// Get blob from combined store (tries local first, then Blossom)
    pub async fn get_blob(&self, hash_hex: &str) -> Option<Vec<u8>> {
        let hash = hashtree_core::from_hex(hash_hex).ok()?;
        self.combined_store.get(&hash).await.ok().flatten()
    }

    /// Walk all blocks in a merkle tree, returning each block's hash and data.
    /// Handles both encrypted and unencrypted trees.
    pub async fn walk_blocks(&self, cid: &WorkerCid) -> Result<Vec<WalkBlock>, String> {
        let hash = hashtree_core::from_hex(&cid.hash)
            .map_err(|e| format!("Invalid hash: {}", e))?;

        let key = if let Some(key_hex) = &cid.key {
            Some(hashtree_core::key_from_hex(key_hex)
                .map_err(|e| format!("Invalid key: {}", e))?)
        } else {
            None
        };

        let mut blocks = Vec::new();
        let mut visited = HashSet::new();

        self.walk_blocks_recursive(&hash, key.as_ref(), &mut blocks, &mut visited).await?;
        Ok(blocks)
    }

    /// Recursive helper for walk_blocks
    async fn walk_blocks_recursive(
        &self,
        hash: &[u8; 32],
        key: Option<&[u8; 32]>,
        blocks: &mut Vec<WalkBlock>,
        visited: &mut HashSet<[u8; 32]>,
    ) -> Result<(), String> {
        if visited.contains(hash) {
            return Ok(());
        }
        visited.insert(*hash);

        // Get raw data from store
        let data = match self.store.get(&hashtree_core::to_hex(hash)).await {
            Some(d) => d,
            None => return Ok(()), // Block not found, skip
        };

        // Add this block
        blocks.push(WalkBlock {
            hash: *hash,
            data: data.clone(),
        });

        // Try to decode as tree node to find children
        if let Some(key) = key {
            // Encrypted tree - decrypt first
            if let Ok(decrypted) = decrypt_chk(&data, key) {
                if let Some(node) = try_decode_tree_node(&decrypted) {
                    for link in node.links {
                        Box::pin(self.walk_blocks_recursive(&link.hash, link.key.as_ref(), blocks, visited)).await?;
                    }
                }
            }
        } else {
            // Unencrypted tree - try decode directly
            if let Some(node) = try_decode_tree_node(&data) {
                for link in node.links {
                    Box::pin(self.walk_blocks_recursive(&link.hash, link.key.as_ref(), blocks, visited)).await?;
                }
            }
        }

        Ok(())
    }

    /// Convert WorkerCid to hashtree_core::Cid
    fn to_cid(worker_cid: &WorkerCid) -> Result<Cid, String> {
        let hash = hashtree_core::from_hex(&worker_cid.hash)
            .map_err(|e| format!("Invalid hash: {}", e))?;

        let key = if let Some(key_hex) = &worker_cid.key {
            Some(
                hashtree_core::key_from_hex(key_hex)
                    .map_err(|e| format!("Invalid key: {}", e))?,
            )
        } else {
            None
        };

        Ok(Cid { hash, key })
    }

    /// Convert hashtree_core::Cid to WorkerCid
    fn from_cid(cid: &Cid) -> WorkerCid {
        WorkerCid {
            hash: hashtree_core::to_hex(&cid.hash),
            key: cid.key.map(|k| hashtree_core::key_to_hex(&k)),
        }
    }

    /// Read file content by CID
    pub async fn read_file(&self, cid: &WorkerCid) -> Result<Vec<u8>, String> {
        let cid = Self::to_cid(cid)?;
        self.tree
            .get(&cid)
            .await
            .map_err(|e| format!("Read error: {}", e))?
            .ok_or_else(|| "File not found".to_string())
    }

    /// Read a byte range from a file (fetches only necessary chunks)
    pub async fn read_file_range(
        &self,
        cid: &WorkerCid,
        start: u64,
        end: Option<u64>,
    ) -> Result<Vec<u8>, String> {
        let cid = Self::to_cid(cid)?;
        self.tree
            .read_file_range(&cid.hash, start, end)
            .await
            .map_err(|e| format!("Range read error: {}", e))?
            .ok_or_else(|| "File not found".to_string())
    }

    /// Write file to tree, returns new root CID
    pub async fn write_file(
        &self,
        parent_cid: Option<&WorkerCid>,
        path: &str,
        data: &[u8],
    ) -> Result<WorkerCid, String> {
        // First, store the file content
        let (file_cid, file_size) = self
            .tree
            .put(data)
            .await
            .map_err(|e| format!("Write error: {}", e))?;

        // If we have a parent, add entry to it
        if let Some(parent) = parent_cid {
            let parent_cid = Self::to_cid(parent)?;

            // Parse path to get directory path and filename
            let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            if path_parts.is_empty() {
                return Err("Empty path".to_string());
            }

            let filename = path_parts.last().unwrap();
            let dir_path: Vec<&str> = path_parts[..path_parts.len() - 1].to_vec();

            let new_root = self
                .tree
                .set_entry(
                    &parent_cid,
                    &dir_path,
                    filename,
                    &file_cid,
                    file_size,
                    LinkType::Blob,
                )
                .await
                .map_err(|e| format!("Set entry error: {}", e))?;

            Ok(Self::from_cid(&new_root))
        } else {
            // No parent - just return the file CID
            Ok(Self::from_cid(&file_cid))
        }
    }

    /// Delete file from tree, returns new root CID
    pub async fn delete_file(
        &self,
        parent_cid: &WorkerCid,
        path: &str,
    ) -> Result<WorkerCid, String> {
        let parent_cid = Self::to_cid(parent_cid)?;

        // Parse path to get directory path and filename
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if path_parts.is_empty() {
            return Err("Empty path".to_string());
        }

        let filename = path_parts.last().unwrap();
        let dir_path: Vec<&str> = path_parts[..path_parts.len() - 1].to_vec();

        let new_root = self
            .tree
            .remove_entry(&parent_cid, &dir_path, filename)
            .await
            .map_err(|e| format!("Delete error: {}", e))?;

        Ok(Self::from_cid(&new_root))
    }

    /// List directory contents
    pub async fn list_dir(&self, cid: &WorkerCid) -> Result<Vec<WorkerDirEntry>, String> {
        let cid = Self::to_cid(cid)?;

        let entries = self
            .tree
            .list_directory(&cid)
            .await
            .map_err(|e| format!("List error: {}", e))?;

        Ok(entries
            .into_iter()
            .map(|e| WorkerDirEntry {
                name: e.name,
                hash: hashtree_core::to_hex(&e.hash),
                size: e.size,
                link_type: e.link_type as u8,
                key: e.key.map(|k| hashtree_core::key_to_hex(&k)),
            })
            .collect())
    }

    /// Create an empty directory, returns CID
    pub async fn create_empty_dir(&self) -> Result<WorkerCid, String> {
        let cid = self
            .tree
            .put_directory(vec![])
            .await
            .map_err(|e| format!("Create dir error: {}", e))?;

        Ok(Self::from_cid(&cid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Returns both the manager and the TempDir to keep it alive during the test
    async fn create_test_manager() -> (TreeManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::new(dir.path().to_path_buf()));
        (TreeManager::new(store), dir)
    }

    #[tokio::test]
    async fn test_write_and_read_file() {
        let (manager, _dir) = create_test_manager().await;

        // Write file without parent
        let data = b"Hello, World!";
        let cid = manager.write_file(None, "test.txt", data).await.unwrap();

        // Read it back
        let result = manager.read_file(&cid).await.unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_create_empty_dir() {
        let (manager, _dir) = create_test_manager().await;

        let dir_cid = manager.create_empty_dir().await.unwrap();

        // List should be empty
        let entries = manager.list_dir(&dir_cid).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_write_to_directory() {
        let (manager, _dir) = create_test_manager().await;

        // Create empty dir
        let dir_cid = manager.create_empty_dir().await.unwrap();

        // Write file to it
        let data = b"File content";
        let new_root = manager
            .write_file(Some(&dir_cid), "test.txt", data)
            .await
            .unwrap();

        // List should have one entry
        let entries = manager.list_dir(&new_root).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "test.txt");
        assert_eq!(entries[0].size, data.len() as u64);
    }

    #[tokio::test]
    async fn test_delete_file() {
        let (manager, _dir) = create_test_manager().await;

        // Create dir with file
        let dir_cid = manager.create_empty_dir().await.unwrap();
        let with_file = manager
            .write_file(Some(&dir_cid), "test.txt", b"content")
            .await
            .unwrap();

        // Delete file
        let after_delete = manager.delete_file(&with_file, "test.txt").await.unwrap();

        // List should be empty again
        let entries = manager.list_dir(&after_delete).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_write_file_returns_file_cid() {
        let (manager, _dir) = create_test_manager().await;

        let data = b"test content";
        let cid = manager.write_file(None, "file.txt", data).await.unwrap();

        // The CID should be valid and readable
        let content = manager.read_file(&cid).await.unwrap();
        assert_eq!(content, data);
    }

    #[test]
    fn test_cid_conversion() {
        let worker_cid = WorkerCid {
            hash: "a".repeat(64), // 32 bytes hex
            key: Some("b".repeat(64)),
        };

        // This should parse successfully
        let cid = TreeManager::to_cid(&worker_cid);
        assert!(cid.is_ok());

        let cid = cid.unwrap();
        let back = TreeManager::from_cid(&cid);
        assert_eq!(back.hash, worker_cid.hash);
        assert_eq!(back.key, worker_cid.key);
    }

    #[test]
    fn test_cid_conversion_no_key() {
        let worker_cid = WorkerCid {
            hash: "c".repeat(64),
            key: None,
        };

        let cid = TreeManager::to_cid(&worker_cid).unwrap();
        assert!(cid.key.is_none());

        let back = TreeManager::from_cid(&cid);
        assert_eq!(back.hash, worker_cid.hash);
        assert!(back.key.is_none());
    }

    #[test]
    fn test_invalid_hash() {
        let worker_cid = WorkerCid {
            hash: "invalid".to_string(),
            key: None,
        };

        let result = TreeManager::to_cid(&worker_cid);
        assert!(result.is_err());
    }

    /// E2E test: Fetch media tree from Blossom and list directory contents
    /// This tests the CombinedStore Blossom fallback for tree operations
    #[tokio::test]
    async fn test_list_media_tree_from_blossom() {
        // The media tree root CID (known to exist on Blossom)
        let media_tree_cid = WorkerCid {
            hash: "e4190b9acd45e5d4675f0a46447a63aa155646d77f734f2c3940184b9a877671".to_string(),
            key: Some("49e0803c8a08c2501547d46786b9f3cc2ba4dfab9ec038ffa100a61f7c28e200".to_string()),
        };

        // Create TreeManager with empty local store (forces Blossom fetch)
        let (manager, _dir) = create_test_manager().await;

        // List directory - should fetch from Blossom via CombinedStore
        println!("Listing media tree directory from Blossom...");
        let result = manager.list_dir(&media_tree_cid).await;

        match result {
            Ok(entries) => {
                println!("SUCCESS! Found {} entries in media tree:", entries.len());
                for entry in &entries {
                    println!("  - {}: hash={} size={}", entry.name, &entry.hash[..16], entry.size);
                }
                // The media tree should NOT be empty
                assert!(!entries.is_empty(), "Media tree should not be empty");
            }
            Err(e) => {
                panic!("Failed to list media tree: {}. This indicates CombinedStore Blossom fallback is not working.", e);
            }
        }
    }
}
