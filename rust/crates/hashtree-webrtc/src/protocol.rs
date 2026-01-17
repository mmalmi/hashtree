//! Wire protocol for hashtree WebRTC data exchange
//!
//! Compatible with hashtree-ts wire format:
//! - Request:  [0x00][msgpack: {h: bytes32, htl?: u8}]
//! - Response: [0x01][msgpack: {h: bytes32, d: bytes, i?: u32, n?: u32}]
//!
//! Fragmented responses include `i` (index) and `n` (total), unfragmented omit them.

use hashtree_core::Hash;
use serde::{Deserialize, Serialize};

/// Message type bytes (prefix before MessagePack body)
pub const MSG_TYPE_REQUEST: u8 = 0x00;
pub const MSG_TYPE_RESPONSE: u8 = 0x01;

/// Fragment size for large data (32KB - safe limit for WebRTC)
pub const FRAGMENT_SIZE: usize = 32 * 1024;

/// Data request message body
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataRequest {
    /// 32-byte hash
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    /// Hops To Live (optional, defaults to MAX_HTL)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub htl: Option<u8>,
}

/// Data response message body
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataResponse {
    /// 32-byte hash
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    /// Data (fragment or full)
    #[serde(with = "serde_bytes")]
    pub d: Vec<u8>,
    /// Fragment index (0-based), absent = unfragmented
    #[serde(skip_serializing_if = "Option::is_none")]
    pub i: Option<u32>,
    /// Total fragments, absent = unfragmented
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
}

/// Parsed data message
#[derive(Debug, Clone)]
pub enum DataMessage {
    Request(DataRequest),
    Response(DataResponse),
}

/// Encode a request message to wire format
/// Uses named/map encoding for compatibility with hashtree-ts and to support optional fields
pub fn encode_request(req: &DataRequest) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(req).expect("Failed to encode request");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_REQUEST);
    result.extend(body);
    result
}

/// Encode a response message to wire format
/// Uses named/map encoding for compatibility with hashtree-ts and to support optional fields
pub fn encode_response(res: &DataResponse) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(res).expect("Failed to encode response");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_RESPONSE);
    result.extend(body);
    result
}

/// Parse a wire format message
pub fn parse_message(data: &[u8]) -> Option<DataMessage> {
    if data.len() < 2 {
        return None;
    }

    let msg_type = data[0];
    let body = &data[1..];

    match msg_type {
        MSG_TYPE_REQUEST => {
            rmp_serde::from_slice::<DataRequest>(body)
                .ok()
                .map(DataMessage::Request)
        }
        MSG_TYPE_RESPONSE => {
            rmp_serde::from_slice::<DataResponse>(body)
                .ok()
                .map(DataMessage::Response)
        }
        _ => None,
    }
}

/// Create a request
pub fn create_request(hash: &Hash, htl: u8) -> DataRequest {
    DataRequest {
        h: hash.to_vec(),
        htl: Some(htl),
    }
}

/// Create an unfragmented response
pub fn create_response(hash: &Hash, data: Vec<u8>) -> DataResponse {
    DataResponse {
        h: hash.to_vec(),
        d: data,
        i: None,
        n: None,
    }
}

/// Create a fragmented response
pub fn create_fragment_response(hash: &Hash, data: Vec<u8>, index: u32, total: u32) -> DataResponse {
    DataResponse {
        h: hash.to_vec(),
        d: data,
        i: Some(index),
        n: Some(total),
    }
}

/// Check if a response is fragmented
pub fn is_fragmented(res: &DataResponse) -> bool {
    res.i.is_some() && res.n.is_some()
}

/// Convert hash bytes to hex string for use as map key
pub fn hash_to_key(hash: &[u8]) -> String {
    hex::encode(hash)
}

/// Convert Hash to bytes
pub fn hash_to_bytes(hash: &Hash) -> Vec<u8> {
    hash.to_vec()
}

/// Convert bytes to Hash
pub fn bytes_to_hash(bytes: &[u8]) -> Option<Hash> {
    if bytes.len() == 32 {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(bytes);
        Some(hash)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_request() {
        let hash = [0xab; 32];
        let req = create_request(&hash, 10);
        let encoded = encode_request(&req);

        assert_eq!(encoded[0], MSG_TYPE_REQUEST);

        let parsed = parse_message(&encoded).unwrap();
        match parsed {
            DataMessage::Request(r) => {
                assert_eq!(r.h, hash.to_vec());
                assert_eq!(r.htl, Some(10));
            }
            _ => panic!("Expected request"),
        }
    }

    #[test]
    fn test_encode_decode_response() {
        let hash = [0xcd; 32];
        let data = vec![1, 2, 3, 4, 5];
        let res = create_response(&hash, data.clone());
        let encoded = encode_response(&res);

        assert_eq!(encoded[0], MSG_TYPE_RESPONSE);

        let parsed = parse_message(&encoded).unwrap();
        match parsed {
            DataMessage::Response(r) => {
                assert_eq!(r.h, hash.to_vec());
                assert_eq!(r.d, data);
                assert!(!is_fragmented(&r));
            }
            _ => panic!("Expected response"),
        }
    }

    #[test]
    fn test_encode_decode_fragment_response() {
        let hash = [0xef; 32];
        let data = vec![10, 20, 30];
        let res = create_fragment_response(&hash, data.clone(), 2, 5);
        let encoded = encode_response(&res);

        let parsed = parse_message(&encoded).unwrap();
        match parsed {
            DataMessage::Response(r) => {
                assert_eq!(r.h, hash.to_vec());
                assert_eq!(r.d, data);
                assert!(is_fragmented(&r));
                assert_eq!(r.i, Some(2));
                assert_eq!(r.n, Some(5));
            }
            _ => panic!("Expected response"),
        }
    }

    #[test]
    fn test_hash_conversions() {
        let hash = [0x12; 32];
        let bytes = hash_to_bytes(&hash);
        let back = bytes_to_hash(&bytes).unwrap();
        assert_eq!(hash, back);
    }
}
