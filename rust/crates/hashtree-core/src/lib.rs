//! HashTree - Simple content-addressed merkle tree storage
//!
//! Rust-first library for building merkle trees with content-hash addressing:
//! SHA256(content) -> content
//!
//! # Overview
//!
//! HashTree provides a simple, efficient way to build and traverse content-addressed
//! merkle trees. It uses SHA256 for hashing and MessagePack for tree node encoding.
//!
//! Content is CHK (Content Hash Key) encrypted by default, enabling deduplication
//! even for encrypted content. Use `.public()` config to disable encryption.
//!
//! # Core Concepts
//!
//! - **Blobs**: Raw data stored directly by their hash (SHA256(data) -> data)
//! - **Tree Nodes**: MessagePack-encoded nodes with links to children (SHA256(msgpack(node)) -> msgpack(node))
//! - **Links**: References to child nodes with optional name and size metadata
//! - **Cid**: Content identifier with hash + optional encryption key
//!
//! # Example
//!
//! ```rust
//! use hashtree_core::{HashTree, HashTreeConfig, MemoryStore};
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let store = Arc::new(MemoryStore::new());
//!     let tree = HashTree::new(HashTreeConfig::new(store));
//!
//!     // Store content (encrypted by default)
//!     let (cid, _size) = tree.put(b"Hello, World!").await?;
//!
//!     // Read it back
//!     let data = tree.get(&cid, None).await?;
//!     assert_eq!(data, Some(b"Hello, World!".to_vec()));
//!
//!     Ok(())
//! }
//! ```

pub mod blob_route;
pub mod builder;
pub mod codec;
pub mod crypto;
pub mod diff;
mod directory;
pub mod hash;
pub mod hashtree;
pub mod nhash;
pub mod reader;
pub mod store;
pub mod types;
pub mod visibility;

// Re-exports for convenience
pub use blob_route::{
    decode_blob_reply_header, decode_blob_request, encode_blob_reply_header, encode_blob_request,
    BlobCodecError, BlobReply, BlobReplyHeader, BlobRequest, BlobRoute, BlobRouteContext,
    StoreBlobRoute, BLOB_DEFAULT_HTL, BLOB_MAX_BYTES, BLOB_MAX_HTL, BLOB_REPLY_HEADER_BYTES,
    BLOB_REQUEST_BYTES,
};

// Main API - unified HashTree
pub use hashtree::{verify_tree as hashtree_verify_tree, HashTree, HashTreeConfig, HashTreeError};

// Constants
pub use builder::{BEP52_CHUNK_SIZE, DEFAULT_CHUNK_SIZE, DEFAULT_MAX_LINKS};

// Low-level codec
pub use codec::{
    decode_tree_node, encode_and_hash, encode_tree_node, get_node_type, is_directory_node,
    is_tree_node, try_decode_tree_node, CodecError,
};
pub use hash::{sha256, verify};

// Reader types (used by HashTree)
pub use reader::{
    verify_tree, verify_tree_integrity, ReaderError, TreeEntry, VerifyIntegrityResult,
    VerifyResult, WalkEntry,
};

// Store
pub use nhash::{
    decode as nhash_decode_any, is_nhash, nhash_decode, nhash_encode, nhash_encode_full,
    DecodeResult, NHashData, NHashError,
};
pub use store::{BufferedStore, MemoryStore, Store, StoreError};
pub use types::{
    from_hex, hash_equals, to_hex, Cid, CidParseError, DirEntry, Hash, Link, LinkType, PutResult,
    TreeNode,
};

pub use crypto::{
    content_hash, could_be_encrypted, decrypt, decrypt_chk, encrypt, encrypt_chk, encrypted_size,
    encrypted_size_chk, generate_key, key_from_hex, key_to_hex, plaintext_size, CryptoError,
    EncryptionKey,
};
pub use visibility::{xor_keys, TreeVisibility};

// Tree diff operations
pub use diff::{
    collect_hashes, collect_hashes_with_progress, tree_diff, tree_diff_streaming,
    tree_diff_with_old_hashes, DiffStats, TreeDiff,
};
