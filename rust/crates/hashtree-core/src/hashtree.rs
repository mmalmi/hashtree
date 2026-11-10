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
use crate::codec::{decode_tree_node, encode_and_hash, is_directory_node, is_tree_node};
use crate::directory::{directory_fanout_meta, is_internal_directory_link, DirectoryFanoutSpan};
use crate::hash::sha256;
use crate::reader::{ReaderError, TreeEntry, WalkEntry};
use crate::store::Store;
use crate::types::{to_hex, Cid, DirEntry, Hash, Link, LinkType, TreeNode};

use crate::encryption::{decrypt_object, ObjectKey};

const DEFAULT_STREAM_PUT_BATCH_TARGET_BYTES: usize = 64 * 1024 * 1024;
const STREAM_PUT_BATCH_TARGET_BYTES_ENV: &str = "HTREE_STREAM_PUT_BATCH_TARGET_BYTES";
const STREAM_PUT_BATCH_MAX_ITEMS: usize = 128;
const STREAM_PUT_INITIAL_CHUNK_CAPACITY: usize = 64 * 1024;
/// Upper bound on how much an in-memory read reserves from declared link sizes.
const ASSEMBLE_MAX_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

#[path = "hashtree/stream.rs"]
mod read_stream;
mod walk;

/// HashTree configuration
#[derive(Clone)]
pub struct HashTreeConfig<S: Store> {
    pub store: Arc<S>,
    pub chunk_size: usize,
    pub max_links: usize,
    /// Whether to encrypt content (default: true).
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

fn decrypt_cid_root_bytes(cid: &Cid, data: Vec<u8>) -> Result<Vec<u8>, HashTreeError> {
    let Some(key) = &cid.key else {
        return Ok(data);
    };

    match decrypt_object(&data, key) {
        // Successful authentication is authoritative, even if ciphertext happens
        // to resemble a tree. Historical plaintext trees may still carry a key.
        Ok(decrypted) => Ok(decrypted),
        Err(_)
            if key.mode() == crate::encryption::EncryptionMode::LegacyChk
                && is_tree_node(&data) =>
        {
            Ok(data)
        }
        Err(error) => Err(HashTreeError::Decryption(error.to_string())),
    }
}

/// Decode already-loaded CID bytes without reading storage again.
/// The caller remains responsible for verifying the stored content hash.
pub fn decode_tree_node_by_cid(
    cid: &Cid,
    data: Vec<u8>,
) -> Result<Option<TreeNode>, HashTreeError> {
    let decrypted = decrypt_cid_root_bytes(cid, data)?;
    if !is_tree_node(&decrypted) {
        return Ok(None);
    }
    Ok(Some(decode_tree_node(&decrypted)?))
}

/// HashTree - unified create, read, and edit merkle tree operations
pub struct HashTree<S: Store> {
    store: Arc<S>,
    chunk_size: usize,
    max_links: usize,
    encrypted: bool,
    encryption_mode: crate::encryption::EncryptionMode,
}

impl<S: Store> HashTree<S> {
    pub fn new(config: HashTreeConfig<S>) -> Self {
        Self {
            store: config.store,
            chunk_size: config.chunk_size,
            max_links: config.max_links,
            encrypted: config.encrypted,
            encryption_mode: crate::encryption::EncryptionMode::default(),
        }
    }

    /// Select the encryption suite for newly written objects.
    pub fn with_encryption_mode(mut self, mode: crate::encryption::EncryptionMode) -> Self {
        self.encryption_mode = mode;
        self
    }

    /// Check if encryption is enabled
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    // ============ UNIFIED API ============

    /// Store content, returns (Cid, size) where Cid is hash + optional key
    /// Encrypts by default; `HashTreeConfig::public()` selects plaintext writes.
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
        let (root_hash, root_key, _) = self.build_tree_internal(links, Some(size)).await?;
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
        reader: R,
    ) -> Result<(Cid, u64), HashTreeError> {
        self.put_stream_with_progress(reader, |_| {}).await
    }

    /// Store content from an async reader and report each stored chunk size.
    pub async fn put_stream_with_progress<R, F>(
        &self,
        mut reader: R,
        mut progress: F,
    ) -> Result<(Cid, u64), HashTreeError>
    where
        R: AsyncRead + Unpin,
        F: FnMut(u64),
    {
        let mut links = Vec::new();
        let mut total_size: u64 = 0;
        let mut consistent_key: Option<ObjectKey> = None;
        let mut pending_store_items = Vec::new();
        let mut pending_store_bytes = 0usize;
        let batch_target_bytes = stream_put_batch_target_bytes();
        let mut chunk_capacity = self.chunk_size.min(STREAM_PUT_INITIAL_CHUNK_CAPACITY);

        loop {
            // Read one chunk straight into its own buffer. The buffer grows with
            // the data, so a small file costs a small allocation, not chunk_size.
            let mut chunk = Vec::with_capacity(chunk_capacity);
            (&mut reader)
                .take(self.chunk_size as u64)
                .read_to_end(&mut chunk)
                .await
                .map_err(|e| HashTreeError::Store(format!("read error: {}", e)))?;

            if chunk.is_empty() {
                break; // No more data
            }
            if chunk.len() == self.chunk_size {
                // More full chunks are likely; skip the growth steps for them.
                chunk_capacity = self.chunk_size;
            }

            let chunk_len = chunk.len() as u64;
            total_size += chunk_len;

            let (hash, data, key) = self.prepare_chunk_for_store(&chunk)?;

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

            pending_store_bytes = pending_store_bytes.saturating_add(data.len());
            pending_store_items.push((hash, data, chunk_len));
            if pending_store_bytes >= batch_target_bytes
                || pending_store_items.len() >= STREAM_PUT_BATCH_MAX_ITEMS
            {
                Self::flush_stream_store_batch(
                    self.store.as_ref(),
                    &mut pending_store_items,
                    &mut pending_store_bytes,
                    &mut progress,
                )
                .await?;
            }
        }

        if links.is_empty() {
            // Empty input
            let (hash, key) = self.put_chunk_internal(&[]).await?;
            return Ok((Cid { hash, key }, 0));
        }

        Self::flush_stream_store_batch(
            self.store.as_ref(),
            &mut pending_store_items,
            &mut pending_store_bytes,
            &mut progress,
        )
        .await?;

        // Build tree from chunks
        let (root_hash, root_key, _) = self.build_tree_internal(links, Some(total_size)).await?;
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
    ) -> Result<(Hash, Option<ObjectKey>), HashTreeError> {
        let (hash, stored_data, key) = self.prepare_chunk_for_store(data)?;
        self.store
            .put(hash, stored_data)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?;
        Ok((hash, key))
    }

    fn prepare_chunk_for_store(
        &self,
        data: &[u8],
    ) -> Result<(Hash, Vec<u8>, Option<ObjectKey>), HashTreeError> {
        if self.encrypted {
            let (encrypted, key) = crate::encryption::encrypt_object(data, self.encryption_mode)
                .map_err(|e| HashTreeError::Encryption(e.to_string()))?;
            let hash = sha256(&encrypted);
            Ok((hash, encrypted, Some(key)))
        } else {
            let hash = sha256(data);
            Ok((hash, data.to_vec(), None))
        }
    }

    async fn flush_stream_store_batch<F>(
        store: &S,
        pending: &mut Vec<(Hash, Vec<u8>, u64)>,
        pending_bytes: &mut usize,
        progress: &mut F,
    ) -> Result<(), HashTreeError>
    where
        F: FnMut(u64),
    {
        if pending.is_empty() {
            return Ok(());
        }

        let mut items = Vec::with_capacity(pending.len());
        let mut sizes = Vec::with_capacity(pending.len());
        for (hash, data, size) in pending.drain(..) {
            items.push((hash, data));
            sizes.push(size);
        }
        *pending_bytes = 0;

        store
            .put_many(items)
            .await
            .map_err(|e| HashTreeError::Store(e.to_string()))?;
        for size in sizes {
            progress(size);
        }
        Ok(())
    }

    /// Build tree and return (hash, optional_key)
    async fn build_tree_internal(
        &self,
        links: Vec<Link>,
        total_size: Option<u64>,
    ) -> Result<(Hash, Option<ObjectKey>, LinkType), HashTreeError> {
        // Single link with matching size - return directly
        if links.len() == 1 {
            if let Some(ts) = total_size {
                if links[0].size == ts {
                    return Ok((links[0].hash, links[0].key, links[0].link_type));
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
                    crate::encryption::encrypt_object(&data, self.encryption_mode)
                        .map_err(|e| HashTreeError::Encryption(e.to_string()))?;
                let hash = sha256(&encrypted);
                self.store
                    .put(hash, encrypted)
                    .await
                    .map_err(|e| HashTreeError::Store(e.to_string()))?;
                return Ok((hash, Some(key), LinkType::File));
            }

            // Unencrypted path
            let hash = sha256(&data);
            self.store
                .put(hash, data)
                .await
                .map_err(|e| HashTreeError::Store(e.to_string()))?;
            return Ok((hash, None, LinkType::File));
        }

        // Too many links - create subtrees
        let mut sub_links = Vec::new();
        for batch in links.chunks(self.max_links) {
            let batch_size: u64 = batch.iter().map(|l| l.size).sum();
            let (hash, key, link_type) =
                Box::pin(self.build_tree_internal(batch.to_vec(), Some(batch_size))).await?;
            sub_links.push(Link {
                hash,
                name: None,
                size: batch_size,
                key,
                link_type,
                meta: None,
            });
        }

        Box::pin(self.build_tree_internal(sub_links, total_size)).await
    }

    /// Get encrypted content by hash and key
    async fn get_encrypted(
        &self,
        hash: &Hash,
        key: &ObjectKey,
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
                .assemble_chunks_limited(&node, max_size, &mut bytes_read)
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
        key: &ObjectKey,
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

    fn decode_linked_file_node(
        link: &Link,
        data: &[u8],
    ) -> Result<Option<TreeNode>, HashTreeError> {
        match link.link_type {
            LinkType::File => match decode_tree_node(data) {
                Ok(node) => Ok(Some(node)),
                Err(_) if link.size == data.len() as u64 => Ok(None),
                Err(err) => Err(HashTreeError::Codec(err)),
            },
            LinkType::Blob => {
                if link.size == data.len() as u64 {
                    return Ok(None);
                }

                match decode_tree_node(data) {
                    Ok(node) if node.node_type == LinkType::File => Ok(Some(node)),
                    _ => Ok(None),
                }
            }
            LinkType::Dir | LinkType::Fanout => Ok(None),
        }
    }

    async fn read_link_bytes(&self, link: &Link) -> Result<Vec<u8>, HashTreeError> {
        let stored = self
            .get_blob(&link.hash)
            .await?
            .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;
        match &link.key {
            Some(key) => crate::encryption::decrypt_object(&stored, key)
                .map_err(|e| HashTreeError::Decryption(e.to_string())),
            None => Ok(stored),
        }
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
        self.put(data).await
    }

    /// Build a directory from entries
    /// Returns Cid with key if encrypted
    ///
    /// Large directories are split into BUD-17 fanout nodes.
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
        self.put_tree_node_with_type(LinkType::Dir, links).await
    }

    async fn put_tree_node_with_type(
        &self,
        node_type: LinkType,
        links: Vec<Link>,
    ) -> Result<Cid, HashTreeError> {
        let node = TreeNode { node_type, links };
        let (data, plain_hash) = encode_and_hash(&node)?;

        if self.encrypted {
            let (encrypted, key) = crate::encryption::encrypt_object(&data, self.encryption_mode)
                .map_err(|e| HashTreeError::Encryption(e.to_string()))?;
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
        let spans = links
            .into_iter()
            .map(|link| {
                let name = link.name.clone().unwrap_or_else(|| to_hex(&link.hash));
                DirectoryFanoutSpan {
                    link,
                    count: 1,
                    first: name.clone(),
                    last: name,
                }
            })
            .collect();
        self.build_directory_fanout_level(spans, LinkType::Dir)
            .await
    }

    async fn build_directory_fanout_level(
        &self,
        spans: Vec<DirectoryFanoutSpan>,
        child_node_type: LinkType,
    ) -> Result<Cid, HashTreeError> {
        let mut sub_trees: Vec<DirectoryFanoutSpan> = Vec::new();

        for batch in spans.chunks(self.max_links) {
            let Some(first_span) = batch.first() else {
                continue;
            };
            let last_span = batch.last().expect("non-empty fanout batch");
            let count: usize = batch.iter().map(|span| span.count).sum();
            let batch_size: u64 = batch.iter().map(|span| span.link.size).sum();
            let child_links = batch.iter().map(|span| span.link.clone()).collect();
            let child_cid = self
                .put_tree_node_with_type(child_node_type, child_links)
                .await?;

            sub_trees.push(DirectoryFanoutSpan {
                link: Link {
                    hash: child_cid.hash,
                    name: None,
                    size: batch_size,
                    key: child_cid.key,
                    link_type: child_node_type,
                    meta: Some(directory_fanout_meta(
                        count,
                        &first_span.first,
                        &last_span.last,
                    )),
                },
                count,
                first: first_span.first.clone(),
                last: last_span.last.clone(),
            });
        }

        if sub_trees.len() <= self.max_links {
            return self
                .put_tree_node_with_type(
                    LinkType::Fanout,
                    sub_trees.into_iter().map(|span| span.link).collect(),
                )
                .await;
        }

        Box::pin(self.build_directory_fanout_level(sub_trees, LinkType::Fanout)).await
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

    /// Get and decode a tree node through its CID, decrypting it when needed.
    pub async fn get_tree_node_by_cid(&self, cid: &Cid) -> Result<Option<TreeNode>, HashTreeError> {
        self.get_node(cid).await
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

        decrypt_cid_root_bytes(cid, data).map(Some)
    }

    /// Get and decode a tree node using Cid (with decryption if key present)
    pub async fn get_node(&self, cid: &Cid) -> Result<Option<TreeNode>, HashTreeError> {
        let data = match self.get_blob(&cid.hash).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        decode_tree_node_by_cid(cid, data)
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
            Some(node) if node.node_type.is_directory_like()
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
        self.read_file_range_cid(&Cid::public(*hash), start, end)
            .await
    }

    /// Read a byte range, using each link's own encryption descriptor.
    pub async fn read_file_range_cid(
        &self,
        cid: &Cid,
        start: u64,
        end: Option<u64>,
    ) -> Result<Option<Vec<u8>>, HashTreeError> {
        let Some(data) = self.get_cid_root_bytes(cid).await? else {
            return Ok(None);
        };
        if !is_tree_node(&data) {
            let end = end.unwrap_or(data.len() as u64).min(data.len() as u64);
            return Ok(Some(data[start.min(end) as usize..end as usize].to_vec()));
        }
        let node = decode_tree_node(&data)?;
        let mut result = Vec::new();
        self.append_range(&node, start, end.unwrap_or(u64::MAX), 0, &mut result)
            .await?;
        Ok(Some(result))
    }

    /// Return the whole leaf chunk that holds byte `offset`, together with the
    /// offset at which that chunk starts.
    ///
    /// Sequential readers can serve many small reads from one fetched (and,
    /// for encrypted files, once-decrypted) chunk instead of paying for the
    /// chunk on every read. An offset at or past the end yields an empty chunk
    /// positioned at the file size. `None` means the root is not in the store.
    pub async fn read_file_chunk_at(
        &self,
        cid: &Cid,
        offset: u64,
    ) -> Result<Option<(u64, Vec<u8>)>, HashTreeError> {
        let Some(data) = self.get_cid_root_bytes(cid).await? else {
            return Ok(None);
        };
        if !is_tree_node(&data) {
            return Ok(Some((0, data)));
        }

        let mut node = decode_tree_node(&data)?;
        let mut base_offset = 0u64;
        'descend: loop {
            let mut child_start = base_offset;
            for link in &node.links {
                let child_end = child_start.saturating_add(link.size);
                if offset >= child_end {
                    child_start = child_end;
                    continue;
                }

                let child_data = self.read_link_bytes(link).await?;
                match Self::decode_linked_file_node(link, &child_data)? {
                    Some(child_node) => {
                        node = child_node;
                        base_offset = child_start;
                        continue 'descend;
                    }
                    None => return Ok(Some((child_start, child_data))),
                }
            }
            // Past the last link: nothing left at this offset.
            return Ok(Some((child_start, Vec::new())));
        }
    }

    async fn append_range(
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

            let from = start.saturating_sub(child_start);
            let to = end.min(child_end) - child_start;
            if from >= to {
                continue;
            }

            if link.link_type == LinkType::Blob && link.key.is_none() {
                let bytes = self
                    .store
                    .get_range(&link.hash, from, to - 1)
                    .await
                    .map_err(|e| HashTreeError::Store(e.to_string()))?
                    .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&link.hash)))?;
                if bytes.len() as u64 != to - from {
                    return Err(HashTreeError::Store(format!(
                        "unexpected range length for {}",
                        to_hex(&link.hash)
                    )));
                }
                result.extend_from_slice(&bytes);
                continue;
            }

            let decrypted_child = self.read_link_bytes(link).await?;
            if let Some(child_node) = Self::decode_linked_file_node(link, &decrypted_child)? {
                Box::pin(self.append_range(&child_node, start, end, child_start, result)).await?;
                continue;
            }

            let bytes = decrypted_child
                .get(from as usize..to as usize)
                .ok_or_else(|| {
                    HashTreeError::Store(format!("invalid chunk size for {}", to_hex(&link.hash)))
                })?;
            result.extend_from_slice(bytes);
        }

        Ok(())
    }

    /// Assemble the file under `node` into one buffer.
    async fn assemble_chunks_limited(
        &self,
        node: &TreeNode,
        max_size: Option<u64>,
        bytes_read: &mut u64,
    ) -> Result<Vec<u8>, HashTreeError> {
        let mut out = Vec::new();
        self.assemble_chunks_into(node, max_size, bytes_read, &mut out)
            .await?;
        Ok(out)
    }

    /// Append the file under `node` to `out`, depth first.
    async fn assemble_chunks_into(
        &self,
        node: &TreeNode,
        max_size: Option<u64>,
        bytes_read: &mut u64,
        out: &mut Vec<u8>,
    ) -> Result<(), HashTreeError> {
        for link in &node.links {
            let projected = (*bytes_read).saturating_add(link.size);
            Self::ensure_size_limit(max_size, projected)?;

            let child_data = self.read_link_bytes(link).await?;

            if let Some(child_node) = Self::decode_linked_file_node(link, &child_data)? {
                Box::pin(self.assemble_chunks_into(&child_node, max_size, bytes_read, out)).await?;
            } else {
                let projected = (*bytes_read).saturating_add(child_data.len() as u64);
                Self::ensure_size_limit(max_size, projected)?;
                *bytes_read = projected;
                if out.is_empty() {
                    // First leaf: reuse its allocation and reserve the declared
                    // remainder, bounded so a bogus size cannot over-allocate.
                    let declared: u64 = node.links.iter().map(|l| l.size).sum();
                    let extra = declared
                        .saturating_sub(child_data.len() as u64)
                        .min(ASSEMBLE_MAX_RESERVE_BYTES);
                    *out = child_data;
                    out.reserve(extra as usize);
                } else {
                    out.extend_from_slice(&child_data);
                }
            }
        }
        Ok(())
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
            let child_data = self.read_link_bytes(link).await?;

            if let Some(child_node) = Self::decode_linked_file_node(link, &child_data)? {
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
            if is_internal_directory_link(&node, link) {
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
            if is_internal_directory_link(&node, link) {
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

    /// List a directory reached through an existing parent link.
    /// Unlike `list_directory`, an unavailable node is not an empty directory.
    pub async fn list_directory_required(
        &self,
        cid: &Cid,
    ) -> Result<Vec<TreeEntry>, HashTreeError> {
        let node = self
            .get_directory_node(cid)
            .await?
            .ok_or_else(|| HashTreeError::MissingChunk(to_hex(&cid.hash)))?;

        let mut entries = Vec::new();
        for link in &node.links {
            if is_internal_directory_link(&node, link) {
                let sub_cid = Cid {
                    hash: link.hash,
                    key: link.key,
                };
                let sub_entries = Box::pin(self.list_directory_required(&sub_cid)).await?;
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
            .find(|l| !is_internal_directory_link(node, l) && l.name.as_deref() == Some(name))
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
            if !is_internal_directory_link(node, link) {
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

    /// Keep randomized directories randomized when rewriting their manifests.
    async fn put_edited_directory(
        &self,
        original: &Cid,
        entries: Vec<DirEntry>,
    ) -> Result<Cid, HashTreeError> {
        if original
            .key
            .is_some_and(|key| key.mode() == crate::encryption::EncryptionMode::RndV1)
        {
            let writer = Self {
                store: self.store.clone(),
                chunk_size: self.chunk_size,
                max_links: self.max_links,
                encrypted: true,
                encryption_mode: crate::encryption::EncryptionMode::RndV1,
            };
            writer.put_directory(entries).await
        } else {
            self.put_directory(entries).await
        }
    }

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
    #[allow(clippy::too_many_arguments)]
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

        let new_dir_cid = self.put_edited_directory(&dir_cid, new_entries).await?;
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

        let new_dir_cid = self.put_edited_directory(&dir_cid, new_entries).await?;
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

        let new_dir_cid = self.put_edited_directory(&dir_cid, new_entries).await?;
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

            child_cid = self
                .put_edited_directory(&parent_cid, new_parent_entries)
                .await?;
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

fn stream_put_batch_target_bytes() -> usize {
    std::env::var(STREAM_PUT_BATCH_TARGET_BYTES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_STREAM_PUT_BATCH_TARGET_BYTES)
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
