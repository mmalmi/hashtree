//! Bech32-encoded identifiers for hashtree content
//!
//! Similar to nostr's nip19 (npub, nprofile, nevent, naddr),
//! provides human-readable, copy-pasteable identifiers.
//!
//! Types:
//! - nhash: Permalink (hash + optional path + optional decrypt key)
//! - nref: Live reference (pubkey + tree + optional path + optional decrypt key)

use crate::types::Hash;
use thiserror::Error;

/// TLV type constants
mod tlv {
    /// 32-byte hash (required for nhash)
    pub const HASH: u8 = 0;
    /// 32-byte nostr pubkey (required for nref)
    pub const PUBKEY: u8 = 2;
    /// UTF-8 tree name (required for nref)
    pub const TREE_NAME: u8 = 3;
    /// UTF-8 path segment (can appear multiple times, in order)
    pub const PATH: u8 = 4;
    /// 32-byte decryption key (optional)
    pub const DECRYPT_KEY: u8 = 5;
}

/// Errors for nhash/npath encoding/decoding
#[derive(Debug, Error)]
pub enum NHashError {
    #[error("Bech32 error: {0}")]
    Bech32(String),
    #[error("Invalid prefix: expected {expected}, got {got}")]
    InvalidPrefix { expected: String, got: String },
    #[error("Invalid hash length: expected 32 bytes, got {0}")]
    InvalidHashLength(usize),
    #[error("Invalid key length: expected 32 bytes, got {0}")]
    InvalidKeyLength(usize),
    #[error("Invalid pubkey length: expected 32 bytes, got {0}")]
    InvalidPubkeyLength(usize),
    #[error("Missing required field: {0}")]
    MissingField(String),
    #[error("TLV error: {0}")]
    TlvError(String),
    #[error("UTF-8 error: {0}")]
    Utf8Error(#[from] std::string::FromUtf8Error),
    #[error("Hex error: {0}")]
    HexError(#[from] hex::FromHexError),
}

/// NHash data - permalink to content by hash
#[derive(Debug, Clone, PartialEq)]
pub struct NHashData {
    /// 32-byte merkle hash
    pub hash: Hash,
    /// Path segments (optional, e.g., ["folder", "file.txt"])
    pub path: Vec<String>,
    /// 32-byte decryption key (optional)
    pub decrypt_key: Option<[u8; 32]>,
}

/// NRef data - live reference via pubkey + tree + path
#[derive(Debug, Clone, PartialEq)]
pub struct NRefData {
    /// 32-byte nostr pubkey
    pub pubkey: [u8; 32],
    /// Tree name (e.g., "home", "photos")
    pub tree_name: String,
    /// Path segments within the tree (optional, e.g., ["folder", "file.txt"])
    pub path: Vec<String>,
    /// 32-byte decryption key (optional)
    pub decrypt_key: Option<[u8; 32]>,
}

/// Decode result
#[derive(Debug, Clone, PartialEq)]
pub enum DecodeResult {
    NHash(NHashData),
    NRef(NRefData),
}

/// Parse TLV-encoded data into a map of type -> values
fn parse_tlv(data: &[u8]) -> Result<std::collections::HashMap<u8, Vec<Vec<u8>>>, NHashError> {
    let mut result: std::collections::HashMap<u8, Vec<Vec<u8>>> = std::collections::HashMap::new();
    let mut offset = 0;

    while offset < data.len() {
        if offset + 2 > data.len() {
            return Err(NHashError::TlvError("unexpected end of data".into()));
        }
        let t = data[offset];
        let l = data[offset + 1] as usize;
        offset += 2;

        if offset + l > data.len() {
            return Err(NHashError::TlvError(format!(
                "not enough data for type {}, need {} bytes",
                t, l
            )));
        }
        let v = data[offset..offset + l].to_vec();
        offset += l;

        result.entry(t).or_default().push(v);
    }

    Ok(result)
}

/// Encode TLV data to bytes
fn encode_tlv(tlv: &std::collections::HashMap<u8, Vec<Vec<u8>>>) -> Result<Vec<u8>, NHashError> {
    let mut entries: Vec<u8> = Vec::new();

    // Process in ascending key order for consistent encoding
    let mut keys: Vec<u8> = tlv.keys().copied().collect();
    keys.sort();

    for t in keys {
        if let Some(values) = tlv.get(&t) {
            for v in values {
                if v.len() > 255 {
                    return Err(NHashError::TlvError(format!(
                        "value too long for type {}: {} bytes",
                        t,
                        v.len()
                    )));
                }
                entries.push(t);
                entries.push(v.len() as u8);
                entries.extend_from_slice(v);
            }
        }
    }

    Ok(entries)
}

/// Encode bech32 with given prefix and data
/// Uses regular bech32 (not bech32m) for compatibility with nostr nip19
fn encode_bech32(hrp: &str, data: &[u8]) -> Result<String, NHashError> {
    use bech32::{Bech32, Hrp};

    let hrp = Hrp::parse(hrp).map_err(|e| NHashError::Bech32(e.to_string()))?;
    bech32::encode::<Bech32>(hrp, data)
        .map_err(|e| NHashError::Bech32(e.to_string()))
}

/// Decode bech32 and return (hrp, data)
fn decode_bech32(s: &str) -> Result<(String, Vec<u8>), NHashError> {
    let (hrp, data) = bech32::decode(s)
        .map_err(|e| NHashError::Bech32(e.to_string()))?;

    Ok((hrp.to_string(), data))
}

// ============================================================================
// nhash - Permalink (hash + optional path + optional decrypt key)
// ============================================================================

/// Encode an nhash permalink from just a hash
pub fn nhash_encode(hash: &Hash) -> Result<String, NHashError> {
    encode_bech32("nhash", hash)
}

/// Encode an nhash permalink with optional path and decrypt key
pub fn nhash_encode_full(data: &NHashData) -> Result<String, NHashError> {
    // No path or decrypt key - simple encoding (just the hash bytes)
    if data.path.is_empty() && data.decrypt_key.is_none() {
        return encode_bech32("nhash", &data.hash);
    }

    // Has path or decrypt key - use TLV
    let mut tlv: std::collections::HashMap<u8, Vec<Vec<u8>>> = std::collections::HashMap::new();
    tlv.insert(tlv::HASH, vec![data.hash.to_vec()]);

    if !data.path.is_empty() {
        tlv.insert(
            tlv::PATH,
            data.path.iter().map(|p| p.as_bytes().to_vec()).collect(),
        );
    }

    if let Some(key) = &data.decrypt_key {
        tlv.insert(tlv::DECRYPT_KEY, vec![key.to_vec()]);
    }

    encode_bech32("nhash", &encode_tlv(&tlv)?)
}

/// Decode an nhash string
pub fn nhash_decode(code: &str) -> Result<NHashData, NHashError> {
    // Strip optional prefix
    let code = code.strip_prefix("hashtree:").unwrap_or(code);

    let (prefix, data) = decode_bech32(code)?;

    if prefix != "nhash" {
        return Err(NHashError::InvalidPrefix {
            expected: "nhash".into(),
            got: prefix,
        });
    }

    // Simple 32-byte hash (no TLV)
    if data.len() == 32 {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data);
        return Ok(NHashData {
            hash,
            path: Vec::new(),
            decrypt_key: None,
        });
    }

    // Parse TLV
    let tlv = parse_tlv(&data)?;

    let hash_bytes = tlv
        .get(&tlv::HASH)
        .and_then(|v| v.first())
        .ok_or_else(|| NHashError::MissingField("hash".into()))?;

    if hash_bytes.len() != 32 {
        return Err(NHashError::InvalidHashLength(hash_bytes.len()));
    }

    let mut hash = [0u8; 32];
    hash.copy_from_slice(hash_bytes);

    // Path segments
    let path = if let Some(paths) = tlv.get(&tlv::PATH) {
        paths
            .iter()
            .map(|p| String::from_utf8(p.clone()))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    let decrypt_key = if let Some(keys) = tlv.get(&tlv::DECRYPT_KEY) {
        if let Some(key_bytes) = keys.first() {
            if key_bytes.len() != 32 {
                return Err(NHashError::InvalidKeyLength(key_bytes.len()));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(key_bytes);
            Some(key)
        } else {
            None
        }
    } else {
        None
    };

    Ok(NHashData { hash, path, decrypt_key })
}

// ============================================================================
// nref - Live reference (pubkey + tree + optional path + optional decrypt key)
// ============================================================================

/// Encode an nref live reference
pub fn nref_encode(data: &NRefData) -> Result<String, NHashError> {
    let mut tlv: std::collections::HashMap<u8, Vec<Vec<u8>>> = std::collections::HashMap::new();

    tlv.insert(tlv::PUBKEY, vec![data.pubkey.to_vec()]);
    tlv.insert(tlv::TREE_NAME, vec![data.tree_name.as_bytes().to_vec()]);

    if !data.path.is_empty() {
        tlv.insert(
            tlv::PATH,
            data.path.iter().map(|p| p.as_bytes().to_vec()).collect(),
        );
    }

    if let Some(key) = &data.decrypt_key {
        tlv.insert(tlv::DECRYPT_KEY, vec![key.to_vec()]);
    }

    encode_bech32("nref", &encode_tlv(&tlv)?)
}

/// Decode an nref string
pub fn nref_decode(code: &str) -> Result<NRefData, NHashError> {
    // Strip optional prefix
    let code = code.strip_prefix("hashtree:").unwrap_or(code);

    let (prefix, data) = decode_bech32(code)?;

    if prefix != "nref" {
        return Err(NHashError::InvalidPrefix {
            expected: "nref".into(),
            got: prefix,
        });
    }

    let tlv = parse_tlv(&data)?;

    // Pubkey
    let pubkey_bytes = tlv
        .get(&tlv::PUBKEY)
        .and_then(|v| v.first())
        .ok_or_else(|| NHashError::MissingField("pubkey".into()))?;

    if pubkey_bytes.len() != 32 {
        return Err(NHashError::InvalidPubkeyLength(pubkey_bytes.len()));
    }

    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(pubkey_bytes);

    // Tree name
    let tree_name_bytes = tlv
        .get(&tlv::TREE_NAME)
        .and_then(|v| v.first())
        .ok_or_else(|| NHashError::MissingField("tree_name".into()))?;

    let tree_name = String::from_utf8(tree_name_bytes.clone())?;

    // Path segments
    let path = if let Some(paths) = tlv.get(&tlv::PATH) {
        paths
            .iter()
            .map(|p| String::from_utf8(p.clone()))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    // Decrypt key
    let decrypt_key = if let Some(keys) = tlv.get(&tlv::DECRYPT_KEY) {
        if let Some(key_bytes) = keys.first() {
            if key_bytes.len() != 32 {
                return Err(NHashError::InvalidKeyLength(key_bytes.len()));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(key_bytes);
            Some(key)
        } else {
            None
        }
    } else {
        None
    };

    Ok(NRefData {
        pubkey,
        tree_name,
        path,
        decrypt_key,
    })
}

// ============================================================================
// Generic decode
// ============================================================================

/// Decode any nhash or nref string
pub fn decode(code: &str) -> Result<DecodeResult, NHashError> {
    let code = code.strip_prefix("hashtree:").unwrap_or(code);

    if code.starts_with("nhash1") {
        return Ok(DecodeResult::NHash(nhash_decode(code)?));
    }
    if code.starts_with("nref1") {
        return Ok(DecodeResult::NRef(nref_decode(code)?));
    }

    Err(NHashError::InvalidPrefix {
        expected: "nhash1 or nref1".into(),
        got: code.chars().take(10).collect(),
    })
}

// ============================================================================
// Type guards
// ============================================================================

/// Check if a string is an nhash
pub fn is_nhash(value: &str) -> bool {
    value.starts_with("nhash1")
}

/// Check if a string is an nref
pub fn is_nref(value: &str) -> bool {
    value.starts_with("nref1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nhash_simple() {
        let hash: Hash = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
        ];

        let encoded = nhash_encode(&hash).unwrap();
        assert!(encoded.starts_with("nhash1"));

        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, hash);
        assert!(decoded.path.is_empty());
        assert!(decoded.decrypt_key.is_none());
    }

    #[test]
    fn test_nhash_with_path() {
        let hash: Hash = [0xaa; 32];

        let data = NHashData {
            hash,
            path: vec!["folder".into(), "file.txt".into()],
            decrypt_key: None,
        };

        let encoded = nhash_encode_full(&data).unwrap();
        assert!(encoded.starts_with("nhash1"));

        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, hash);
        assert_eq!(decoded.path, vec!["folder", "file.txt"]);
        assert!(decoded.decrypt_key.is_none());
    }

    #[test]
    fn test_nhash_with_key() {
        let hash: Hash = [0xaa; 32];
        let key: [u8; 32] = [0xbb; 32];

        let data = NHashData {
            hash,
            path: vec![],
            decrypt_key: Some(key),
        };

        let encoded = nhash_encode_full(&data).unwrap();
        assert!(encoded.starts_with("nhash1"));

        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, hash);
        assert!(decoded.path.is_empty());
        assert_eq!(decoded.decrypt_key, Some(key));
    }

    #[test]
    fn test_nhash_with_path_and_key() {
        let hash: Hash = [0xaa; 32];
        let key: [u8; 32] = [0xbb; 32];

        let data = NHashData {
            hash,
            path: vec!["docs".into()],
            decrypt_key: Some(key),
        };

        let encoded = nhash_encode_full(&data).unwrap();
        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, hash);
        assert_eq!(decoded.path, vec!["docs"]);
        assert_eq!(decoded.decrypt_key, Some(key));
    }

    #[test]
    fn test_nref_simple() {
        let pubkey: [u8; 32] = [0xcc; 32];
        let data = NRefData {
            pubkey,
            tree_name: "home".into(),
            path: vec![],
            decrypt_key: None,
        };

        let encoded = nref_encode(&data).unwrap();
        assert!(encoded.starts_with("nref1"));

        let decoded = nref_decode(&encoded).unwrap();
        assert_eq!(decoded.pubkey, pubkey);
        assert_eq!(decoded.tree_name, "home");
        assert!(decoded.path.is_empty());
        assert!(decoded.decrypt_key.is_none());
    }

    #[test]
    fn test_nref_with_path_and_key() {
        let pubkey: [u8; 32] = [0xdd; 32];
        let key: [u8; 32] = [0xee; 32];

        let data = NRefData {
            pubkey,
            tree_name: "photos".into(),
            path: vec!["vacation".into(), "beach.jpg".into()],
            decrypt_key: Some(key),
        };

        let encoded = nref_encode(&data).unwrap();
        assert!(encoded.starts_with("nref1"));

        let decoded = nref_decode(&encoded).unwrap();
        assert_eq!(decoded.pubkey, pubkey);
        assert_eq!(decoded.tree_name, "photos");
        assert_eq!(decoded.path, vec!["vacation", "beach.jpg"]);
        assert_eq!(decoded.decrypt_key, Some(key));
    }

    #[test]
    fn test_decode_generic() {
        let hash: Hash = [0x11; 32];
        let nhash = nhash_encode(&hash).unwrap();

        match decode(&nhash).unwrap() {
            DecodeResult::NHash(data) => assert_eq!(data.hash, hash),
            _ => panic!("expected NHash"),
        }

        let pubkey: [u8; 32] = [0x22; 32];
        let nref_data = NRefData {
            pubkey,
            tree_name: "test".into(),
            path: vec![],
            decrypt_key: None,
        };
        let nref = nref_encode(&nref_data).unwrap();

        match decode(&nref).unwrap() {
            DecodeResult::NRef(data) => {
                assert_eq!(data.pubkey, pubkey);
                assert_eq!(data.tree_name, "test");
            }
            _ => panic!("expected NRef"),
        }
    }

    #[test]
    fn test_is_nhash() {
        assert!(is_nhash("nhash1abc"));
        assert!(!is_nhash("nref1abc"));
        assert!(!is_nhash("npub1abc"));
    }

    #[test]
    fn test_is_nref() {
        assert!(is_nref("nref1abc"));
        assert!(!is_nref("nhash1abc"));
        assert!(!is_nref("npub1abc"));
    }
}
