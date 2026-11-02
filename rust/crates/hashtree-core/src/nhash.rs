//! Bech32-encoded identifiers for hashtree content
//!
//! Similar to nostr's nip19 (npub, nprofile, nevent, naddr),
//! provides human-readable, copy-pasteable identifiers.
//!
//! Types:
//! - nhash: Permalink (hash + optional decrypt key)

use crate::encryption::ObjectKey;
use crate::types::Hash;
use thiserror::Error;

/// TLV type constants
mod tlv {
    /// 32-byte hash (required for nhash)
    pub const HASH: u8 = 0;
    /// UTF-8 path segment (legacy; ignored by decoder)
    pub const PATH: u8 = 4;
    /// 32-byte decryption key (optional)
    pub const DECRYPT_KEY: u8 = 5;
}

/// Errors for nhash encoding/decoding
#[derive(Debug, Error)]
pub enum NHashError {
    #[error("Bech32 error: {0}")]
    Bech32(String),
    #[error("Invalid prefix: expected {expected}, got {got}")]
    InvalidPrefix { expected: String, got: String },
    #[error("Invalid hash length: expected 32 bytes, got {0}")]
    InvalidHashLength(usize),
    #[error("Invalid key length: expected 32 or 33 bytes, got {0}")]
    InvalidKeyLength(usize),
    #[error("Missing required field: {0}")]
    MissingField(String),
    #[error("TLV error: {0}")]
    TlvError(String),
    #[error("Hex error: {0}")]
    HexError(#[from] hex::FromHexError),
}

/// NHash data - permalink to content by hash
#[derive(Debug, Clone, PartialEq)]
pub struct NHashData {
    /// 32-byte merkle hash
    pub hash: Hash,
    /// 32-byte decryption key (optional)
    pub decrypt_key: Option<ObjectKey>,
}

/// Decode result
#[derive(Debug, Clone, PartialEq)]
pub enum DecodeResult {
    NHash(NHashData),
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
    bech32::encode::<Bech32>(hrp, data).map_err(|e| NHashError::Bech32(e.to_string()))
}

/// Decode bech32 and return (hrp, data)
fn decode_bech32(s: &str) -> Result<(String, Vec<u8>), NHashError> {
    let (hrp, data) = bech32::decode(s).map_err(|e| NHashError::Bech32(e.to_string()))?;

    Ok((hrp.to_string(), data))
}

// ============================================================================
// nhash - Permalink (hash + optional decrypt key)
// ============================================================================

/// Encode an nhash permalink from just a hash
pub fn nhash_encode(hash: &Hash) -> Result<String, NHashError> {
    nhash_encode_full(&NHashData {
        hash: *hash,
        decrypt_key: None,
    })
}

/// Encode an nhash permalink with optional decrypt key
///
/// Encoding is always TLV (canonical):
/// - HASH tag is always present
/// - DECRYPT_KEY tag is optional
pub fn nhash_encode_full(data: &NHashData) -> Result<String, NHashError> {
    let mut tlv: std::collections::HashMap<u8, Vec<Vec<u8>>> = std::collections::HashMap::new();
    tlv.insert(tlv::HASH, vec![data.hash.to_vec()]);

    if let Some(key) = &data.decrypt_key {
        tlv.insert(tlv::DECRYPT_KEY, vec![key.as_bytes().to_vec()]);
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

    // Legacy simple 32-byte hash (no TLV)
    if data.len() == 32 {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data);
        return Ok(NHashData {
            hash,
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

    // Legacy PATH tags are ignored. Path traversal uses nhash/... URL segments.
    let _ = tlv.get(&tlv::PATH);

    let decrypt_key = if let Some(keys) = tlv.get(&tlv::DECRYPT_KEY) {
        if let Some(key_bytes) = keys.first() {
            Some(
                ObjectKey::from_bytes(key_bytes)
                    .map_err(|e| NHashError::TlvError(e.to_string()))?,
            )
        } else {
            None
        }
    } else {
        None
    };

    Ok(NHashData { hash, decrypt_key })
}

// ============================================================================
// Generic decode
// ============================================================================

/// Decode an nhash string, returning a tagged decode result.
pub fn decode(code: &str) -> Result<DecodeResult, NHashError> {
    let code = code.strip_prefix("hashtree:").unwrap_or(code);

    if code.starts_with("nhash1") {
        return Ok(DecodeResult::NHash(nhash_decode(code)?));
    }

    Err(NHashError::InvalidPrefix {
        expected: "nhash1".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nhash_hash_only_uses_tlv_encoding() {
        let hash: Hash = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20,
        ];

        let encoded = nhash_encode(&hash).unwrap();
        assert!(encoded.starts_with("nhash1"));

        let (_prefix, payload) = decode_bech32(&encoded).unwrap();
        assert_ne!(payload.len(), 32, "hash-only nhash must use TLV payload");

        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, hash);
        assert!(decoded.decrypt_key.is_none());
    }

    #[test]
    fn test_nhash_decode_legacy_simple_hash_payload() {
        let hash: Hash = [0x42; 32];
        let encoded = encode_bech32("nhash", &hash).unwrap();

        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, hash);
        assert!(decoded.decrypt_key.is_none());
    }

    #[test]
    fn test_nhash_with_key() {
        let hash: Hash = [0xaa; 32];
        let key: [u8; 32] = [0xbb; 32];

        let data = NHashData {
            hash,
            decrypt_key: Some(key.into()),
        };

        let encoded = nhash_encode_full(&data).unwrap();
        assert!(encoded.starts_with("nhash1"));

        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, hash);
        assert_eq!(decoded.decrypt_key, Some(key.into()));
    }

    #[test]
    fn test_nhash_encode_full_matches_nhash_encode_when_no_key() {
        let hash: Hash = [0xaa; 32];
        let encoded_a = nhash_encode(&hash).unwrap();
        let encoded_b = nhash_encode_full(&NHashData {
            hash,
            decrypt_key: None,
        })
        .unwrap();
        assert_eq!(encoded_a, encoded_b);
    }

    #[test]
    fn test_nhash_decode_ignores_embedded_path_tags() {
        let mut tlv: std::collections::HashMap<u8, Vec<Vec<u8>>> = std::collections::HashMap::new();
        tlv.insert(tlv::HASH, vec![vec![0x11; 32]]);
        tlv.insert(tlv::PATH, vec![b"nested".to_vec(), b"file.txt".to_vec()]);

        let payload = encode_tlv(&tlv).unwrap();
        let encoded = encode_bech32("nhash", &payload).unwrap();

        let decoded = nhash_decode(&encoded).unwrap();
        assert_eq!(decoded.hash, [0x11; 32]);
        assert!(decoded.decrypt_key.is_none());
    }

    #[test]
    fn test_decode_generic() {
        let hash: Hash = [0x11; 32];
        let nhash = nhash_encode(&hash).unwrap();

        match decode(&nhash).unwrap() {
            DecodeResult::NHash(data) => assert_eq!(data.hash, hash),
        }
    }

    #[test]
    fn test_decode_rejects_non_nhash_prefix() {
        let err = decode("nref1abc").unwrap_err();
        match err {
            NHashError::InvalidPrefix { expected, .. } => assert_eq!(expected, "nhash1"),
            _ => panic!("expected InvalidPrefix"),
        }
    }

    #[test]
    fn test_is_nhash() {
        assert!(is_nhash("nhash1abc"));
        assert!(!is_nhash("nref1abc"));
        assert!(!is_nhash("npub1abc"));
    }
}
