//! Tree reader and traversal utilities
//!
//! Read files and directories from content-addressed storage

use std::collections::HashMap;
use std::sync::Arc;

use crate::codec::{decode_tree_node, is_directory_node, is_tree_node, try_decode_tree_node};
use crate::store::Store;
use crate::types::{to_hex, Cid, Hash, Link, LinkType, TreeNode};

use crate::crypto::{decrypt_chk, EncryptionKey};

/// Tree entry for directory listings
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: String,
    pub hash: Hash,
    pub size: u64,
    /// Type of content this entry points to (Blob, File, or Dir)
    pub link_type: LinkType,
    /// Optional decryption key (for encrypted content)
    pub key: Option<[u8; 32]>,
    /// Optional metadata (createdAt, mimeType, thumbnail, etc.)
    pub meta: Option<HashMap<String, serde_json::Value>>,
}

/// Walk entry for tree traversal
#[derive(Debug, Clone)]
pub struct WalkEntry {
    pub path: String,
    pub hash: Hash,
    /// Type of content this entry points to (Blob, File, or Dir)
    pub link_type: LinkType,
    pub size: u64,
    /// Optional decryption key (for encrypted content)
    pub key: Option<[u8; 32]>,
}

/// TreeReader - reads and traverses merkle trees
pub struct TreeReader<S: Store> {
    store: Arc<S>,
}

impl<S: Store> TreeReader<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    /// Get raw data by hash
    pub async fn get_blob(&self, hash: &Hash) -> Result<Option<Vec<u8>>, ReaderError> {
        self.store
            .get(hash)
            .await
            .map_err(|e| ReaderError::Store(e.to_string()))
    }

    /// Get and decode a tree node
    pub async fn get_tree_node(&self, hash: &Hash) -> Result<Option<TreeNode>, ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        if !is_tree_node(&data) {
            return Ok(None); // It's a blob, not a tree
        }

        let node = decode_tree_node(&data).map_err(ReaderError::Codec)?;
        Ok(Some(node))
    }

    /// Check if hash points to a tree node or blob
    pub async fn is_tree(&self, hash: &Hash) -> Result<bool, ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(false),
        };
        Ok(is_tree_node(&data))
    }

    /// Check if hash points to a directory (tree with named links)
    /// vs a chunked file (tree with unnamed links) or raw blob
    pub async fn is_directory(&self, hash: &Hash) -> Result<bool, ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(false),
        };
        Ok(is_directory_node(&data))
    }

    /// Read content by CID (handles both encrypted and public content)
    ///
    /// This is the unified read method that handles decryption automatically
    /// when the CID contains an encryption key.
    pub async fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>, ReaderError> {
        if let Some(key) = cid.key {
            self.get_encrypted(&cid.hash, &key).await
        } else {
            self.read_file(&cid.hash).await
        }
    }

    /// Read encrypted content by hash and key (internal)
    async fn get_encrypted(
        &self,
        hash: &Hash,
        key: &EncryptionKey,
    ) -> Result<Option<Vec<u8>>, ReaderError> {
        let encrypted_data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        // Decrypt the data
        let decrypted = decrypt_chk(&encrypted_data, key)
            .map_err(|e| ReaderError::Decryption(e.to_string()))?;

        // Check if it's a tree node
        if is_tree_node(&decrypted) {
            let node = decode_tree_node(&decrypted)?;
            let assembled = self.assemble_encrypted_chunks(&node).await?;
            return Ok(Some(assembled));
        }

        // Single chunk data
        Ok(Some(decrypted))
    }

    /// Assemble encrypted chunks from tree
    async fn assemble_encrypted_chunks(&self, node: &TreeNode) -> Result<Vec<u8>, ReaderError> {
        let mut parts: Vec<Vec<u8>> = Vec::new();

        for link in &node.links {
            let chunk_key = link.key.ok_or(ReaderError::MissingKey)?;

            let encrypted_child = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| ReaderError::Store(e.to_string()))?
                .ok_or_else(|| ReaderError::MissingChunk(to_hex(&link.hash)))?;

            let decrypted = decrypt_chk(&encrypted_child, &chunk_key)
                .map_err(|e| ReaderError::Decryption(e.to_string()))?;

            if is_tree_node(&decrypted) {
                // Intermediate tree node - recurse
                let child_node = decode_tree_node(&decrypted)?;
                let child_data = Box::pin(self.assemble_encrypted_chunks(&child_node)).await?;
                parts.push(child_data);
            } else {
                // Leaf data chunk
                parts.push(decrypted);
            }
        }

        let total_len: usize = parts.iter().map(|p| p.len()).sum();
        let mut result = Vec::with_capacity(total_len);
        for part in parts {
            result.extend_from_slice(&part);
        }

        Ok(result)
    }

    /// Read a complete file (reassemble chunks if needed)
    /// For unencrypted content only - use `get()` for unified access
    pub async fn read_file(&self, hash: &Hash) -> Result<Option<Vec<u8>>, ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        // Check if it's a tree (chunked file) or raw blob
        if !is_tree_node(&data) {
            return Ok(Some(data)); // Direct blob
        }

        // It's a tree - reassemble chunks
        let node = decode_tree_node(&data).map_err(ReaderError::Codec)?;
        let assembled = self.assemble_chunks(&node).await?;
        Ok(Some(assembled))
    }

    /// Read a byte range from a file (fetches only necessary chunks)
    ///
    /// - `start`: Starting byte offset (inclusive)
    /// - `end`: Ending byte offset (exclusive), or None to read to end
    ///
    /// For unencrypted content only - encrypted range reads not yet supported.
    pub async fn read_file_range(
        &self,
        hash: &Hash,
        start: u64,
        end: Option<u64>,
    ) -> Result<Option<Vec<u8>>, ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        // Single blob - just slice it
        if !is_tree_node(&data) {
            let start_idx = start as usize;
            let end_idx = end.map(|e| e as usize).unwrap_or(data.len());
            if start_idx >= data.len() {
                return Ok(Some(vec![]));
            }
            let end_idx = end_idx.min(data.len());
            return Ok(Some(data[start_idx..end_idx].to_vec()));
        }

        // It's a chunked file - fetch only needed chunks
        let node = decode_tree_node(&data).map_err(ReaderError::Codec)?;
        let range_data = self.assemble_chunks_range(&node, start, end).await?;
        Ok(Some(range_data))
    }

    /// Assemble only the chunks needed for a byte range
    async fn assemble_chunks_range(
        &self,
        node: &TreeNode,
        start: u64,
        end: Option<u64>,
    ) -> Result<Vec<u8>, ReaderError> {
        // First, flatten the tree to get all leaf chunks with their byte offsets
        let chunks_info = self.collect_chunk_offsets(node).await?;

        if chunks_info.is_empty() {
            return Ok(vec![]);
        }

        // Calculate total size and actual end
        let total_size: u64 = chunks_info.iter().map(|(_, _, size)| size).sum();
        let actual_end = end.unwrap_or(total_size).min(total_size);

        if start >= actual_end {
            return Ok(vec![]);
        }

        // Find chunks that overlap with [start, actual_end)
        let mut result = Vec::with_capacity((actual_end - start) as usize);
        let mut current_offset = 0u64;

        for (chunk_hash, _chunk_offset, chunk_size) in &chunks_info {
            let chunk_start = current_offset;
            let chunk_end = current_offset + chunk_size;

            // Check if this chunk overlaps with our range
            if chunk_end > start && chunk_start < actual_end {
                // Fetch this chunk
                let chunk_data = self
                    .store
                    .get(chunk_hash)
                    .await
                    .map_err(|e| ReaderError::Store(e.to_string()))?
                    .ok_or_else(|| ReaderError::MissingChunk(to_hex(chunk_hash)))?;

                // Calculate slice bounds within this chunk
                let slice_start = if start > chunk_start {
                    (start - chunk_start) as usize
                } else {
                    0
                };
                let slice_end = if actual_end < chunk_end {
                    (actual_end - chunk_start) as usize
                } else {
                    chunk_data.len()
                };

                result.extend_from_slice(&chunk_data[slice_start..slice_end]);
            }

            current_offset = chunk_end;

            // Early exit if we've passed the requested range
            if current_offset >= actual_end {
                break;
            }
        }

        Ok(result)
    }

    /// Collect all leaf chunk hashes with their byte offsets
    /// Returns Vec<(hash, offset, size)>
    async fn collect_chunk_offsets(
        &self,
        node: &TreeNode,
    ) -> Result<Vec<(Hash, u64, u64)>, ReaderError> {
        let mut chunks = Vec::new();
        let mut offset = 0u64;
        self.collect_chunk_offsets_recursive(node, &mut chunks, &mut offset).await?;
        Ok(chunks)
    }

    async fn collect_chunk_offsets_recursive(
        &self,
        node: &TreeNode,
        chunks: &mut Vec<(Hash, u64, u64)>,
        offset: &mut u64,
    ) -> Result<(), ReaderError> {
        for link in &node.links {
            let child_data = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| ReaderError::Store(e.to_string()))?
                .ok_or_else(|| ReaderError::MissingChunk(to_hex(&link.hash)))?;

            if is_tree_node(&child_data) {
                // Intermediate node - recurse
                let child_node = decode_tree_node(&child_data).map_err(ReaderError::Codec)?;
                Box::pin(self.collect_chunk_offsets_recursive(&child_node, chunks, offset)).await?;
            } else {
                // Leaf chunk
                let size = child_data.len() as u64;
                chunks.push((link.hash, *offset, size));
                *offset += size;
            }
        }
        Ok(())
    }

    /// Recursively assemble chunks from tree (unencrypted)
    async fn assemble_chunks(&self, node: &TreeNode) -> Result<Vec<u8>, ReaderError> {
        let mut parts: Vec<Vec<u8>> = Vec::new();

        for link in &node.links {
            let child_data = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| ReaderError::Store(e.to_string()))?
                .ok_or_else(|| ReaderError::MissingChunk(to_hex(&link.hash)))?;

            if is_tree_node(&child_data) {
                // Nested tree - recurse
                let child_node = decode_tree_node(&child_data).map_err(ReaderError::Codec)?;
                parts.push(Box::pin(self.assemble_chunks(&child_node)).await?);
            } else {
                // Leaf blob
                parts.push(child_data);
            }
        }

        // Concatenate all parts
        let total_length: usize = parts.iter().map(|p| p.len()).sum();
        let mut result = Vec::with_capacity(total_length);
        for part in parts {
            result.extend_from_slice(&part);
        }

        Ok(result)
    }

    /// Read a file with streaming (returns chunks as vec)
    pub async fn read_file_chunks(&self, hash: &Hash) -> Result<Vec<Vec<u8>>, ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(vec![]),
        };

        if !is_tree_node(&data) {
            return Ok(vec![data]);
        }

        let node = decode_tree_node(&data).map_err(ReaderError::Codec)?;
        self.collect_chunks(&node).await
    }

    /// Recursively collect chunks
    async fn collect_chunks(&self, node: &TreeNode) -> Result<Vec<Vec<u8>>, ReaderError> {
        let mut chunks = Vec::new();

        for link in &node.links {
            let child_data = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| ReaderError::Store(e.to_string()))?
                .ok_or_else(|| ReaderError::MissingChunk(to_hex(&link.hash)))?;

            if is_tree_node(&child_data) {
                let child_node = decode_tree_node(&child_data).map_err(ReaderError::Codec)?;
                chunks.extend(Box::pin(self.collect_chunks(&child_node)).await?);
            } else {
                chunks.push(child_data);
            }
        }

        Ok(chunks)
    }

    /// List directory entries
    pub async fn list_directory(&self, hash: &Hash) -> Result<Vec<TreeEntry>, ReaderError> {
        let node = match self.get_tree_node(hash).await? {
            Some(n) => n,
            None => return Ok(vec![]),
        };

        let mut entries = Vec::new();

        for link in &node.links {
            // Skip internal chunk nodes (names starting with _chunk_)
            if let Some(ref name) = link.name {
                if name.starts_with("_chunk_") {
                    // This is an internal split - recurse into it
                    let sub_entries = Box::pin(self.list_directory(&link.hash)).await?;
                    entries.extend(sub_entries);
                    continue;
                }

                // Skip internal group nodes (names starting with _ but not _chunk_)
                if name.starts_with('_') {
                    let sub_entries = Box::pin(self.list_directory(&link.hash)).await?;
                    entries.extend(sub_entries);
                    continue;
                }
            }

            entries.push(TreeEntry {
                name: link.name.clone().unwrap_or_else(|| to_hex(&link.hash)),
                hash: link.hash,
                size: link.size,
                link_type: link.link_type,
                key: link.key,
                meta: link.meta.clone(),
            });
        }

        Ok(entries)
    }

    /// Resolve a path within a tree
    /// e.g., resolve_path("root/foo/bar.txt")
    pub async fn resolve_path(&self, root_hash: &Hash, path: &str) -> Result<Option<Hash>, ReaderError> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();

        let mut current_hash = *root_hash;

        for part in parts {
            let node = match self.get_tree_node(&current_hash).await? {
                Some(n) => n,
                None => return Ok(None),
            };

            if let Some(link) = self.find_link(&node, part) {
                current_hash = link.hash;
            } else {
                // Check internal nodes
                match self.find_in_subtrees(&node, part).await? {
                    Some(hash) => current_hash = hash,
                    None => return Ok(None),
                }
            }
        }

        Ok(Some(current_hash))
    }

    /// Find a link by name in a tree node
    fn find_link(&self, node: &TreeNode, name: &str) -> Option<Link> {
        node.links
            .iter()
            .find(|l| l.name.as_deref() == Some(name))
            .cloned()
    }

    /// Search for name in internal subtrees
    async fn find_in_subtrees(&self, node: &TreeNode, name: &str) -> Result<Option<Hash>, ReaderError> {
        for link in &node.links {
            // Only search internal nodes
            if !link.name.as_ref().map(|n| n.starts_with('_')).unwrap_or(false) {
                continue;
            }

            let sub_node = match self.get_tree_node(&link.hash).await? {
                Some(n) => n,
                None => continue,
            };

            if let Some(found) = self.find_link(&sub_node, name) {
                return Ok(Some(found.hash));
            }

            // Recurse deeper
            if let Some(deep_found) = Box::pin(self.find_in_subtrees(&sub_node, name)).await? {
                return Ok(Some(deep_found));
            }
        }

        Ok(None)
    }

    /// Get total size of a tree
    pub async fn get_size(&self, hash: &Hash) -> Result<u64, ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(0),
        };

        if !is_tree_node(&data) {
            return Ok(data.len() as u64);
        }

        let node = decode_tree_node(&data).map_err(ReaderError::Codec)?;
        // Calculate from children
        let mut total = 0u64;
        for link in &node.links {
            total += link.size;
        }
        Ok(total)
    }

    /// Walk entire tree depth-first
    pub async fn walk(&self, hash: &Hash, path: &str) -> Result<Vec<WalkEntry>, ReaderError> {
        let mut entries = Vec::new();
        self.walk_recursive(hash, path, &mut entries).await?;
        Ok(entries)
    }

    async fn walk_recursive(
        &self,
        hash: &Hash,
        path: &str,
        entries: &mut Vec<WalkEntry>,
    ) -> Result<(), ReaderError> {
        let data = match self.store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(()),
        };

        let node = match try_decode_tree_node(&data) {
            Some(n) => n,
            None => {
                entries.push(WalkEntry {
                    path: path.to_string(),
                    hash: *hash,
                    link_type: LinkType::Blob,
                    size: data.len() as u64,
                    key: None, // TreeReader doesn't track keys
                });
                return Ok(());
            }
        };

        let node_size: u64 = node.links.iter().map(|l| l.size).sum();
        entries.push(WalkEntry {
            path: path.to_string(),
            hash: *hash,
            link_type: node.node_type,
            size: node_size,
            key: None, // directories are not encrypted
        });

        for link in &node.links {
            let child_path = match &link.name {
                Some(name) => {
                    // Skip internal chunk nodes in path
                    if name.starts_with("_chunk_") || name.starts_with('_') {
                        Box::pin(self.walk_recursive(&link.hash, path, entries)).await?;
                        continue;
                    }
                    if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{}/{}", path, name)
                    }
                }
                None => path.to_string(),
            };

            Box::pin(self.walk_recursive(&link.hash, &child_path, entries)).await?;
        }

        Ok(())
    }
}

/// Verify tree integrity
/// Checks that all referenced hashes exist
pub async fn verify_tree<S: Store>(store: Arc<S>, root_hash: &Hash) -> Result<VerifyResult, ReaderError> {
    let mut missing = Vec::new();
    let mut visited = std::collections::HashSet::new();

    verify_recursive(store, root_hash, &mut missing, &mut visited).await?;

    Ok(VerifyResult {
        valid: missing.is_empty(),
        missing,
    })
}

async fn verify_recursive<S: Store>(
    store: Arc<S>,
    hash: &Hash,
    missing: &mut Vec<Hash>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), ReaderError> {
    let hex = to_hex(hash);
    if visited.contains(&hex) {
        return Ok(());
    }
    visited.insert(hex);

    let data = match store.get(hash).await.map_err(|e| ReaderError::Store(e.to_string()))? {
        Some(d) => d,
        None => {
            missing.push(*hash);
            return Ok(());
        }
    };

    if is_tree_node(&data) {
        let node = decode_tree_node(&data).map_err(ReaderError::Codec)?;
        for link in &node.links {
            Box::pin(verify_recursive(store.clone(), &link.hash, missing, visited)).await?;
        }
    }

    Ok(())
}

/// Result of tree verification
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub valid: bool,
    pub missing: Vec<Hash>,
}

/// Reader error type
#[derive(Debug, thiserror::Error)]
pub enum ReaderError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("Codec error: {0}")]
    Codec(#[from] crate::codec::CodecError),
    #[error("Missing chunk: {0}")]
    MissingChunk(String),
    #[error("Decryption error: {0}")]
    Decryption(String),
    #[error("Missing decryption key")]
    MissingKey,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{BuilderConfig, TreeBuilder};
    use crate::store::MemoryStore;
    use crate::types::DirEntry;

    fn make_store() -> Arc<MemoryStore> {
        Arc::new(MemoryStore::new())
    }

    #[tokio::test]
    async fn test_get_blob() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let data = vec![1u8, 2, 3, 4, 5];
        let hash = builder.put_blob(&data).await.unwrap();

        let result = reader.get_blob(&hash).await.unwrap();
        assert_eq!(result, Some(data));
    }

    #[tokio::test]
    async fn test_get_blob_missing() {
        let store = make_store();
        let reader = TreeReader::new(store);

        let hash = [0u8; 32];
        let result = reader.get_blob(&hash).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_tree_node() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let file_hash = builder.put_blob(&[1u8]).await.unwrap();
        let dir_hash = builder
            .put_directory(vec![DirEntry::new("test.txt", file_hash).with_size(1)])
            .await
            .unwrap();

        let node = reader.get_tree_node(&dir_hash).await.unwrap();
        assert!(node.is_some());
        assert_eq!(node.unwrap().links.len(), 1);
    }

    #[tokio::test]
    async fn test_get_tree_node_returns_none_for_blob() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let hash = builder.put_blob(&[1u8, 2, 3]).await.unwrap();
        let node = reader.get_tree_node(&hash).await.unwrap();
        assert!(node.is_none());
    }

    #[tokio::test]
    async fn test_is_tree() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let file_hash = builder.put_blob(&[1u8]).await.unwrap();
        let dir_hash = builder
            .put_directory(vec![DirEntry::new("test.txt", file_hash)])
            .await
            .unwrap();

        assert!(reader.is_tree(&dir_hash).await.unwrap());
        assert!(!reader.is_tree(&file_hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_read_file_small() {
        let store = make_store();
        // Use public() for tests that check raw data storage
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()).public());
        let reader = TreeReader::new(store);

        let data = vec![1u8, 2, 3, 4, 5];
        let (cid, _size) = builder.put(&data).await.unwrap();

        let result = reader.read_file(&cid.hash).await.unwrap();
        assert_eq!(result, Some(data));
    }

    #[tokio::test]
    async fn test_read_file_chunked() {
        let store = make_store();
        let config = BuilderConfig::new(store.clone()).with_chunk_size(100).public();
        let builder = TreeBuilder::new(config);
        let reader = TreeReader::new(store);

        let mut data = vec![0u8; 350];
        for i in 0..data.len() {
            data[i] = (i % 256) as u8;
        }

        let (cid, _size) = builder.put(&data).await.unwrap();
        let result = reader.read_file(&cid.hash).await.unwrap();

        assert_eq!(result, Some(data));
    }

    #[tokio::test]
    async fn test_list_directory() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let h1 = builder.put_blob(&[1u8]).await.unwrap();
        let h2 = builder.put_blob(&[2u8]).await.unwrap();

        let dir_hash = builder
            .put_directory(
                vec![
                    DirEntry::new("first.txt", h1).with_size(1),
                    DirEntry::new("second.txt", h2).with_size(1),
                ],
            )
            .await
            .unwrap();

        let entries = reader.list_directory(&dir_hash).await.unwrap();

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "first.txt"));
        assert!(entries.iter().any(|e| e.name == "second.txt"));
    }

    #[tokio::test]
    async fn test_resolve_path() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let file_data = vec![1u8, 2, 3];
        let file_hash = builder.put_blob(&file_data).await.unwrap();

        let dir_hash = builder
            .put_directory(vec![DirEntry::new("test.txt", file_hash)])
            .await
            .unwrap();

        let resolved = reader.resolve_path(&dir_hash, "test.txt").await.unwrap();
        assert_eq!(resolved, Some(file_hash));
    }

    #[tokio::test]
    async fn test_resolve_path_nested() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let file_hash = builder.put_blob(&[1u8]).await.unwrap();

        let sub_sub_dir = builder
            .put_directory(vec![DirEntry::new("deep.txt", file_hash)])
            .await
            .unwrap();

        let sub_dir = builder
            .put_directory(vec![DirEntry::new("level2", sub_sub_dir)])
            .await
            .unwrap();

        let root_dir = builder
            .put_directory(vec![DirEntry::new("level1", sub_dir)])
            .await
            .unwrap();

        let resolved = reader
            .resolve_path(&root_dir, "level1/level2/deep.txt")
            .await
            .unwrap();
        assert_eq!(resolved, Some(file_hash));
    }

    #[tokio::test]
    async fn test_get_size() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let data = vec![0u8; 123];
        let hash = builder.put_blob(&data).await.unwrap();

        assert_eq!(reader.get_size(&hash).await.unwrap(), 123);
    }

    #[tokio::test]
    async fn test_walk() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()));
        let reader = TreeReader::new(store);

        let f1 = builder.put_blob(&[1u8]).await.unwrap();
        let f2 = builder.put_blob(&[2u8, 3]).await.unwrap();

        let sub_dir = builder
            .put_directory(vec![DirEntry::new("nested.txt", f2).with_size(2)])
            .await
            .unwrap();

        let root_dir = builder
            .put_directory(
                vec![
                    DirEntry::new("root.txt", f1).with_size(1),
                    DirEntry::new("sub", sub_dir),
                ],
            )
            .await
            .unwrap();

        let entries = reader.walk(&root_dir, "").await.unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();

        assert!(paths.contains(&""));
        assert!(paths.contains(&"root.txt"));
        assert!(paths.contains(&"sub"));
        assert!(paths.contains(&"sub/nested.txt"));
    }

    #[tokio::test]
    async fn test_verify_tree_valid() {
        let store = make_store();
        let config = BuilderConfig::new(store.clone()).with_chunk_size(100).public();
        let builder = TreeBuilder::new(config);

        let data = vec![0u8; 350];
        let (cid, _size) = builder.put(&data).await.unwrap();

        let result = verify_tree(store, &cid.hash).await.unwrap();
        assert!(result.valid);
        assert!(result.missing.is_empty());
    }

    #[tokio::test]
    async fn test_verify_tree_missing() {
        let store = make_store();
        let config = BuilderConfig::new(store.clone()).with_chunk_size(100).public();
        let builder = TreeBuilder::new(config);

        let data = vec![0u8; 350];
        let (cid, _size) = builder.put(&data).await.unwrap();

        // Delete one of the chunks
        let keys = store.keys();
        if let Some(chunk_to_delete) = keys.iter().find(|k| **k != cid.hash) {
            store.delete(chunk_to_delete).await.unwrap();
        }

        let result = verify_tree(store, &cid.hash).await.unwrap();
        assert!(!result.valid);
        assert!(!result.missing.is_empty());
    }

    #[tokio::test]
    async fn test_read_file_range_small_blob() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()).public());
        let reader = TreeReader::new(store);

        let data = b"Hello, World!";
        let hash = builder.put_blob(data).await.unwrap();

        // Read middle portion
        let result = reader.read_file_range(&hash, 7, Some(12)).await.unwrap();
        assert_eq!(result, Some(b"World".to_vec()));

        // Read from start
        let result = reader.read_file_range(&hash, 0, Some(5)).await.unwrap();
        assert_eq!(result, Some(b"Hello".to_vec()));

        // Read to end (no end specified)
        let result = reader.read_file_range(&hash, 7, None).await.unwrap();
        assert_eq!(result, Some(b"World!".to_vec()));
    }

    #[tokio::test]
    async fn test_read_file_range_chunked() {
        let store = make_store();
        // Small chunk size to force chunking
        let config = BuilderConfig::new(store.clone()).with_chunk_size(100).public();
        let builder = TreeBuilder::new(config);
        let reader = TreeReader::new(store);

        // Create 350 bytes of sequential data
        let mut data = vec![0u8; 350];
        for i in 0..data.len() {
            data[i] = (i % 256) as u8;
        }

        let (cid, _size) = builder.put(&data).await.unwrap();

        // Read bytes 50-150 (spans chunk boundary at 100)
        let result = reader.read_file_range(&cid.hash, 50, Some(150)).await.unwrap().unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(result, data[50..150].to_vec());

        // Read bytes 200-300 (within third and fourth chunks)
        let result = reader.read_file_range(&cid.hash, 200, Some(300)).await.unwrap().unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(result, data[200..300].to_vec());

        // Read last 50 bytes
        let result = reader.read_file_range(&cid.hash, 300, None).await.unwrap().unwrap();
        assert_eq!(result.len(), 50);
        assert_eq!(result, data[300..].to_vec());
    }

    #[tokio::test]
    async fn test_read_file_range_entire_file() {
        let store = make_store();
        let config = BuilderConfig::new(store.clone()).with_chunk_size(100).public();
        let builder = TreeBuilder::new(config);
        let reader = TreeReader::new(store);

        let mut data = vec![0u8; 350];
        for i in 0..data.len() {
            data[i] = (i % 256) as u8;
        }

        let (cid, _size) = builder.put(&data).await.unwrap();

        // Read entire file using range
        let result = reader.read_file_range(&cid.hash, 0, None).await.unwrap().unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_read_file_range_out_of_bounds() {
        let store = make_store();
        let builder = TreeBuilder::new(BuilderConfig::new(store.clone()).public());
        let reader = TreeReader::new(store);

        let data = b"Short";
        let hash = builder.put_blob(data).await.unwrap();

        // Start past end of file
        let result = reader.read_file_range(&hash, 100, Some(200)).await.unwrap();
        assert_eq!(result, Some(vec![]));

        // End past file length (should clamp)
        let result = reader.read_file_range(&hash, 0, Some(100)).await.unwrap();
        assert_eq!(result, Some(b"Short".to_vec()));
    }

    #[tokio::test]
    async fn test_read_file_range_single_byte() {
        let store = make_store();
        let config = BuilderConfig::new(store.clone()).with_chunk_size(100).public();
        let builder = TreeBuilder::new(config);
        let reader = TreeReader::new(store);

        let mut data = vec![0u8; 350];
        for i in 0..data.len() {
            data[i] = (i % 256) as u8;
        }

        let (cid, _size) = builder.put(&data).await.unwrap();

        // Read single byte at chunk boundary
        let result = reader.read_file_range(&cid.hash, 100, Some(101)).await.unwrap().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], 100);
    }
}
