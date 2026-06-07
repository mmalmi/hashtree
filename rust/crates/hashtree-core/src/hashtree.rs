//! HashTree - Unified merkle tree operations
//!
//! Single struct for creating, reading, and editing content-addressed merkle trees.
//! Mirrors the hashtree-ts HashTree class API.

use std::pin::Pin;
use std::sync::Arc;

use futures::io::AsyncRead;
use futures::stream::{self, Stream};
use futures::AsyncReadExt;

use crate::builder::{BuilderError, DEFAULT_CHUNK_SIZE, DEFAULT_MAX_LINKS};
use crate::codec::{
    decode_tree_node, encode_and_hash, is_directory_node, is_tree_node, try_decode_tree_node,
};
use crate::hash::sha256;
use crate::reader::{ReaderError, TreeEntry, WalkEntry};
use crate::store::Store;
use crate::types::{to_hex, Cid, DirEntry, Hash, Link, LinkType, TreeNode};

use crate::crypto::{decrypt_chk, encrypt_chk, EncryptionKey};

#[path = "hashtree/stream.rs"]
mod read_stream;
mod walk;

/// HashTree configuration
#[derive(Clone)]
pub struct HashTreeConfig<S: Store> {
    pub store: Arc<S>,
    pub chunk_size: usize,
    pub max_links: usize,
    /// Whether to encrypt content (default: true when encryption feature enabled)
    pub encrypted: bool,
}

impl<S: Store> HashTreeConfig<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_links: DEFAULT_MAX_LINKS,
            encrypted: true,
        }
    }

    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    pub fn with_max_links(mut self, max_links: usize) -> Self {
        self.max_links = max_links;
        self
    }

    /// Disable encryption (store content publicly)
    pub fn public(mut self) -> Self {
        self.encrypted = false;
        self
    }
}

/// HashTree error type
#[derive(Debug, thiserror::Error)]
pub enum HashTreeError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("Codec error: {0}")]
    Codec(#[from] crate::codec::CodecError),
    #[error("Missing chunk: {0}")]
    MissingChunk(String),
    #[error("Path not found: {0}")]
    PathNotFound(String),
    #[error("Entry not found: {0}")]
    EntryNotFound(String),
    #[error("Encryption error: {0}")]
    Encryption(String),
    #[error("Decryption error: {0}")]
    Decryption(String),
    #[error("Content size {actual_size} exceeds max_size {max_size}")]
    SizeLimitExceeded { max_size: u64, actual_size: u64 },
}

impl From<BuilderError> for HashTreeError {
    fn from(e: BuilderError) -> Self {
        match e {
            BuilderError::Store(s) => HashTreeError::Store(s),
            BuilderError::Codec(c) => HashTreeError::Codec(c),
            BuilderError::Encryption(s) => HashTreeError::Encryption(s),
        }
    }
}

impl From<ReaderError> for HashTreeError {
    fn from(e: ReaderError) -> Self {
        match e {
            ReaderError::Store(s) => HashTreeError::Store(s),
            ReaderError::Codec(c) => HashTreeError::Codec(c),
            ReaderError::MissingChunk(s) => HashTreeError::MissingChunk(s),
            ReaderError::Decryption(s) => HashTreeError::Encryption(s),
            ReaderError::MissingKey => {
                HashTreeError::Encryption("missing decryption key".to_string())
            }
        }
    }
}

/// HashTree - unified create, read, and edit merkle tree operations
pub struct HashTree<S: Store> {
    store: Arc<S>,
    chunk_size: usize,
    max_links: usize,
    encrypted: bool,
}

impl<S: Store> HashTree<S> {
    fn internal_chunk_start(name: &str) -> Option<usize> {
        let suffix = name.strip_prefix("_chunk_")?;
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        suffix.parse().ok()
    }

    fn node_uses_directory_fanout(node: &TreeNode) -> bool {
        !node.links.is_empty()
            && node.links.iter().all(|link| {
                let Some(name) = link.name.as_deref() else {
                    return false;
                };
                Self::internal_chunk_start(name).is_some() && link.link_type == LinkType::Dir
            })
    }

    fn is_internal_directory_link_with_fanout(link: &Link, uses_fanout: bool) -> bool {
        if !uses_fanout || link.link_type != LinkType::Dir {
            return false;
        }

        let Some(name) = link.name.as_deref() else {
            return false;
        };
        Self::internal_chunk_start(name).is_some()
    }

    fn is_internal_directory_link(node: &TreeNode, link: &Link) -> bool {
        Self::is_internal_directory_link_with_fanout(link, Self::node_uses_directory_fanout(node))
    }

    pub fn new(config: HashTreeConfig<S>) -> Self {
        Self {
            store: config.store,
            chunk_size: config.chunk_size,
            max_links: config.max_links,
            encrypted: config.encrypted,
        }
    }

    /// Check if encryption is enabled
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    // ============ UNIFIED API ============

    /// Store content, returns (Cid, size) where Cid is hash + optional key
    /// Encrypts by default when encryption feature is enabled
    pub async fn put(&self, data: &[u8]) -> Result<(Cid, u64), HashTreeError> {
        let size = data.len() as u64;

        // Small data - store as single chunk
        if data.len() <= self.chunk_size {
            let (hash, key) = self.put_chunk_internal(data).await?;
            return Ok((Cid { hash, key }, size));
        }

        // Large data - chunk it
        let mut links: Vec<Link> = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let end = (offset + self.chunk_size).min(data.len());
            let chunk = &data[offset..end];
            let chunk_size = chunk.len() as u64;
            let (hash, key) = self.put_chunk_internal(chunk).await?;
            links.push(Link {
                hash,
                name: None,
                size: chunk_size,
                key,
                link_type: LinkType::Blob, // Leaf chunk (raw blob)
                meta: None,
            });
            offset = end;
        }

        // Build tree from chunks
        let (root_hash, root_key) = self.build_tree_internal(links, Some(size)).await?;
        Ok((
            Cid {
                hash: root_hash,
                key: root_key,
            },
            size,
        ))
    }

    /// Get content by Cid (handles decryption automatically)
    ///
    /// - `max_size`: Optional max plaintext size in bytes. If exceeded, returns
    ///   `HashTreeError::SizeLimitExceeded`.
    pub async fn get(
        &self,
        cid: &Cid,
        max_size: Option<u64>,
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        if let Some(key) = cid.key {
            self.get_encrypted(&cid.hash, &key, max_size).await
        } else {
            self.read_file_with_limit(&cid.hash, max_size).await
        }
    }

    /// Store content from an async reader (streaming put)
    ///
    /// Reads data in chunks and builds a merkle tree incrementally.
    /// Useful for large files or streaming data sources.
    /// Returns (Cid, size) where Cid is hash + optional key
    pub async fn put_stream<R: AsyncRead + Unpin>(
        &self,
        mut reader: R,
    ) -> Result<(Cid, u64), HashTreeError> {
        let mut buffer = vec![0u8; self.chunk_size];
        let mut links = Vec::new();
        let mut total_size: u64 = 0;
        let mut consistent_key: Option<[u8; 32]> = None;

        loop {
            let mut chunk = Vec::new();
            let mut bytes_read = 0;

            // Read until we have a full chunk or EOF
            while bytes_read < self.chunk_size {
                let n = reader
                    .read(&mut buffer[..self.chunk_size - bytes_read])
                    .await
                    .map_err(|e| HashTreeError::Store(format!("read error: {}", e)))?;
                if n == 0 {
                    break; // EOF
                }
                chunk.extend_from_slice(&buffer[..n]);
                bytes_read += n;
            }

            if chunk.is_empty() {
                break; // No more data
            }

            let chunk_len = chunk.len() as u64;
            total_size += chunk_len;

            let (hash, key) = self.put_chunk_internal(&chunk).await?;

            // Track consistent key for single-key result
            if links.is_empty() {
                consistent_key = key;
            } else if consistent_key != key {
                consistent_key = None;
            }

            links.push(Link {
                hash,
                name: None,
                size: chunk_len,
                key,
                link_type: LinkType::Blob, // Leaf chunk (raw blob)
                meta: None,
            });
        }

        if links.is_empty() {
            // Empty input
            let (hash, key) = self.put_chunk_internal(&[]).await?;
            return Ok((Cid { hash, key }, 0));
        }

        // Build tree from chunks
        let (root_hash, root_key) = self.build_tree_internal(links, Some(total_size)).await?;
        Ok((
            Cid {
                hash: root_hash,
                key: root_key,
            },
            total_size,
        ))
    }

    /// Store a chunk with optional encryption
    async fn put_chunk_internal(
        &self,
        data: &[u8],
    ) -> Result<(Hash, Option<EncryptionKey>), HashTreeError> {
        if self.encrypted {
            let (encrypted, key) =
                encrypt_chk(data).map_err(|e| HashTreeError::Encryption(e.to_string()))?;
            let hash = sha256(&encrypted);
            self.store
                .put(hash, encrypted)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?;
            Ok((hash, Some(key)))
        } else {
            let hash = self.put_blob(data).await?;
            Ok((hash, None))
        }
    }

    /// Build tree and return (hash, optional_key)
    async fn build_tree_internal(
        &self,
        links: Vec<Link>,
        total_size: Option<u64>,
    ) -> Result<(Hash, Option<[u8; 32]>), HashTreeError> {
        // Single link with matching size - return directly
        if links.len() == 1 {
            if let Some(ts) = total_size {
                if links[0].size == ts {
                    return Ok((links[0].hash, links[0].key));
                }
            }
        }

        if links.len() <= self.max_links {
            let node = TreeNode {
                node_type: LinkType::File,
                links,
            };
            let (data, _) = encode_and_hash(&node)?;

            if self.encrypted {
                let (encrypted, key) =
                    encrypt_chk(&data).map_err(|e| HashTreeError::Encryption(e.to_string()))?;
                let hash = sha256(&encrypted);
                self.store
                    .put(hash, encrypted)
                    .await
                    .map_err(|e| HashTreeError::Store(e.to_string()))?;
                return Ok((hash, Some(key)));
            }

            // Unencrypted path
            let hash = sha256(&data);
            self.store
                .put(hash, data)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?;
            return Ok((hash, None));
        }

        // Too many links - create subtrees
        let mut sub_links = Vec::new();
        for batch in links.chunks(self.max_links) {
            let batch_size: u64 = batch.iter().map(|l| l.size).sum();
            let (hash, key) =
                Box::pin(self.build_tree_internal(batch.to_vec(), Some(batch_size))).await?;
            sub_links.push(Link {
                hash,
                name: None,
                size: batch_size,
                key,
                link_type: LinkType::File, // Internal tree node
                meta: None,
            });
        }

        Box::pin(self.build_tree_internal(sub_links, total_size)).await
    }

    /// Get encrypted content by hash and key
    async fn get_encrypted(
        &self,
        hash: &Hash,
        key: &EncryptionKey,
        max_size: Option<u64>,
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        let decrypted = match self.get_encrypted_root(hash, key).await? {
            Some(data) => data,
            None => return Ok(None),
        };

        // Check if it's a tree node
        if is_tree_node(&decrypted) {
            let node = decode_tree_node(&decrypted)?;
            let declared_size: u64 = node.links.iter().map(|l| l.size).sum();
            Self::ensure_size_limit(max_size, declared_size)?;

            let mut bytes_read = 0u64;
            let assembled = self
                .assemble_encrypted_chunks_limited(&node, max_size, &mut bytes_read)
                .await?;
            return Ok(Some(assembled));
        }

        // Single chunk data
        Self::ensure_size_limit(max_size, decrypted.len() as u64)?;
        Ok(Some(decrypted))
    }

    async fn get_encrypted_root(
        &self,
        hash: &Hash,
        key: &EncryptionKey,
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        self.get_cid_root_bytes(&Cid {
            hash: *hash,
            key: Some(*key),
        })
        .await
        .map_err(|err| match err {
            HashTreeError::Decryption(message) => HashTreeError::Encryption(message),
            other => other,
        })
    }

    fn ensure_size_limit(max_size: Option<u64>, actual_size: u64) -> Result<(), HashTreeError> {
        if let Some(max_size) = max_size {
            if actual_size > max_size {
                return Err(HashTreeError::SizeLimitExceeded {
                    max_size,
                    actual_size,
                });
            }
        }
        Ok(())
    }

    /// Assemble encrypted chunks from tree
    async fn assemble_encrypted_chunks_limited(
        &self,
        node: &TreeNode,
        max_size: Option<u64>,
        bytes_read: &mut u64,
    ) -> Result<Vec<u8>, HashTreeError> {
        let mut parts: Vec<Vec<u8>> = Vec::new();

        for link in &node.links {
            let projected = (*bytes_read).saturating_add(link.size);
            Self::ensure_size_limit(max_size, projected)?;

            let chunk_key = link
                .key
                .ok_or_else(|| HashTreeError::Encryption("missing chunk key".to_string()))?;

            let encrypted_child = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?
                .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;

            let decrypted = decrypt_chk(&encrypted_child, &chunk_key)
                .map_err(|e| HashTreeError::Encryption(e.to_string()))?;

            if is_tree_node(&decrypted) {
                // Intermediate tree node - recurse
                let child_node = decode_tree_node(&decrypted)?;
                let child_data = Box::pin(self.assemble_encrypted_chunks_limited(
                    &child_node,
                    max_size,
                    bytes_read,
                ))
                .await?;
                parts.push(child_data);
            } else {
                // Leaf data chunk
                let projected = (*bytes_read).saturating_add(decrypted.len() as u64);
                Self::ensure_size_limit(max_size, projected)?;
                *bytes_read = projected;
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

    // ============ LOW-LEVEL CREATE ============

    /// Store a blob directly (small data, no encryption)
    /// Returns the content hash
    pub async fn put_blob(&self, data: &[u8]) -> Result<Hash, HashTreeError> {
        let hash = sha256(data);
        self.store
            .put(hash, data.to_vec())
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?;
        Ok(hash)
    }

    /// Store a file, chunking if necessary
    /// Returns (Cid, size) where Cid is hash + optional key
    pub async fn put_file(&self, data: &[u8]) -> Result<(Cid, u64), HashTreeError> {
        let size = data.len() as u64;

        // Small file - store as single chunk
        if data.len() <= self.chunk_size {
            let (hash, key) = self.put_chunk_internal(data).await?;
            return Ok((Cid { hash, key }, size));
        }

        // Large file - chunk it
        let mut links: Vec<Link> = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let end = (offset + self.chunk_size).min(data.len());
            let chunk = &data[offset..end];
            let chunk_size = (end - offset) as u64;

            let (hash, key) = self.put_chunk_internal(chunk).await?;
            links.push(Link {
                hash,
                name: None,
                size: chunk_size,
                key,
                link_type: LinkType::Blob, // Leaf chunk
                meta: None,
            });
            offset = end;
        }

        // Build tree from chunks (uses encryption if enabled)
        let (root_hash, root_key) = self.build_tree_internal(links, Some(size)).await?;
        Ok((
            Cid {
                hash: root_hash,
                key: root_key,
            },
            size,
        ))
    }

    /// Build a directory from entries
    /// Returns Cid with key if encrypted
    ///
    /// Large directories are split into BUD-17 `_chunk_<start>` fanout nodes.
    pub async fn put_directory(&self, entries: Vec<DirEntry>) -> Result<Cid, HashTreeError> {
        // Sort entries by name for deterministic hashing
        let mut sorted = entries;
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        let links: Vec<Link> = sorted
            .into_iter()
            .map(|e| Link {
                hash: e.hash,
                name: Some(e.name),
                size: e.size,
                key: e.key,
                link_type: e.link_type,
                meta: e.meta,
            })
            .collect();

        if links.len() <= self.max_links {
            return self.put_directory_node(links).await;
        }

        self.build_directory_by_chunks(links).await
    }

    async fn put_directory_node(&self, links: Vec<Link>) -> Result<Cid, HashTreeError> {
        let node = TreeNode {
            node_type: LinkType::Dir,
            links,
        };
        let (data, plain_hash) = encode_and_hash(&node)?;

        if self.encrypted {
            let (encrypted, key) =
                encrypt_chk(&data).map_err(|e| HashTreeError::Encryption(e.to_string()))?;
            let hash = sha256(&encrypted);
            self.store
                .put(hash, encrypted)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?;
            return Ok(Cid {
                hash,
                key: Some(key),
            });
        }

        self.store
            .put(plain_hash, data)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?;
        Ok(Cid {
            hash: plain_hash,
            key: None,
        })
    }

    async fn build_directory_by_chunks(&self, links: Vec<Link>) -> Result<Cid, HashTreeError> {
        let indexed_links: Vec<(usize, Link)> = links.into_iter().enumerate().collect();
        self.build_indexed_directory_chunks(indexed_links).await
    }

    async fn build_indexed_directory_chunks(
        &self,
        links: Vec<(usize, Link)>,
    ) -> Result<Cid, HashTreeError> {
        let mut sub_trees: Vec<(usize, Link)> = Vec::new();

        for (i, batch) in links.chunks(self.max_links).enumerate() {
            let start = batch
                .first()
                .map(|(start, _)| *start)
                .unwrap_or(i * self.max_links);
            let batch_size: u64 = batch.iter().map(|(_, link)| link.size).sum();
            let child_links = batch.iter().map(|(_, link)| link.clone()).collect();
            let child_cid = self.put_directory_node(child_links).await?;

            sub_trees.push((
                start,
                Link {
                    hash: child_cid.hash,
                    name: Some(format!("_chunk_{start}")),
                    size: batch_size,
                    key: child_cid.key,
                    link_type: LinkType::Dir,
                    meta: None,
                },
            ));
        }

        if sub_trees.len() <= self.max_links {
            return self
                .put_directory_node(sub_trees.into_iter().map(|(_, link)| link).collect())
                .await;
        }

        Box::pin(self.build_indexed_directory_chunks(sub_trees)).await
    }

    /// Create a tree node with custom links
    pub async fn put_tree_node(&self, links: Vec<Link>) -> Result<Hash, HashTreeError> {
        let node = TreeNode {
            node_type: LinkType::Dir,
            links,
        };

        let (data, hash) = encode_and_hash(&node)?;
        self.store
            .put(hash, data)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?;
        Ok(hash)
    }

    // ============ READ ============

    /// Get raw data by hash
    pub async fn get_blob(&self, hash: &Hash) -> Result<Option<Vec<u8>>, HashTreeError> {
        self.store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))
    }

    async fn has_stored_chunk(&self, hash: &Hash) -> Result<bool, HashTreeError> {
        self.store
            .get(hash)
            .await
            .map(|data| data.is_some())
            .map_err(|e| HashTreeError::Store(e.to_string()))
    }

    /// Get and decode a tree node (unencrypted)
    pub async fn get_tree_node(&self, hash: &Hash) -> Result<Option<TreeNode>, HashTreeError> {
        let data = match self
            .store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(None),
        };

        if !is_tree_node(&data) {
            return Ok(None);
        }

        let node = decode_tree_node(&data)?;
        Ok(Some(node))
    }

    async fn get_cid_root_bytes(&self, cid: &Cid) -> Result<Option<Vec<u8>>, HashTreeError> {
        let data = match self
            .store
            .get(&cid.hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(None),
        };

        let Some(key) = &cid.key else {
            return Ok(Some(data));
        };

        let raw_is_tree = is_tree_node(&data);
        match decrypt_chk(&data, key) {
            Ok(decrypted) => {
                if is_tree_node(&decrypted) || !raw_is_tree {
                    Ok(Some(decrypted))
                } else {
                    Ok(Some(data))
                }
            }
            Err(err) => {
                if raw_is_tree {
                    Ok(Some(data))
                } else {
                    Err(HashTreeError::Decryption(err.to_string()))
                }
            }
        }
    }

    /// Get and decode a tree node using Cid (with decryption if key present)
    pub async fn get_node(&self, cid: &Cid) -> Result<Option<TreeNode>, HashTreeError> {
        let decrypted = match self.get_cid_root_bytes(cid).await? {
            Some(d) => d,
            None => return Ok(None),
        };

        if !is_tree_node(&decrypted) {
            return Ok(None);
        }

        let node = decode_tree_node(&decrypted)?;
        Ok(Some(node))
    }

    /// Get directory node, handling historical byte-chunked directory data.
    /// Use this when you know the target is a directory (from parent link_type)
    pub async fn get_directory_node(&self, cid: &Cid) -> Result<Option<TreeNode>, HashTreeError> {
        let decrypted = match self.get_cid_root_bytes(cid).await? {
            Some(d) => d,
            None => return Ok(None),
        };

        if !is_tree_node(&decrypted) {
            return Ok(None);
        }

        let node = decode_tree_node(&decrypted)?;

        // If this is a file tree (chunked data), reassemble to get actual directory
        if node.node_type == LinkType::File {
            let mut bytes_read = 0u64;
            let assembled = self
                .assemble_chunks_limited(&node, None, &mut bytes_read)
                .await?;
            if is_tree_node(&assembled) {
                let inner_node = decode_tree_node(&assembled)?;
                return Ok(Some(inner_node));
            }
        }

        Ok(Some(node))
    }

    /// Check if hash points to a tree node (no decryption)
    pub async fn is_tree(&self, hash: &Hash) -> Result<bool, HashTreeError> {
        let data = match self
            .store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(false),
        };
        Ok(is_tree_node(&data))
    }

    /// Check if Cid points to a directory (with decryption)
    pub async fn is_dir(&self, cid: &Cid) -> Result<bool, HashTreeError> {
        Ok(matches!(
            self.get_directory_node(cid).await?,
            Some(node) if node.node_type == LinkType::Dir
        ))
    }

    /// Check if hash points to a directory (tree with named links, no decryption)
    pub async fn is_directory(&self, hash: &Hash) -> Result<bool, HashTreeError> {
        let data = match self
            .store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(false),
        };
        Ok(is_directory_node(&data))
    }

    /// Read a complete file (reassemble chunks if needed)
    pub async fn read_file(&self, hash: &Hash) -> Result<Option<Vec<u8>>, HashTreeError> {
        self.read_file_with_limit(hash, None).await
    }

    /// Read a complete file with optional size limit.
    async fn read_file_with_limit(
        &self,
        hash: &Hash,
        max_size: Option<u64>,
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        let data = match self
            .store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(None),
        };

        // Check if it's a tree (chunked file) or raw blob
        if !is_tree_node(&data) {
            Self::ensure_size_limit(max_size, data.len() as u64)?;
            return Ok(Some(data));
        }

        // It's a tree - reassemble chunks
        let node = decode_tree_node(&data)?;
        let declared_size: u64 = node.links.iter().map(|l| l.size).sum();
        Self::ensure_size_limit(max_size, declared_size)?;

        let mut bytes_read = 0u64;
        let assembled = self
            .assemble_chunks_limited(&node, max_size, &mut bytes_read)
            .await?;
        Ok(Some(assembled))
    }

    /// Read a byte range from a file (fetches only necessary chunks)
    ///
    /// - `start`: Starting byte offset (inclusive)
    /// - `end`: Ending byte offset (exclusive), or None to read to end
    ///
    /// This is more efficient than read_file() for partial reads of large files.
    pub async fn read_file_range(
        &self,
        hash: &Hash,
        start: u64,
        end: Option<u64>,
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        let data = match self
            .store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
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
        let node = decode_tree_node(&data)?;
        let range_data = self.assemble_chunks_range(&node, start, end).await?;
        Ok(Some(range_data))
    }

    /// Read a byte range from a file using a Cid (handles decryption if key present)
    pub async fn read_file_range_cid(
        &self,
        cid: &Cid,
        start: u64,
        end: Option<u64>,
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        if let Some(key) = cid.key {
            let data = match self.get_encrypted_root(&cid.hash, &key).await? {
                Some(d) => d,
                None => return Ok(None),
            };

            if is_tree_node(&data) {
                let node = decode_tree_node(&data)?;
                let total_size: u64 = node.links.iter().map(|link| link.size).sum();
                let actual_end = end.unwrap_or(total_size).min(total_size);
                if start >= actual_end {
                    return Ok(Some(vec![]));
                }

                let mut result = Vec::with_capacity((actual_end - start) as usize);
                self.append_encrypted_range(&node, start, actual_end, 0, &mut result)
                    .await?;
                return Ok(Some(result));
            }

            let start_idx = start as usize;
            let end_idx = end.map(|e| e as usize).unwrap_or(data.len());
            if start_idx >= data.len() {
                return Ok(Some(vec![]));
            }
            let end_idx = end_idx.min(data.len());
            return Ok(Some(data[start_idx..end_idx].to_vec()));
        }

        self.read_file_range(&cid.hash, start, end).await
    }

    async fn append_encrypted_range(
        &self,
        node: &TreeNode,
        start: u64,
        end: u64,
        base_offset: u64,
        result: &mut Vec<u8>,
    ) -> Result<(), HashTreeError> {
        let mut current_offset = base_offset;

        for link in &node.links {
            let child_start = current_offset;
            let child_end = child_start.saturating_add(link.size);
            current_offset = child_end;

            if child_end <= start {
                continue;
            }
            if child_start >= end {
                break;
            }

            let chunk_key = link
                .key
                .ok_or_else(|| HashTreeError::Encryption("missing chunk key".to_string()))?;

            let encrypted_child = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?
                .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;
            let decrypted_child = decrypt_chk(&encrypted_child, &chunk_key)
                .map_err(|e| HashTreeError::Encryption(e.to_string()))?;

            if is_tree_node(&decrypted_child) {
                let child_node = decode_tree_node(&decrypted_child)?;
                Box::pin(self.append_encrypted_range(&child_node, start, end, child_start, result))
                    .await?;
                continue;
            }

            let slice_start = if start > child_start {
                (start - child_start) as usize
            } else {
                0
            };
            let slice_end = if end < child_end {
                (end - child_start) as usize
            } else {
                decrypted_child.len()
            };
            result.extend_from_slice(&decrypted_child[slice_start..slice_end]);
        }

        Ok(())
    }

    /// Assemble only the chunks needed for a byte range
    async fn assemble_chunks_range(
        &self,
        node: &TreeNode,
        start: u64,
        end: Option<u64>,
    ) -> Result<Vec<u8>, HashTreeError> {
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
                    .map_err(|e| HashTreeError::Store(e.to_string()))?
                    .ok_or_else(|| HashTreeError::MissingChunk(to_hex(chunk_hash)))?;

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
    ) -> Result<Vec<(Hash, u64, u64)>, HashTreeError> {
        let mut chunks = Vec::new();
        let mut offset = 0u64;
        self.collect_chunk_offsets_recursive(node, &mut chunks, &mut offset)
            .await?;
        Ok(chunks)
    }

    async fn collect_chunk_offsets_recursive(
        &self,
        node: &TreeNode,
        chunks: &mut Vec<(Hash, u64, u64)>,
        offset: &mut u64,
    ) -> Result<(), HashTreeError> {
        for link in &node.links {
            let child_data = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?
                .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;

            if is_tree_node(&child_data) {
                // Intermediate node - recurse
                let child_node = decode_tree_node(&child_data)?;
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

    /// Recursively assemble chunks from tree
    async fn assemble_chunks_limited(
        &self,
        node: &TreeNode,
        max_size: Option<u64>,
        bytes_read: &mut u64,
    ) -> Result<Vec<u8>, HashTreeError> {
        let mut parts: Vec<Vec<u8>> = Vec::new();

        for link in &node.links {
            let projected = (*bytes_read).saturating_add(link.size);
            Self::ensure_size_limit(max_size, projected)?;

            let child_data = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?
                .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;

            if is_tree_node(&child_data) {
                let child_node = decode_tree_node(&child_data)?;
                parts.push(
                    Box::pin(self.assemble_chunks_limited(&child_node, max_size, bytes_read))
                        .await?,
                );
            } else {
                let projected = (*bytes_read).saturating_add(child_data.len() as u64);
                Self::ensure_size_limit(max_size, projected)?;
                *bytes_read = projected;
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

    /// Read file chunks as Vec (non-streaming version)
    pub async fn read_file_chunks(&self, hash: &Hash) -> Result<Vec<Vec<u8>>, HashTreeError> {
        let data = match self
            .store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(vec![]),
        };

        if !is_tree_node(&data) {
            return Ok(vec![data]);
        }

        let node = decode_tree_node(&data)?;
        self.collect_chunks(&node).await
    }

    async fn collect_chunks(&self, node: &TreeNode) -> Result<Vec<Vec<u8>>, HashTreeError> {
        let mut chunks = Vec::new();

        for link in &node.links {
            let child_data = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?
                .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;

            if is_tree_node(&child_data) {
                let child_node = decode_tree_node(&child_data)?;
                chunks.extend(Box::pin(self.collect_chunks(&child_node)).await?);
            } else {
                chunks.push(child_data);
            }
        }

        Ok(chunks)
    }

    /// List directory entries (Cid-based, supports encrypted directories)
    pub async fn list(&self, cid: &Cid) -> Result<Vec<TreeEntry>, HashTreeError> {
        let node = match self.get_node(cid).await? {
            Some(n) => n,
            None => return Ok(vec![]),
        };

        let mut entries = Vec::new();

        for link in &node.links {
            // Skip internal chunk nodes - recurse into them
            if Self::is_internal_directory_link(&node, link) {
                let chunk_cid = Cid {
                    hash: link.hash,
                    key: link.key,
                };
                let sub_entries = Box::pin(self.list(&chunk_cid)).await?;
                entries.extend(sub_entries);
                continue;
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

    /// List directory entries using Cid (with decryption if key present).
    /// Handles both regular and fanout directory data.
    pub async fn list_directory(&self, cid: &Cid) -> Result<Vec<TreeEntry>, HashTreeError> {
        // Use get_directory_node which handles chunked directory data
        let node = match self.get_directory_node(cid).await? {
            Some(n) => n,
            None => return Ok(vec![]),
        };

        let mut entries = Vec::new();

        for link in &node.links {
            // Skip internal fanout nodes.
            if Self::is_internal_directory_link(&node, link) {
                let sub_cid = Cid {
                    hash: link.hash,
                    key: link.key,
                };
                let sub_entries = Box::pin(self.list_directory(&sub_cid)).await?;
                entries.extend(sub_entries);
                continue;
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

    /// Resolve a path within a tree (returns Cid with key if encrypted)
    pub async fn resolve(&self, cid: &Cid, path: &str) -> Result<Option<Cid>, HashTreeError> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Ok(Some(cid.clone()));
        }

        let mut current_cid = cid.clone();

        for part in parts {
            // Use get_directory_node which handles chunked directory data
            let node = match self.get_directory_node(&current_cid).await? {
                Some(n) => n,
                None => {
                    if !self.has_stored_chunk(&current_cid.hash).await? {
                        return Err(HashTreeError::MissingChunk(to_hex(&current_cid.hash)));
                    }
                    return Ok(None);
                }
            };

            if let Some(link) = self.find_link(&node, part) {
                current_cid = Cid {
                    hash: link.hash,
                    key: link.key,
                };
            } else {
                // Check internal nodes
                match self
                    .find_link_in_subtrees_cid(&node, part, &current_cid)
                    .await?
                {
                    Some(link) => {
                        current_cid = Cid {
                            hash: link.hash,
                            key: link.key,
                        };
                    }
                    None => return Ok(None),
                }
            }
        }

        Ok(Some(current_cid))
    }

    /// Resolve a path within a tree using Cid (with decryption if key present)
    pub async fn resolve_path(&self, cid: &Cid, path: &str) -> Result<Option<Cid>, HashTreeError> {
        self.resolve(cid, path).await
    }

    fn find_link(&self, node: &TreeNode, name: &str) -> Option<Link> {
        node.links
            .iter()
            .find(|l| l.name.as_deref() == Some(name))
            .cloned()
    }

    /// Find a link in subtrees using Cid (with decryption support)
    async fn find_link_in_subtrees_cid(
        &self,
        node: &TreeNode,
        name: &str,
        _parent_cid: &Cid,
    ) -> Result<Option<Link>, HashTreeError> {
        for link in &node.links {
            if !Self::is_internal_directory_link(node, link) {
                continue;
            }

            // Internal nodes inherit encryption from parent context
            let sub_cid = Cid {
                hash: link.hash,
                key: link.key,
            };

            let sub_node = match self.get_node(&sub_cid).await? {
                Some(n) => n,
                None => {
                    if !self.has_stored_chunk(&sub_cid.hash).await? {
                        return Err(HashTreeError::MissingChunk(to_hex(&sub_cid.hash)));
                    }
                    continue;
                }
            };

            if let Some(found) = self.find_link(&sub_node, name) {
                return Ok(Some(found));
            }

            if let Some(deep_found) =
                Box::pin(self.find_link_in_subtrees_cid(&sub_node, name, &sub_cid)).await?
            {
                return Ok(Some(deep_found));
            }
        }

        Ok(None)
    }

    /// Get total size of a tree
    pub async fn get_size(&self, hash: &Hash) -> Result<u64, HashTreeError> {
        let data = match self
            .store
            .get(hash)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?
        {
            Some(d) => d,
            None => return Ok(0),
        };

        if !is_tree_node(&data) {
            return Ok(data.len() as u64);
        }

        let node = decode_tree_node(&data)?;
        // Calculate from children
        let mut total = 0u64;
        for link in &node.links {
            total += link.size;
        }
        Ok(total)
    }

    /// Get total size using a Cid (handles decryption if key present)
    pub async fn get_size_cid(&self, cid: &Cid) -> Result<u64, HashTreeError> {
        if let Some(key) = cid.key {
            let data = match self.get_encrypted_root(&cid.hash, &key).await? {
                Some(d) => d,
                None => return Ok(0),
            };
            if is_tree_node(&data) {
                let node = decode_tree_node(&data)?;
                return Ok(node.links.iter().map(|link| link.size).sum());
            }
            return Ok(data.len() as u64);
        }

        self.get_size(&cid.hash).await
    }

    // ============ EDIT ============

    /// Add or update an entry in a directory
    /// Returns new root Cid (immutable operation)
    pub async fn set_entry(
        &self,
        root: &Cid,
        path: &[&str],
        name: &str,
        entry_cid: &Cid,
        size: u64,
        link_type: LinkType,
    ) -> Result<Cid, HashTreeError> {
        self.set_entry_with_meta(root, path, name, entry_cid, size, link_type, None)
            .await
    }

    /// Add or update an entry in a directory with optional link metadata.
    /// Returns new root Cid (immutable operation)
    pub async fn set_entry_with_meta(
        &self,
        root: &Cid,
        path: &[&str],
        name: &str,
        entry_cid: &Cid,
        size: u64,
        link_type: LinkType,
        meta: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Result<Cid, HashTreeError> {
        let dir_cid = self.resolve_path_array(root, path).await?;
        let dir_cid = dir_cid.ok_or_else(|| HashTreeError::PathNotFound(path.join("/")))?;

        let entries = self.list_directory(&dir_cid).await?;
        let mut new_entries: Vec<DirEntry> = entries
            .into_iter()
            .filter(|e| e.name != name)
            .map(|e| DirEntry {
                name: e.name,
                hash: e.hash,
                size: e.size,
                key: e.key,
                link_type: e.link_type,
                meta: e.meta,
            })
            .collect();

        new_entries.push(DirEntry {
            name: name.to_string(),
            hash: entry_cid.hash,
            size,
            key: entry_cid.key,
            link_type,
            meta,
        });

        let new_dir_cid = self.put_directory(new_entries).await?;
        self.rebuild_path(root, path, new_dir_cid).await
    }

    /// Remove an entry from a directory
    /// Returns new root Cid
    pub async fn remove_entry(
        &self,
        root: &Cid,
        path: &[&str],
        name: &str,
    ) -> Result<Cid, HashTreeError> {
        let dir_cid = self.resolve_path_array(root, path).await?;
        let dir_cid = dir_cid.ok_or_else(|| HashTreeError::PathNotFound(path.join("/")))?;

        let entries = self.list_directory(&dir_cid).await?;
        let new_entries: Vec<DirEntry> = entries
            .into_iter()
            .filter(|e| e.name != name)
            .map(|e| DirEntry {
                name: e.name,
                hash: e.hash,
                size: e.size,
                key: e.key,
                link_type: e.link_type,
                meta: e.meta,
            })
            .collect();

        let new_dir_cid = self.put_directory(new_entries).await?;
        self.rebuild_path(root, path, new_dir_cid).await
    }

    /// Rename an entry in a directory
    /// Returns new root Cid
    pub async fn rename_entry(
        &self,
        root: &Cid,
        path: &[&str],
        old_name: &str,
        new_name: &str,
    ) -> Result<Cid, HashTreeError> {
        if old_name == new_name {
            return Ok(root.clone());
        }

        let dir_cid = self.resolve_path_array(root, path).await?;
        let dir_cid = dir_cid.ok_or_else(|| HashTreeError::PathNotFound(path.join("/")))?;

        let entries = self.list_directory(&dir_cid).await?;
        let entry = entries
            .iter()
            .find(|e| e.name == old_name)
            .ok_or_else(|| HashTreeError::EntryNotFound(old_name.to_string()))?;

        let entry_hash = entry.hash;
        let entry_size = entry.size;
        let entry_key = entry.key;
        let entry_link_type = entry.link_type;
        let entry_meta = entry.meta.clone();

        let new_entries: Vec<DirEntry> = entries
            .into_iter()
            .filter(|e| e.name != old_name)
            .map(|e| DirEntry {
                name: e.name,
                hash: e.hash,
                size: e.size,
                key: e.key,
                link_type: e.link_type,
                meta: e.meta,
            })
            .chain(std::iter::once(DirEntry {
                name: new_name.to_string(),
                hash: entry_hash,
                size: entry_size,
                key: entry_key,
                link_type: entry_link_type,
                meta: entry_meta,
            }))
            .collect();

        let new_dir_cid = self.put_directory(new_entries).await?;
        self.rebuild_path(root, path, new_dir_cid).await
    }

    /// Move an entry to a different directory
    /// Returns new root Cid
    pub async fn move_entry(
        &self,
        root: &Cid,
        source_path: &[&str],
        name: &str,
        target_path: &[&str],
    ) -> Result<Cid, HashTreeError> {
        let source_dir_cid = self.resolve_path_array(root, source_path).await?;
        let source_dir_cid =
            source_dir_cid.ok_or_else(|| HashTreeError::PathNotFound(source_path.join("/")))?;

        let source_entries = self.list_directory(&source_dir_cid).await?;
        let entry = source_entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| HashTreeError::EntryNotFound(name.to_string()))?;

        let entry_cid = Cid {
            hash: entry.hash,
            key: entry.key,
        };
        let entry_size = entry.size;
        let entry_link_type = entry.link_type;

        // Remove from source
        let new_root = self.remove_entry(root, source_path, name).await?;

        // Add to target
        self.set_entry(
            &new_root,
            target_path,
            name,
            &entry_cid,
            entry_size,
            entry_link_type,
        )
        .await
    }

    async fn resolve_path_array(
        &self,
        root: &Cid,
        path: &[&str],
    ) -> Result<Option<Cid>, HashTreeError> {
        if path.is_empty() {
            return Ok(Some(root.clone()));
        }
        self.resolve_path(root, &path.join("/")).await
    }

    async fn rebuild_path(
        &self,
        root: &Cid,
        path: &[&str],
        new_child: Cid,
    ) -> Result<Cid, HashTreeError> {
        if path.is_empty() {
            return Ok(new_child);
        }

        let mut child_cid = new_child;
        let parts: Vec<&str> = path.to_vec();

        for i in (0..parts.len()).rev() {
            let child_name = parts[i];
            let parent_path = &parts[..i];

            let parent_cid = if parent_path.is_empty() {
                root.clone()
            } else {
                self.resolve_path_array(root, parent_path)
                    .await?
                    .ok_or_else(|| HashTreeError::PathNotFound(parent_path.join("/")))?
            };

            let parent_entries = self.list_directory(&parent_cid).await?;
            let new_parent_entries: Vec<DirEntry> = parent_entries
                .into_iter()
                .map(|e| {
                    if e.name == child_name {
                        DirEntry {
                            name: e.name,
                            hash: child_cid.hash,
                            size: 0, // Directories don't have a meaningful size in the link
                            key: child_cid.key,
                            link_type: e.link_type,
                            meta: e.meta,
                        }
                    } else {
                        DirEntry {
                            name: e.name,
                            hash: e.hash,
                            size: e.size,
                            key: e.key,
                            link_type: e.link_type,
                            meta: e.meta,
                        }
                    }
                })
                .collect();

            child_cid = self.put_directory(new_parent_entries).await?;
        }

        Ok(child_cid)
    }

    // ============ UTILITY ============

    /// Get the underlying store
    pub fn get_store(&self) -> Arc<S> {
        self.store.clone()
    }

    /// Get chunk size configuration
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Get max links configuration
    pub fn max_links(&self) -> usize {
        self.max_links
    }
}

/// Verify tree integrity - checks that all referenced hashes exist
pub async fn verify_tree<S: Store>(
    store: Arc<S>,
    root_hash: &Hash,
) -> Result<crate::reader::VerifyResult, HashTreeError> {
    let mut missing = Vec::new();
    let mut visited = std::collections::HashSet::new();

    verify_recursive(store, root_hash, &mut missing, &mut visited).await?;

    Ok(crate::reader::VerifyResult {
        valid: missing.is_empty(),
        missing,
    })
}

async fn verify_recursive<S: Store>(
    store: Arc<S>,
    hash: &Hash,
    missing: &mut Vec<Hash>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<(), HashTreeError> {
    let hex = to_hex(hash);
    if visited.contains(&hex) {
        return Ok(());
    }
    visited.insert(hex);

    let data = match store
        .get(hash)
        .await
        .map_err(|e| HashTreeError::Store(e.to_string()))?
    {
        Some(d) => d,
        None => {
            missing.push(*hash);
            return Ok(());
        }
    };

    if is_tree_node(&data) {
        let node = decode_tree_node(&data)?;
        for link in &node.links {
            Box::pin(verify_recursive(
                store.clone(),
                &link.hash,
                missing,
                visited,
            ))
            .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
