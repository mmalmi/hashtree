//! MessagePack encoding/decoding for tree nodes
//!
//! Blobs are stored raw (not wrapped) for efficiency.
//! Tree nodes are MessagePack-encoded.
//!
//! **Determinism:** Unlike CBOR, MessagePack doesn't have a built-in canonical encoding.
//! We ensure deterministic output by:
//! 1. Using fixed struct field order (Rust declaration order via serde)
//! 2. Converting HashMap metadata to BTreeMap before encoding (sorted keys)
//!
//! Format uses short keys for compact encoding:
//! - t: type (1 = File, 2 = Dir) - node type
//! - l: links array
//! - h: hash (in link)
//! - t: type (in link, 0 = Blob, 1 = File, 2 = Dir)
//! - n: name (in link, optional)
//! - s: size (in link)
//! - m: metadata (optional)

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::hash::sha256;
use crate::types::{Hash, Link, LinkType, TreeNode};

/// Error type for codec operations
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("Invalid node type: {0}")]
    InvalidNodeType(u8),
    #[error("Missing required field: {0}")]
    MissingField(&'static str),
    #[error("Invalid field type for {0}")]
    InvalidFieldType(&'static str),
    #[error("MessagePack encoding error: {0}")]
    MsgpackEncode(String),
    #[error("MessagePack decoding error: {0}")]
    MsgpackDecode(String),
    #[error("Invalid hash length: expected 32, got {0}")]
    InvalidHashLength(usize),
}

/// Wire format for a link (compact keys)
/// Fields are ordered alphabetically for canonical encoding: h, k?, m?, n?, s, t
#[derive(Serialize, Deserialize)]
struct WireLink {
    /// Hash (required) - use serde_bytes for proper MessagePack binary encoding
    #[serde(with = "serde_bytes")]
    h: Vec<u8>,
    /// Encryption key (optional) - use serde_bytes for proper MessagePack binary encoding
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_bytes"
    )]
    k: Option<Vec<u8>>,
    /// Metadata (optional) - uses BTreeMap for deterministic key ordering
    #[serde(skip_serializing_if = "Option::is_none")]
    m: Option<BTreeMap<String, serde_json::Value>>,
    /// Name (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<String>,
    /// Size (required)
    s: u64,
    /// Link type (0 = Blob, 1 = File, 2 = Dir)
    #[serde(default)]
    t: u8,
}

/// Helper module for optional bytes serialization
mod option_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(data: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match data {
            Some(bytes) => serde_bytes::serialize(bytes, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<serde_bytes::ByteBuf>::deserialize(deserializer)
            .map(|opt| opt.map(|bb| bb.into_vec()))
    }
}

/// Wire format for a tree node (compact keys)
/// Fields are ordered alphabetically for canonical encoding: l, t
#[derive(Serialize, Deserialize)]
struct WireTreeNode {
    /// Links
    l: Vec<WireLink>,
    /// Type (1 = File, 2 = Dir)
    t: u8,
}

/// Encode a tree node to MessagePack
pub fn encode_tree_node(node: &TreeNode) -> Result<Vec<u8>, CodecError> {
    let wire = WireTreeNode {
        t: node.node_type as u8,
        l: node
            .links
            .iter()
            .map(|link| {
                // Convert HashMap to BTreeMap for deterministic key ordering
                let sorted_meta = link.meta.as_ref().map(|m| m.iter().collect::<BTreeMap<_, _>>());
                WireLink {
                    h: link.hash.to_vec(),
                    t: link.link_type as u8,
                    n: link.name.clone(),
                    s: link.size,
                    k: link.key.map(|k| k.to_vec()),
                    m: sorted_meta.map(|m| m.into_iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
                }
            })
            .collect(),
    };

    rmp_serde::to_vec_named(&wire).map_err(|e| CodecError::MsgpackEncode(e.to_string()))
}

/// Decode MessagePack to a tree node
pub fn decode_tree_node(data: &[u8]) -> Result<TreeNode, CodecError> {
    let wire: WireTreeNode =
        rmp_serde::from_slice(data).map_err(|e| CodecError::MsgpackDecode(e.to_string()))?;

    // Validate node type (must be File=1 or Dir=2)
    let node_type = LinkType::from_u8(wire.t)
        .filter(|t| t.is_tree())
        .ok_or(CodecError::InvalidNodeType(wire.t))?;

    let mut links = Vec::with_capacity(wire.l.len());
    for wl in wire.l {
        if wl.h.len() != 32 {
            return Err(CodecError::InvalidHashLength(wl.h.len()));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&wl.h);

        let key = match wl.k {
            Some(k) if k.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&k);
                Some(key)
            }
            _ => None,
        };

        // Link type defaults to Blob if not valid
        let link_type = LinkType::from_u8(wl.t).unwrap_or(LinkType::Blob);

        // Convert BTreeMap back to HashMap for the public API
        let meta = wl.m.map(|m| m.into_iter().collect::<HashMap<_, _>>());

        links.push(Link {
            hash,
            name: wl.n,
            size: wl.s,
            key,
            link_type,
            meta,
        });
    }

    Ok(TreeNode {
        node_type,
        links,
    })
}

/// Encode a tree node and compute its hash
pub fn encode_and_hash(node: &TreeNode) -> Result<(Vec<u8>, Hash), CodecError> {
    let data = encode_tree_node(node)?;
    let hash = sha256(&data);
    Ok((data, hash))
}

/// Try to decode data as a tree node
/// Returns Some(TreeNode) if valid tree node, None otherwise
/// This is preferred over is_tree_node() to avoid double decoding
pub fn try_decode_tree_node(data: &[u8]) -> Option<TreeNode> {
    decode_tree_node(data).ok()
}

/// Get the type of data (Blob, File, or Dir)
/// Returns LinkType::Blob for raw blobs that aren't tree nodes
pub fn get_node_type(data: &[u8]) -> LinkType {
    try_decode_tree_node(data)
        .map(|n| n.node_type)
        .unwrap_or(LinkType::Blob)
}

/// Check if data is a MessagePack-encoded tree node (vs raw blob)
/// Tree nodes decode successfully with type = File or Dir
/// Note: Prefer try_decode_tree_node() to avoid double decoding
pub fn is_tree_node(data: &[u8]) -> bool {
    try_decode_tree_node(data).is_some()
}

/// Check if data is a directory tree node (node_type == Dir)
/// Note: Prefer try_decode_tree_node() to avoid double decoding
pub fn is_directory_node(data: &[u8]) -> bool {
    try_decode_tree_node(data)
        .map(|n| n.node_type == LinkType::Dir)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::to_hex;

    #[test]
    fn test_encode_decode_empty_tree() {
        let node = TreeNode::dir(vec![]);

        let encoded = encode_tree_node(&node).unwrap();
        let decoded = decode_tree_node(&encoded).unwrap();

        assert_eq!(decoded.links.len(), 0);
        assert_eq!(decoded.node_type, LinkType::Dir);
    }

    #[test]
    fn test_encode_decode_tree_with_links() {
        let hash1 = [1u8; 32];
        let hash2 = [2u8; 32];

        let node = TreeNode::dir(vec![
            Link {
                hash: hash1,
                name: Some("file1.txt".to_string()),
                size: 100,
                key: None,
                link_type: LinkType::Blob,
                meta: None,
            },
            Link {
                hash: hash2,
                name: Some("dir".to_string()),
                size: 0,
                key: None,
                link_type: LinkType::Dir,
                meta: None,
            },
        ]);

        let encoded = encode_tree_node(&node).unwrap();
        let decoded = decode_tree_node(&encoded).unwrap();

        assert_eq!(decoded.links.len(), 2);
        assert_eq!(decoded.links[0].name, Some("file1.txt".to_string()));
        assert_eq!(decoded.links[0].size, 100);
        assert_eq!(decoded.links[0].link_type, LinkType::Blob);
        assert_eq!(to_hex(&decoded.links[0].hash), to_hex(&hash1));
        assert_eq!(decoded.links[1].name, Some("dir".to_string()));
        assert_eq!(decoded.links[1].link_type, LinkType::Dir);
    }

    #[test]
    fn test_preserve_link_meta() {
        let mut meta = HashMap::new();
        meta.insert("createdAt".to_string(), serde_json::json!(1234567890));
        meta.insert("mimeType".to_string(), serde_json::json!("image/png"));

        let node = TreeNode::dir(vec![
            Link::new([1u8; 32])
                .with_name("file.png")
                .with_size(1024)
                .with_meta(meta.clone())
        ]);

        let encoded = encode_tree_node(&node).unwrap();
        let decoded = decode_tree_node(&encoded).unwrap();

        assert!(decoded.links[0].meta.is_some());
        let m = decoded.links[0].meta.as_ref().unwrap();
        assert_eq!(m.get("createdAt"), Some(&serde_json::json!(1234567890)));
        assert_eq!(m.get("mimeType"), Some(&serde_json::json!("image/png")));
    }

    #[test]
    fn test_links_without_optional_fields() {
        let hash = [42u8; 32];

        let node = TreeNode::file(vec![Link::new(hash)]);

        let encoded = encode_tree_node(&node).unwrap();
        let decoded = decode_tree_node(&encoded).unwrap();

        assert_eq!(decoded.links[0].name, None);
        assert_eq!(decoded.links[0].size, 0);
        assert_eq!(decoded.links[0].link_type, LinkType::Blob);
        assert_eq!(decoded.links[0].meta, None);
        assert_eq!(to_hex(&decoded.links[0].hash), to_hex(&hash));
    }

    #[test]
    fn test_encode_and_hash() {
        let node = TreeNode::dir(vec![]);

        let (data, hash) = encode_and_hash(&node).unwrap();
        let expected_hash = sha256(&data);

        assert_eq!(to_hex(&hash), to_hex(&expected_hash));
    }

    #[test]
    fn test_encode_and_hash_consistent() {
        let node = TreeNode::dir(vec![Link {
            hash: [1u8; 32],
            name: Some("test".to_string()),
            size: 100,
            key: None,
            link_type: LinkType::Blob,
            meta: None,
        }]);

        let (_, hash1) = encode_and_hash(&node).unwrap();
        let (_, hash2) = encode_and_hash(&node).unwrap();

        assert_eq!(to_hex(&hash1), to_hex(&hash2));
    }

    #[test]
    fn test_is_tree_node() {
        let node = TreeNode::dir(vec![]);
        let encoded = encode_tree_node(&node).unwrap();

        assert!(is_tree_node(&encoded));
    }

    #[test]
    fn test_is_tree_node_raw_blob() {
        let blob = vec![1u8, 2, 3, 4, 5];
        assert!(!is_tree_node(&blob));
    }

    #[test]
    fn test_is_tree_node_invalid_msgpack() {
        let invalid = vec![255u8, 255, 255];
        assert!(!is_tree_node(&invalid));
    }

    #[test]
    fn test_is_directory_node() {
        let node = TreeNode::dir(vec![Link {
            hash: [1u8; 32],
            name: Some("file.txt".to_string()),
            size: 100,
            key: None,
            link_type: LinkType::Blob,
            meta: None,
        }]);
        let encoded = encode_tree_node(&node).unwrap();

        assert!(is_directory_node(&encoded));
    }

    #[test]
    fn test_is_directory_node_empty() {
        let node = TreeNode::dir(vec![]);
        let encoded = encode_tree_node(&node).unwrap();

        assert!(is_directory_node(&encoded));
    }

    #[test]
    fn test_is_not_directory_node() {
        // A File node is not a directory
        let node = TreeNode::file(vec![Link::new([1u8; 32])]);
        let encoded = encode_tree_node(&node).unwrap();

        assert!(!is_directory_node(&encoded));
    }

    #[test]
    fn test_encrypted_link_roundtrip() {
        let hash = [1u8; 32];
        let key = [2u8; 32];

        let node = TreeNode::dir(vec![Link {
            hash,
            name: Some("encrypted.dat".to_string()),
            size: 1024,
            key: Some(key),
            link_type: LinkType::Blob,
            meta: None,
        }]);

        let encoded = encode_tree_node(&node).unwrap();
        let decoded = decode_tree_node(&encoded).unwrap();

        assert_eq!(decoded.links[0].key, Some(key));
    }

    #[test]
    fn test_encoding_determinism() {
        // Test that encoding is deterministic across multiple calls
        // This is critical for content-addressed storage where hash must be stable
        let hash = [42u8; 32];

        let node = TreeNode::dir(vec![
            Link {
                hash,
                name: Some("file.txt".to_string()),
                size: 100,
                key: None,
                link_type: LinkType::Blob,
                meta: None,
            },
        ]);

        // Encode multiple times and verify identical output
        let encoded1 = encode_tree_node(&node).unwrap();
        let encoded2 = encode_tree_node(&node).unwrap();
        let encoded3 = encode_tree_node(&node).unwrap();

        assert_eq!(encoded1, encoded2, "Encoding should be deterministic");
        assert_eq!(encoded2, encoded3, "Encoding should be deterministic");
    }

    #[test]
    fn test_link_meta_determinism() {
        // Test that link meta encoding is deterministic regardless of HashMap insertion order
        // We use BTreeMap internally to ensure sorted keys
        let hash = [1u8; 32];

        // Create meta with keys in different orders
        let mut meta1 = HashMap::new();
        meta1.insert("zebra".to_string(), serde_json::json!("last"));
        meta1.insert("alpha".to_string(), serde_json::json!("first"));
        meta1.insert("middle".to_string(), serde_json::json!("mid"));

        let mut meta2 = HashMap::new();
        meta2.insert("alpha".to_string(), serde_json::json!("first"));
        meta2.insert("middle".to_string(), serde_json::json!("mid"));
        meta2.insert("zebra".to_string(), serde_json::json!("last"));

        let node1 = TreeNode::dir(vec![Link::new(hash).with_name("file").with_size(100).with_meta(meta1)]);
        let node2 = TreeNode::dir(vec![Link::new(hash).with_name("file").with_size(100).with_meta(meta2)]);

        let encoded1 = encode_tree_node(&node1).unwrap();
        let encoded2 = encode_tree_node(&node2).unwrap();

        // Both should produce identical bytes (keys sorted alphabetically)
        assert_eq!(
            encoded1, encoded2,
            "Link meta encoding should be deterministic regardless of insertion order"
        );

        // Verify the hash is also identical
        let hash1 = crate::hash::sha256(&encoded1);
        let hash2 = crate::hash::sha256(&encoded2);
        assert_eq!(hash1, hash2, "Hashes should match for identical content");
    }

    #[test]
    fn test_get_node_type() {
        let dir_node = TreeNode::dir(vec![]);
        let dir_encoded = encode_tree_node(&dir_node).unwrap();
        assert_eq!(get_node_type(&dir_encoded), LinkType::Dir);

        let file_node = TreeNode::file(vec![]);
        let file_encoded = encode_tree_node(&file_node).unwrap();
        assert_eq!(get_node_type(&file_encoded), LinkType::File);

        // Raw blob returns Blob type
        let blob = vec![1u8, 2, 3, 4, 5];
        assert_eq!(get_node_type(&blob), LinkType::Blob);
    }

    #[test]
    fn test_link_type_roundtrip() {
        let node = TreeNode::dir(vec![
            Link::new([1u8; 32]).with_link_type(LinkType::Blob),
            Link::new([2u8; 32]).with_link_type(LinkType::File),
            Link::new([3u8; 32]).with_link_type(LinkType::Dir),
        ]);

        let encoded = encode_tree_node(&node).unwrap();
        let decoded = decode_tree_node(&encoded).unwrap();

        assert_eq!(decoded.links[0].link_type, LinkType::Blob);
        assert_eq!(decoded.links[1].link_type, LinkType::File);
        assert_eq!(decoded.links[2].link_type, LinkType::Dir);
    }
}
