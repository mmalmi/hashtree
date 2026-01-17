//! HashTree - Simple content-addressed merkle tree
//!
//! Core principle: Every node is stored by SHA256(msgpack(node)) -> msgpack(node)
//! This enables pure KV content-addressed storage.


/// Link type - distinguishes blobs, chunked files, and directories
/// Uses small integer values for efficient MessagePack encoding
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum LinkType {
    /// Raw blob data (not a tree node)
    #[default]
    Blob = 0,
    /// Chunked file (tree node with unnamed links)
    File = 1,
    /// Directory (tree node with named links)
    Dir = 2,
}

impl LinkType {
    /// Create from u8 value
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(LinkType::Blob),
            1 => Some(LinkType::File),
            2 => Some(LinkType::Dir),
            _ => None,
        }
    }

    /// Check if this type represents a tree node (File or Dir)
    pub fn is_tree(&self) -> bool {
        matches!(self, LinkType::File | LinkType::Dir)
    }
}

/// 32-byte SHA256 hash used as content address
pub type Hash = [u8; 32];

/// Convert hash to hex string
pub fn to_hex(hash: &Hash) -> String {
    hex::encode(hash)
}

/// Convert hex string to hash
pub fn from_hex(hex_str: &str) -> Result<Hash, hex::FromHexError> {
    let bytes = hex::decode(hex_str)?;
    if bytes.len() != 32 {
        return Err(hex::FromHexError::InvalidStringLength);
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes);
    Ok(hash)
}

/// Compare two hashes for equality
pub fn hash_equals(a: &Hash, b: &Hash) -> bool {
    a == b
}

/// A link to a child node with optional metadata
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// SHA256 hash of the child node's MessagePack encoding
    pub hash: Hash,
    /// Optional name (for directory entries)
    pub name: Option<String>,
    /// Size of subtree in bytes (for efficient seeks). 0 for Dir links.
    pub size: u64,
    /// Optional decryption key for encrypted links (CHK: content hash)
    pub key: Option<[u8; 32]>,
    /// Type of content this link points to (Blob, File, or Dir)
    /// Always set explicitly - no probing needed during tree traversal
    pub link_type: LinkType,
    /// Optional metadata (for directory entries: createdAt, mimeType, thumbnail, etc.)
    pub meta: Option<std::collections::HashMap<String, serde_json::Value>>,
}

impl Link {
    pub fn new(hash: Hash) -> Self {
        Self {
            hash,
            name: None,
            size: 0,
            key: None,
            link_type: LinkType::Blob, // Default to Blob (raw data)
            meta: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    pub fn with_key(mut self, key: [u8; 32]) -> Self {
        self.key = Some(key);
        self
    }

    pub fn with_link_type(mut self, link_type: LinkType) -> Self {
        self.link_type = link_type;
        self
    }

    pub fn with_meta(mut self, meta: std::collections::HashMap<String, serde_json::Value>) -> Self {
        self.meta = Some(meta);
        self
    }

    /// Convert this link to a Cid (extracts hash and key)
    pub fn to_cid(&self) -> Cid {
        Cid {
            hash: self.hash,
            key: self.key,
        }
    }
}

/// Tree node - contains links to children
/// Stored as: SHA256(msgpack(TreeNode)) -> msgpack(TreeNode)
///
/// For directories: links have names, node_type = Dir
/// For chunked files: links are ordered chunks, node_type = File
/// For large directories: links can be other tree nodes (fanout)
#[derive(Debug, Clone, PartialEq)]
pub struct TreeNode {
    /// Type of this node (File or Dir)
    pub node_type: LinkType,
    /// Links to child nodes
    pub links: Vec<Link>,
}

impl TreeNode {
    /// Create a new tree node with specified type
    pub fn new(node_type: LinkType, links: Vec<Link>) -> Self {
        Self {
            node_type,
            links,
        }
    }

    /// Create a File node (chunked file)
    pub fn file(links: Vec<Link>) -> Self {
        Self::new(LinkType::File, links)
    }

    /// Create a Dir node (directory)
    pub fn dir(links: Vec<Link>) -> Self {
        Self::new(LinkType::Dir, links)
    }

    /// Check if this is a directory node
    pub fn is_dir(&self) -> bool {
        self.node_type == LinkType::Dir
    }

    /// Check if this is a file node
    pub fn is_file(&self) -> bool {
        self.node_type == LinkType::File
    }
}

/// Result of adding content to the tree
#[derive(Debug, Clone, PartialEq)]
pub struct PutResult {
    /// Hash of the stored node
    pub hash: Hash,
    /// Size of the stored data
    pub size: u64,
}

/// Content identifier with hash and optional encryption key
///
/// For encrypted content: contains both hash (to locate) and key (to decrypt)
/// For public content: contains only hash
///
/// Note: Size is not part of CID - it's metadata stored in Link/DirEntry
#[derive(Debug, Clone, PartialEq)]
pub struct Cid {
    /// SHA256 hash of the (possibly encrypted) content
    pub hash: Hash,
    /// Encryption key (content hash of plaintext for CHK)
    /// None for unencrypted/public content
    pub key: Option<[u8; 32]>,
}

impl Cid {
    /// Create a new CID for public (unencrypted) content
    pub fn public(hash: Hash) -> Self {
        Self { hash, key: None }
    }

    /// Create a new CID for encrypted content
    pub fn encrypted(hash: Hash, key: [u8; 32]) -> Self {
        Self { hash, key: Some(key) }
    }

    /// Check if this CID refers to encrypted content
    pub fn is_encrypted(&self) -> bool {
        self.key.is_some()
    }

    /// Parse a CID from string format
    /// Accepts "hash" or "hash:key"
    pub fn parse(s: &str) -> Result<Self, CidParseError> {
        if let Some((hash_hex, key_hex)) = s.split_once(':') {
            let hash = from_hex(hash_hex).map_err(|_| CidParseError::InvalidHash)?;
            let key = from_hex(key_hex).map_err(|_| CidParseError::InvalidKey)?;
            Ok(Self { hash, key: Some(key) })
        } else {
            let hash = from_hex(s).map_err(|_| CidParseError::InvalidHash)?;
            Ok(Self { hash, key: None })
        }
    }
}

impl std::fmt::Display for Cid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(key) = &self.key {
            write!(f, "{}:{}", to_hex(&self.hash), to_hex(key))
        } else {
            write!(f, "{}", to_hex(&self.hash))
        }
    }
}

/// Error parsing a CID string
#[derive(Debug, Clone, PartialEq)]
pub enum CidParseError {
    InvalidHash,
    InvalidKey,
}

impl std::fmt::Display for CidParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CidParseError::InvalidHash => write!(f, "invalid hash in CID"),
            CidParseError::InvalidKey => write!(f, "invalid key in CID"),
        }
    }
}

impl std::error::Error for CidParseError {}

/// Directory entry for building directory trees
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub hash: Hash,
    pub size: u64,
    pub key: Option<[u8; 32]>,
    /// Type of content this entry points to (Blob, File, or Dir)
    pub link_type: LinkType,
    /// Optional metadata (createdAt, mimeType, thumbnail, etc.)
    pub meta: Option<std::collections::HashMap<String, serde_json::Value>>,
}

impl DirEntry {
    pub fn new(name: impl Into<String>, hash: Hash) -> Self {
        Self {
            name: name.into(),
            hash,
            size: 0,
            key: None,
            link_type: LinkType::Blob, // Default to Blob (raw data)
            meta: None,
        }
    }

    /// Create from Cid (hash + optional key)
    /// Use .with_size() to set the size
    pub fn from_cid(name: impl Into<String>, cid: &Cid) -> Self {
        Self {
            name: name.into(),
            hash: cid.hash,
            size: 0,
            key: cid.key,
            link_type: LinkType::Blob, // Caller should set this appropriately
            meta: None,
        }
    }

    pub fn with_size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    pub fn with_key(mut self, key: [u8; 32]) -> Self {
        self.key = Some(key);
        self
    }

    pub fn with_link_type(mut self, link_type: LinkType) -> Self {
        self.link_type = link_type;
        self
    }

    pub fn with_meta(mut self, meta: std::collections::HashMap<String, serde_json::Value>) -> Self {
        self.meta = Some(meta);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_hex_empty() {
        let hash = [0u8; 32];
        let hex = to_hex(&hash);
        assert_eq!(hex, "0000000000000000000000000000000000000000000000000000000000000000");
    }

    #[test]
    fn test_to_hex_bytes() {
        let mut hash = [0u8; 32];
        hash[0] = 0x00;
        hash[1] = 0xff;
        hash[2] = 0x10;
        let hex = to_hex(&hash);
        assert!(hex.starts_with("00ff10"));
    }

    #[test]
    fn test_from_hex() {
        let hex = "00ff100000000000000000000000000000000000000000000000000000000000";
        let hash = from_hex(hex).unwrap();
        assert_eq!(hash[0], 0x00);
        assert_eq!(hash[1], 0xff);
        assert_eq!(hash[2], 0x10);
    }

    #[test]
    fn test_roundtrip() {
        let mut original = [0u8; 32];
        original[0] = 0;
        original[1] = 1;
        original[2] = 127;
        original[3] = 128;
        original[4] = 255;

        let hex = to_hex(&original);
        let result = from_hex(&hex).unwrap();
        assert_eq!(result, original);
    }
}
