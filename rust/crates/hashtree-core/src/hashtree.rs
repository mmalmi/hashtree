//! HashTree - Unified merkle tree operations
//!
//! Single struct for creating, reading, and editing content-addressed merkle trees.
//! Mirrors the hashtree-ts HashTree class API.

use std::pin::Pin;
use std::sync::Arc;

use futures::stream::{self, Stream};
use futures::io::AsyncRead;
use futures::AsyncReadExt;

use crate::builder::{BuilderError, DEFAULT_CHUNK_SIZE, DEFAULT_MAX_LINKS};
use crate::codec::{decode_tree_node, encode_and_hash, is_directory_node, is_tree_node, try_decode_tree_node};
use crate::hash::sha256;
use crate::reader::{ReaderError, TreeEntry, WalkEntry};
use crate::store::Store;
use crate::types::{to_hex, Cid, DirEntry, Hash, Link, LinkType, TreeNode};

use crate::crypto::{decrypt_chk, encrypt_chk, EncryptionKey};

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
            ReaderError::MissingKey => HashTreeError::Encryption("missing decryption key".to_string()),
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
        Ok((Cid { hash: root_hash, key: root_key }, size))
    }

    /// Get content by Cid (handles decryption automatically)
    pub async fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>, HashTreeError> {
        if let Some(key) = cid.key {
            self.get_encrypted(&cid.hash, &key).await
        } else {
            self.read_file(&cid.hash).await
        }
    }

    /// Store content from an async reader (streaming put)
    ///
    /// Reads data in chunks and builds a merkle tree incrementally.
    /// Useful for large files or streaming data sources.
    /// Returns (Cid, size) where Cid is hash + optional key
    pub async fn put_stream<R: AsyncRead + Unpin>(&self, mut reader: R) -> Result<(Cid, u64), HashTreeError> {
        let mut buffer = vec![0u8; self.chunk_size];
        let mut links = Vec::new();
        let mut total_size: u64 = 0;
        let mut consistent_key: Option<[u8; 32]> = None;

        loop {
            let mut chunk = Vec::new();
            let mut bytes_read = 0;

            // Read until we have a full chunk or EOF
            while bytes_read < self.chunk_size {
                let n = reader.read(&mut buffer[..self.chunk_size - bytes_read]).await
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
        Ok((Cid { hash: root_hash, key: root_key }, total_size))
    }

    /// Read content as a stream of chunks by Cid (handles decryption automatically)
    ///
    /// Returns an async stream that yields chunks as they are read.
    /// Useful for large files or when you want to process data incrementally.
    pub fn get_stream(
        &self,
        cid: &Cid,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, HashTreeError>> + Send + '_>> {
        let hash = cid.hash;
        let key = cid.key;

        if let Some(k) = key {
            // Encrypted stream
            Box::pin(self.read_file_stream_encrypted(hash, k))
        } else {
            // Unencrypted stream
            self.read_file_stream(hash)
        }
    }

    /// Read encrypted file as stream (internal)
    fn read_file_stream_encrypted(
        &self,
        hash: Hash,
        key: EncryptionKey,
    ) -> impl Stream<Item = Result<Vec<u8>, HashTreeError>> + Send + '_ {
        stream::unfold(
            EncryptedStreamState::Init { hash, key, tree: self },
            |state| async move {
                match state {
                    EncryptedStreamState::Init { hash, key, tree } => {
                        let data = match tree.store.get(&hash).await {
                            Ok(Some(d)) => d,
                            Ok(None) => return None,
                            Err(e) => return Some((Err(HashTreeError::Store(e.to_string())), EncryptedStreamState::Done)),
                        };

                        // Try to decrypt
                        let decrypted = match decrypt_chk(&data, &key) {
                            Ok(d) => d,
                            Err(e) => return Some((Err(HashTreeError::Decryption(e.to_string())), EncryptedStreamState::Done)),
                        };

                        if !is_tree_node(&decrypted) {
                            // Single blob - yield decrypted data
                            return Some((Ok(decrypted), EncryptedStreamState::Done));
                        }

                        // Tree node - parse and traverse
                        let node = match decode_tree_node(&decrypted) {
                            Ok(n) => n,
                            Err(e) => return Some((Err(HashTreeError::Codec(e)), EncryptedStreamState::Done)),
                        };

                        let mut stack: Vec<EncryptedStackItem> = Vec::new();
                        for link in node.links.into_iter().rev() {
                            stack.push(EncryptedStackItem { hash: link.hash, key: link.key });
                        }

                        tree.process_encrypted_stream_stack(&mut stack).await
                    }
                    EncryptedStreamState::Processing { mut stack, tree } => {
                        tree.process_encrypted_stream_stack(&mut stack).await
                    }
                    EncryptedStreamState::Done => None,
                }
            },
        )
    }

    async fn process_encrypted_stream_stack<'a>(
        &'a self,
        stack: &mut Vec<EncryptedStackItem>,
    ) -> Option<(Result<Vec<u8>, HashTreeError>, EncryptedStreamState<'a, S>)> {
        while let Some(item) = stack.pop() {
            let data = match self.store.get(&item.hash).await {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return Some((
                        Err(HashTreeError::MissingChunk(to_hex(&item.hash))),
                        EncryptedStreamState::Done,
                    ))
                }
                Err(e) => {
                    return Some((
                        Err(HashTreeError::Store(e.to_string())),
                        EncryptedStreamState::Done,
                    ))
                }
            };

            // Decrypt if we have a key
            let decrypted = if let Some(key) = item.key {
                match decrypt_chk(&data, &key) {
                    Ok(d) => d,
                    Err(e) => {
                        return Some((
                            Err(HashTreeError::Decryption(e.to_string())),
                            EncryptedStreamState::Done,
                        ))
                    }
                }
            } else {
                data
            };

            if is_tree_node(&decrypted) {
                // Nested tree node - add children to stack
                let node = match decode_tree_node(&decrypted) {
                    Ok(n) => n,
                    Err(e) => {
                        return Some((
                            Err(HashTreeError::Codec(e)),
                            EncryptedStreamState::Done,
                        ))
                    }
                };
                for link in node.links.into_iter().rev() {
                    stack.push(EncryptedStackItem { hash: link.hash, key: link.key });
                }
            } else {
                // Leaf chunk - yield decrypted data
                return Some((
                    Ok(decrypted),
                    EncryptedStreamState::Processing { stack: std::mem::take(stack), tree: self },
                ));
            }
        }
        None
    }

    /// Store a chunk with optional encryption
    async fn put_chunk_internal(&self, data: &[u8]) -> Result<(Hash, Option<EncryptionKey>), HashTreeError> {
        if self.encrypted {
            let (encrypted, key) = encrypt_chk(data)
                .map_err(|e| HashTreeError::Encryption(e.to_string()))?;
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
                let (encrypted, key) = encrypt_chk(&data)
                    .map_err(|e| HashTreeError::Encryption(e.to_string()))?;
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
            let (hash, key) = Box::pin(self.build_tree_internal(batch.to_vec(), Some(batch_size))).await?;
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
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        let encrypted_data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        // Decrypt the data
        let decrypted = decrypt_chk(&encrypted_data, key)
            .map_err(|e| HashTreeError::Encryption(e.to_string()))?;

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
    async fn assemble_encrypted_chunks(&self, node: &TreeNode) -> Result<Vec<u8>, HashTreeError> {
        let mut parts: Vec<Vec<u8>> = Vec::new();

        for link in &node.links {
            let chunk_key = link.key.ok_or_else(|| HashTreeError::Encryption("missing chunk key".to_string()))?;

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
        Ok((Cid { hash: root_hash, key: root_key }, size))
    }

    /// Build a directory from entries
    /// Returns Cid with key if encrypted
    ///
    /// For large directories, the messagepack-encoded TreeNode is stored via put()
    /// which automatically chunks the data. The reader uses read_file() to reassemble.
    pub async fn put_directory(
        &self,
        entries: Vec<DirEntry>,
    ) -> Result<Cid, HashTreeError> {
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

        // Create the directory node with all entries
        let node = TreeNode {
            node_type: LinkType::Dir,
            links,
        };
        let (data, _plain_hash) = encode_and_hash(&node)?;

        // Store directory data via put() - handles both small and large directories
        // For small dirs, stores as single chunk
        // For large dirs, chunks transparently via build_tree()
        // Reader uses read_file() to reassemble before decoding
        let (cid, _size) = self.put(&data).await?;
        Ok(cid)
    }

    /// Create a tree node with custom links
    pub async fn put_tree_node(
        &self,
        links: Vec<Link>,
    ) -> Result<Hash, HashTreeError> {
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

    /// Get and decode a tree node (unencrypted)
    pub async fn get_tree_node(&self, hash: &Hash) -> Result<Option<TreeNode>, HashTreeError> {
        let data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        if !is_tree_node(&data) {
            return Ok(None);
        }

        let node = decode_tree_node(&data)?;
        Ok(Some(node))
    }

    /// Get and decode a tree node using Cid (with decryption if key present)
    pub async fn get_node(&self, cid: &Cid) -> Result<Option<TreeNode>, HashTreeError> {
        let data = match self.store.get(&cid.hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        // Decrypt if key is present
        let decrypted = if let Some(key) = &cid.key {
            decrypt_chk(&data, key)
                .map_err(|e| HashTreeError::Decryption(e.to_string()))?
        } else {
            data
        };

        if !is_tree_node(&decrypted) {
            return Ok(None);
        }

        let node = decode_tree_node(&decrypted)?;
        Ok(Some(node))
    }

    /// Get directory node, handling chunked directory data
    /// Use this when you know the target is a directory (from parent link_type)
    pub async fn get_directory_node(&self, cid: &Cid) -> Result<Option<TreeNode>, HashTreeError> {
        let data = match self.store.get(&cid.hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        // Decrypt if key is present
        let decrypted = if let Some(key) = &cid.key {
            decrypt_chk(&data, key)
                .map_err(|e| HashTreeError::Decryption(e.to_string()))?
        } else {
            data
        };

        if !is_tree_node(&decrypted) {
            return Ok(None);
        }

        let node = decode_tree_node(&decrypted)?;

        // If this is a file tree (chunked data), reassemble to get actual directory
        if node.node_type == LinkType::File {
            let assembled = self.assemble_chunks(&node).await?;
            if is_tree_node(&assembled) {
                let inner_node = decode_tree_node(&assembled)?;
                return Ok(Some(inner_node));
            }
        }

        Ok(Some(node))
    }

    /// Check if hash points to a tree node (no decryption)
    pub async fn is_tree(&self, hash: &Hash) -> Result<bool, HashTreeError> {
        let data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(false),
        };
        Ok(is_tree_node(&data))
    }

    /// Check if Cid points to a directory (with decryption)
    pub async fn is_dir(&self, cid: &Cid) -> Result<bool, HashTreeError> {
        let node = match self.get_node(cid).await? {
            Some(n) => n,
            None => return Ok(false),
        };
        // Directory has named links (not just internal chunks)
        Ok(node.links.iter().any(|l| l.name.as_ref().map(|n| !n.starts_with('_')).unwrap_or(false)))
    }

    /// Check if hash points to a directory (tree with named links, no decryption)
    pub async fn is_directory(&self, hash: &Hash) -> Result<bool, HashTreeError> {
        let data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(false),
        };
        Ok(is_directory_node(&data))
    }

    /// Read a complete file (reassemble chunks if needed)
    pub async fn read_file(&self, hash: &Hash) -> Result<Option<Vec<u8>>, HashTreeError> {
        let data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(None),
        };

        // Check if it's a tree (chunked file) or raw blob
        if !is_tree_node(&data) {
            return Ok(Some(data));
        }

        // It's a tree - reassemble chunks
        let node = decode_tree_node(&data)?;
        let assembled = self.assemble_chunks(&node).await?;
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
        let data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
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
        self.collect_chunk_offsets_recursive(node, &mut chunks, &mut offset).await?;
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
    async fn assemble_chunks(&self, node: &TreeNode) -> Result<Vec<u8>, HashTreeError> {
        let mut parts: Vec<Vec<u8>> = Vec::new();

        for link in &node.links {
            let child_data = self
                .store
                .get(&link.hash)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?
                .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;

            if is_tree_node(&child_data) {
                let child_node = decode_tree_node(&child_data)?;
                parts.push(Box::pin(self.assemble_chunks(&child_node)).await?);
            } else {
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

    /// Read a file as stream of chunks
    /// Returns an async stream that yields chunks as they are read
    pub fn read_file_stream(
        &self,
        hash: Hash,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<u8>, HashTreeError>> + Send + '_>> {
        Box::pin(stream::unfold(
            ReadStreamState::Init { hash, tree: self },
            |state| async move {
                match state {
                    ReadStreamState::Init { hash, tree } => {
                        let data = match tree.store.get(&hash).await {
                            Ok(Some(d)) => d,
                            Ok(None) => return None,
                            Err(e) => return Some((Err(HashTreeError::Store(e.to_string())), ReadStreamState::Done)),
                        };

                        if !is_tree_node(&data) {
                            // Single blob - yield it and finish
                            return Some((Ok(data), ReadStreamState::Done));
                        }

                        // Tree node - start streaming chunks
                        let node = match decode_tree_node(&data) {
                            Ok(n) => n,
                            Err(e) => return Some((Err(HashTreeError::Codec(e)), ReadStreamState::Done)),
                        };

                        // Create stack with all links to process
                        let mut stack: Vec<StreamStackItem> = Vec::new();
                        for link in node.links.into_iter().rev() {
                            stack.push(StreamStackItem::Hash(link.hash));
                        }

                        // Process first item
                        tree.process_stream_stack(&mut stack).await
                    }
                    ReadStreamState::Processing { mut stack, tree } => {
                        tree.process_stream_stack(&mut stack).await
                    }
                    ReadStreamState::Done => None,
                }
            },
        ))
    }

    async fn process_stream_stack<'a>(
        &'a self,
        stack: &mut Vec<StreamStackItem>,
    ) -> Option<(Result<Vec<u8>, HashTreeError>, ReadStreamState<'a, S>)> {
        while let Some(item) = stack.pop() {
            match item {
                StreamStackItem::Hash(hash) => {
                    let data = match self.store.get(&hash).await {
                        Ok(Some(d)) => d,
                        Ok(None) => {
                            return Some((
                                Err(HashTreeError::MissingChunk(to_hex(&hash))),
                                ReadStreamState::Done,
                            ))
                        }
                        Err(e) => {
                            return Some((
                                Err(HashTreeError::Store(e.to_string())),
                                ReadStreamState::Done,
                            ))
                        }
                    };

                    if is_tree_node(&data) {
                        // Nested tree - push its children to stack
                        let node = match decode_tree_node(&data) {
                            Ok(n) => n,
                            Err(e) => return Some((Err(HashTreeError::Codec(e)), ReadStreamState::Done)),
                        };
                        for link in node.links.into_iter().rev() {
                            stack.push(StreamStackItem::Hash(link.hash));
                        }
                    } else {
                        // Leaf blob - yield it
                        return Some((Ok(data), ReadStreamState::Processing { stack: std::mem::take(stack), tree: self }));
                    }
                }
            }
        }
        None
    }

    /// Read file chunks as Vec (non-streaming version)
    pub async fn read_file_chunks(&self, hash: &Hash) -> Result<Vec<Vec<u8>>, HashTreeError> {
        let data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
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
            if let Some(ref name) = link.name {
                if name.starts_with("_chunk_") || name.starts_with('_') {
                    let chunk_cid = Cid { hash: link.hash, key: link.key };
                    let sub_entries = Box::pin(self.list(&chunk_cid)).await?;
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

    /// List directory entries using Cid (with decryption if key present)
    /// Handles both regular and chunked directory data
    pub async fn list_directory(&self, cid: &Cid) -> Result<Vec<TreeEntry>, HashTreeError> {
        // Use get_directory_node which handles chunked directory data
        let node = match self.get_directory_node(cid).await? {
            Some(n) => n,
            None => return Ok(vec![]),
        };

        let mut entries = Vec::new();

        for link in &node.links {
            // Skip internal chunk nodes (backwards compat with old _chunk_ format)
            if let Some(ref name) = link.name {
                if name.starts_with("_chunk_") || name.starts_with('_') {
                    // Internal nodes inherit parent's key for decryption
                    let sub_cid = Cid { hash: link.hash, key: cid.key };
                    let sub_entries = Box::pin(self.list_directory(&sub_cid)).await?;
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
                None => return Ok(None),
            };

            if let Some(link) = self.find_link(&node, part) {
                current_cid = Cid {
                    hash: link.hash,
                    key: link.key,
                };
            } else {
                // Check internal nodes
                match self.find_link_in_subtrees_cid(&node, part, &current_cid).await? {
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
    async fn find_link_in_subtrees_cid(&self, node: &TreeNode, name: &str, _parent_cid: &Cid) -> Result<Option<Link>, HashTreeError> {
        for link in &node.links {
            if !link.name.as_ref().map(|n| n.starts_with('_')).unwrap_or(false) {
                continue;
            }

            // Internal nodes inherit encryption from parent context
            let sub_cid = Cid {
                hash: link.hash,
                key: link.key.clone(),
            };

            let sub_node = match self.get_node(&sub_cid).await? {
                Some(n) => n,
                None => continue,
            };

            if let Some(found) = self.find_link(&sub_node, name) {
                return Ok(Some(found));
            }

            if let Some(deep_found) = Box::pin(self.find_link_in_subtrees_cid(&sub_node, name, &sub_cid)).await? {
                return Ok(Some(deep_found));
            }
        }

        Ok(None)
    }

    /// Get total size of a tree
    pub async fn get_size(&self, hash: &Hash) -> Result<u64, HashTreeError> {
        let data = match self.store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
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

    /// Walk entire tree depth-first (returns Vec)
    pub async fn walk(&self, cid: &Cid, path: &str) -> Result<Vec<WalkEntry>, HashTreeError> {
        let mut entries = Vec::new();
        self.walk_recursive(cid, path, &mut entries).await?;
        Ok(entries)
    }

    async fn walk_recursive(
        &self,
        cid: &Cid,
        path: &str,
        entries: &mut Vec<WalkEntry>,
    ) -> Result<(), HashTreeError> {
        let data = match self.store.get(&cid.hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
            Some(d) => d,
            None => return Ok(()),
        };

        // Decrypt if key is present
        let data = if let Some(key) = &cid.key {
            decrypt_chk(&data, key).map_err(|e| HashTreeError::Decryption(e.to_string()))?
        } else {
            data
        };

        let node = match try_decode_tree_node(&data) {
            Some(n) => n,
            None => {
                entries.push(WalkEntry {
                    path: path.to_string(),
                    hash: cid.hash,
                    link_type: LinkType::Blob,
                    size: data.len() as u64,
                    key: cid.key,
                });
                return Ok(());
            }
        };

        let node_size: u64 = node.links.iter().map(|l| l.size).sum();
        entries.push(WalkEntry {
            path: path.to_string(),
            hash: cid.hash,
            link_type: node.node_type,
            size: node_size,
            key: cid.key,
        });

        for link in &node.links {
            let child_path = match &link.name {
                Some(name) => {
                    if name.starts_with("_chunk_") || name.starts_with('_') {
                        // Internal nodes inherit parent's key
                        let sub_cid = Cid { hash: link.hash, key: cid.key };
                        Box::pin(self.walk_recursive(&sub_cid, path, entries)).await?;
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

            // Child nodes use their own key from link
            let child_cid = Cid { hash: link.hash, key: link.key };
            Box::pin(self.walk_recursive(&child_cid, &child_path, entries)).await?;
        }

        Ok(())
    }

    /// Walk entire tree with parallel fetching
    /// Uses a work-stealing approach: always keeps `concurrency` requests in flight
    pub async fn walk_parallel(&self, cid: &Cid, path: &str, concurrency: usize) -> Result<Vec<WalkEntry>, HashTreeError> {
        self.walk_parallel_with_progress(cid, path, concurrency, None).await
    }

    /// Walk entire tree with parallel fetching and optional progress counter
    /// The counter is incremented for each node fetched (not just entries found)
    ///
    /// OPTIMIZATION: Blobs are NOT fetched - their metadata (hash, size, link_type)
    /// comes from the parent node's link, so we just add them directly to entries.
    /// This avoids downloading file contents during tree traversal.
    pub async fn walk_parallel_with_progress(
        &self,
        cid: &Cid,
        path: &str,
        concurrency: usize,
        progress: Option<&std::sync::atomic::AtomicUsize>,
    ) -> Result<Vec<WalkEntry>, HashTreeError> {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::collections::VecDeque;
        use std::sync::atomic::Ordering;

        let mut entries = Vec::new();
        let mut pending: VecDeque<(Cid, String)> = VecDeque::new();
        let mut active = FuturesUnordered::new();

        // Seed with root
        pending.push_back((cid.clone(), path.to_string()));

        loop {
            // Fill up to concurrency limit from pending queue
            while active.len() < concurrency {
                if let Some((node_cid, node_path)) = pending.pop_front() {
                    let store = &self.store;
                    let fut = async move {
                        let data = store.get(&node_cid.hash).await
                            .map_err(|e| HashTreeError::Store(e.to_string()))?;
                        Ok::<_, HashTreeError>((node_cid, node_path, data))
                    };
                    active.push(fut);
                } else {
                    break;
                }
            }

            // If nothing active, we're done
            if active.is_empty() {
                break;
            }

            // Wait for any future to complete
            if let Some(result) = active.next().await {
                let (node_cid, node_path, data) = result?;

                // Update progress counter
                if let Some(counter) = progress {
                    counter.fetch_add(1, Ordering::Relaxed);
                }

                let data = match data {
                    Some(d) => d,
                    None => continue,
                };

                // Decrypt if key is present
                let data = if let Some(key) = &node_cid.key {
                    decrypt_chk(&data, key).map_err(|e| HashTreeError::Decryption(e.to_string()))?
                } else {
                    data
                };

                let node = match try_decode_tree_node(&data) {
                    Some(n) => n,
                    None => {
                        // It's a blob/file - this case only happens for root
                        entries.push(WalkEntry {
                            path: node_path,
                            hash: node_cid.hash,
                            link_type: LinkType::Blob,
                            size: data.len() as u64,
                            key: node_cid.key,
                        });
                        continue;
                    }
                };

                // It's a directory/file node
                let node_size: u64 = node.links.iter().map(|l| l.size).sum();
                entries.push(WalkEntry {
                    path: node_path.clone(),
                    hash: node_cid.hash,
                    link_type: node.node_type,
                    size: node_size,
                    key: node_cid.key,
                });

                // Queue children - but DON'T fetch blobs, just add them directly
                for link in &node.links {
                    let child_path = match &link.name {
                        Some(name) => {
                            if name.starts_with("_chunk_") || name.starts_with('_') {
                                // Internal chunked nodes - inherit parent's key, same path
                                let sub_cid = Cid { hash: link.hash, key: node_cid.key };
                                pending.push_back((sub_cid, node_path.clone()));
                                continue;
                            }
                            if node_path.is_empty() {
                                name.clone()
                            } else {
                                format!("{}/{}", node_path, name)
                            }
                        }
                        None => node_path.clone(),
                    };

                    // OPTIMIZATION: If it's a blob, add entry directly without fetching
                    // The link already contains all the metadata we need
                    if link.link_type == LinkType::Blob {
                        entries.push(WalkEntry {
                            path: child_path,
                            hash: link.hash,
                            link_type: LinkType::Blob,
                            size: link.size,
                            key: link.key,
                        });
                        if let Some(counter) = progress {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }

                    // For tree nodes (File/Dir), we need to fetch to see their children
                    let child_cid = Cid { hash: link.hash, key: link.key };
                    pending.push_back((child_cid, child_path));
                }
            }
        }

        Ok(entries)
    }

    /// Walk tree as stream
    pub fn walk_stream(
        &self,
        cid: Cid,
        initial_path: String,
    ) -> Pin<Box<dyn Stream<Item = Result<WalkEntry, HashTreeError>> + Send + '_>> {
        Box::pin(stream::unfold(
            WalkStreamState::Init { cid, path: initial_path, tree: self },
            |state| async move {
                match state {
                    WalkStreamState::Init { cid, path, tree } => {
                        let data = match tree.store.get(&cid.hash).await {
                            Ok(Some(d)) => d,
                            Ok(None) => return None,
                            Err(e) => {
                                return Some((
                                    Err(HashTreeError::Store(e.to_string())),
                                    WalkStreamState::Done,
                                ))
                            }
                        };

                        // Decrypt if key is present
                        let data = if let Some(key) = &cid.key {
                            match decrypt_chk(&data, key) {
                                Ok(d) => d,
                                Err(e) => return Some((Err(HashTreeError::Decryption(e.to_string())), WalkStreamState::Done)),
                            }
                        } else {
                            data
                        };

                        let node = match try_decode_tree_node(&data) {
                            Some(n) => n,
                            None => {
                                // Blob data
                                let entry = WalkEntry {
                                    path,
                                    hash: cid.hash,
                                    link_type: LinkType::Blob,
                                    size: data.len() as u64,
                                    key: cid.key,
                                };
                                return Some((Ok(entry), WalkStreamState::Done));
                            }
                        };

                        let node_size: u64 = node.links.iter().map(|l| l.size).sum();
                        let entry = WalkEntry {
                            path: path.clone(),
                            hash: cid.hash,
                            link_type: node.node_type,
                            size: node_size,
                            key: cid.key,
                        };

                        // Create stack with children to process
                        let mut stack: Vec<WalkStackItem> = Vec::new();
                        for link in node.links.into_iter().rev() {
                            let child_path = match &link.name {
                                Some(name) if !name.starts_with('_') => {
                                    if path.is_empty() {
                                        name.clone()
                                    } else {
                                        format!("{}/{}", path, name)
                                    }
                                }
                                _ => path.clone(),
                            };
                            // Child nodes use their own key from link
                            stack.push(WalkStackItem { hash: link.hash, path: child_path, key: link.key });
                        }

                        Some((Ok(entry), WalkStreamState::Processing { stack, tree }))
                    }
                    WalkStreamState::Processing { mut stack, tree } => {
                        tree.process_walk_stack(&mut stack).await
                    }
                    WalkStreamState::Done => None,
                }
            },
        ))
    }

    async fn process_walk_stack<'a>(
        &'a self,
        stack: &mut Vec<WalkStackItem>,
    ) -> Option<(Result<WalkEntry, HashTreeError>, WalkStreamState<'a, S>)> {
        while let Some(item) = stack.pop() {
            let data = match self.store.get(&item.hash).await {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    return Some((
                        Err(HashTreeError::Store(e.to_string())),
                        WalkStreamState::Done,
                    ))
                }
            };

            let node = match try_decode_tree_node(&data) {
                Some(n) => n,
                None => {
                    // Blob data
                    let entry = WalkEntry {
                        path: item.path,
                        hash: item.hash,
                        link_type: LinkType::Blob,
                        size: data.len() as u64,
                        key: item.key,
                    };
                    return Some((Ok(entry), WalkStreamState::Processing { stack: std::mem::take(stack), tree: self }));
                }
            };

            let node_size: u64 = node.links.iter().map(|l| l.size).sum();
            let entry = WalkEntry {
                path: item.path.clone(),
                hash: item.hash,
                link_type: node.node_type,
                size: node_size,
                key: None, // directories are not encrypted
            };

            // Push children to stack
            for link in node.links.into_iter().rev() {
                let child_path = match &link.name {
                    Some(name) if !name.starts_with('_') => {
                        if item.path.is_empty() {
                            name.clone()
                        } else {
                            format!("{}/{}", item.path, name)
                        }
                    }
                    _ => item.path.clone(),
                };
                stack.push(WalkStackItem { hash: link.hash, path: child_path, key: link.key });
            }

            return Some((Ok(entry), WalkStreamState::Processing { stack: std::mem::take(stack), tree: self }));
        }
        None
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
            meta: None,
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
        let source_dir_cid = source_dir_cid.ok_or_else(|| HashTreeError::PathNotFound(source_path.join("/")))?;

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
        self.set_entry(&new_root, target_path, name, &entry_cid, entry_size, entry_link_type).await
    }

    async fn resolve_path_array(&self, root: &Cid, path: &[&str]) -> Result<Option<Cid>, HashTreeError> {
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

// Internal state types for streaming

enum StreamStackItem {
    Hash(Hash),
}

enum ReadStreamState<'a, S: Store> {
    Init { hash: Hash, tree: &'a HashTree<S> },
    Processing { stack: Vec<StreamStackItem>, tree: &'a HashTree<S> },
    Done,
}

struct WalkStackItem {
    hash: Hash,
    path: String,
    key: Option<[u8; 32]>,
}

enum WalkStreamState<'a, S: Store> {
    Init { cid: Cid, path: String, tree: &'a HashTree<S> },
    Processing { stack: Vec<WalkStackItem>, tree: &'a HashTree<S> },
    Done,
}

// Encrypted stream state types
struct EncryptedStackItem {
    hash: Hash,
    key: Option<[u8; 32]>,
}

enum EncryptedStreamState<'a, S: Store> {
    Init { hash: Hash, key: [u8; 32], tree: &'a HashTree<S> },
    Processing { stack: Vec<EncryptedStackItem>, tree: &'a HashTree<S> },
    Done,
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

    let data = match store.get(hash).await.map_err(|e| HashTreeError::Store(e.to_string()))? {
        Some(d) => d,
        None => {
            missing.push(*hash);
            return Ok(());
        }
    };

    if is_tree_node(&data) {
        let node = decode_tree_node(&data)?;
        for link in &node.links {
            Box::pin(verify_recursive(store.clone(), &link.hash, missing, visited)).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    fn make_tree() -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        // Use public (unencrypted) mode for these tests
        let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());
        (store, tree)
    }

    #[tokio::test]
    async fn test_put_and_read_blob() {
        let (_store, tree) = make_tree();

        let data = vec![1, 2, 3, 4, 5];
        let hash = tree.put_blob(&data).await.unwrap();

        let result = tree.get_blob(&hash).await.unwrap();
        assert_eq!(result, Some(data));
    }

    #[tokio::test]
    async fn test_put_and_read_file_small() {
        let (_store, tree) = make_tree();

        let data = b"Hello, World!";
        let (cid, size) = tree.put_file(data).await.unwrap();

        assert_eq!(size, data.len() as u64);

        let read_data = tree.read_file(&cid.hash).await.unwrap();
        assert_eq!(read_data, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn test_put_and_read_directory() {
        let (_store, tree) = make_tree();

        let file1 = tree.put_blob(b"content1").await.unwrap();
        let file2 = tree.put_blob(b"content2").await.unwrap();

        let dir_cid = tree
            .put_directory(
                vec![
                    DirEntry::new("a.txt", file1).with_size(8),
                    DirEntry::new("b.txt", file2).with_size(8),
                ],
            )
            .await
            .unwrap();

        let entries = tree.list_directory(&dir_cid).await.unwrap();
        assert_eq!(entries.len(), 2);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[tokio::test]
    async fn test_is_directory() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"data").await.unwrap();
        let dir_cid = tree.put_directory(vec![]).await.unwrap();

        assert!(!tree.is_directory(&file_hash).await.unwrap());
        assert!(tree.is_directory(&dir_cid.hash).await.unwrap());
    }

    #[tokio::test]
    async fn test_resolve_path() {
        let (_store, tree) = make_tree();

        let file_hash = tree.put_blob(b"nested").await.unwrap();
        let sub_dir = tree.put_directory(
            vec![DirEntry::new("file.txt", file_hash).with_size(6)],
        ).await.unwrap();
        let root_dir = tree.put_directory(
            vec![DirEntry::new("subdir", sub_dir.hash)],
        ).await.unwrap();

        let resolved = tree.resolve_path(&root_dir, "subdir/file.txt").await.unwrap();
        assert_eq!(resolved.map(|c| c.hash), Some(file_hash));
    }

    // ============ UNIFIED API TESTS ============

    #[tokio::test]
    async fn test_unified_put_get_public() {
        let store = Arc::new(MemoryStore::new());
        // Use .public() to disable encryption
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let data = b"Hello, public world!";
        let (cid, size) = tree.put(data).await.unwrap();

        assert_eq!(size, data.len() as u64);
        assert!(cid.key.is_none()); // No key for public content

        let retrieved = tree.get(&cid).await.unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_unified_put_get_encrypted() {
        let store = Arc::new(MemoryStore::new());
        // Default config has encryption enabled
        let tree = HashTree::new(HashTreeConfig::new(store));

        let data = b"Hello, encrypted world!";
        let (cid, size) = tree.put(data).await.unwrap();

        assert_eq!(size, data.len() as u64);
        assert!(cid.key.is_some()); // Has encryption key

        let retrieved = tree.get(&cid).await.unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_unified_put_get_encrypted_chunked() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store).with_chunk_size(100));

        // Data larger than chunk size
        let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
        let (cid, size) = tree.put(&data).await.unwrap();

        assert_eq!(size, data.len() as u64);
        assert!(cid.key.is_some());

        let retrieved = tree.get(&cid).await.unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_cid_deterministic() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store));

        let data = b"Same content produces same CID";

        let (cid1, _) = tree.put(data).await.unwrap();
        let (cid2, _) = tree.put(data).await.unwrap();

        // CHK: same content = same hash AND same key
        assert_eq!(cid1.hash, cid2.hash);
        assert_eq!(cid1.key, cid2.key);
        assert_eq!(cid1.to_string(), cid2.to_string());
    }

    #[tokio::test]
    async fn test_cid_to_string_public() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let (cid, _) = tree.put(b"test").await.unwrap();
        let s = cid.to_string();

        // Public CID is just the hash (64 hex chars)
        assert_eq!(s.len(), 64);
        assert!(!s.contains(':'));
    }

    #[tokio::test]
    async fn test_cid_to_string_encrypted() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store));

        let (cid, _) = tree.put(b"test").await.unwrap();
        let s = cid.to_string();

        // Encrypted CID is "hash:key" (64 + 1 + 64 = 129 chars)
        assert_eq!(s.len(), 129);
        assert!(s.contains(':'));
    }
}
