//! Wire protocol for hashtree mesh data exchange.
//!
//! Compatible with hashtree-ts wire format:
//! - Request:        [0x00][msgpack: {h: bytes32, htl?: u8, q?: u64}]
//! - Response:       [0x01][msgpack: {h: bytes32, d: bytes, i?: u32, n?: u32}]
//! - QuoteRequest:   [0x02][msgpack: {h: bytes32, p: u64, t: u32, m?: string}]
//! - QuoteResponse:  [0x03][msgpack: {h: bytes32, a: bool, q?: u64, p?: u64, t?: u32, m?: string}]
//! - Payment:        [0x04][msgpack: {h: bytes32, q: u64, c: u32, p: u64, m?: string, tok: string}]
//! - PaymentAck:     [0x05][msgpack: {h: bytes32, q: u64, c: u32, a: bool, e?: string}]
//! - Chunk:          [0x06][msgpack: {h: bytes32, q: u64, c: u32, n: u32, p: u64, d: bytes}]
//! - PeerHints:      [0x07][msgpack: {u: [direct signaling endpoint URLs]}]
//! - PubsubInterest: [0x08][msgpack: {s: stream, sub: subscriber peer id, q: seq, a: active, htl?: u8}]
//! - PubsubFrame:    [0x09][msgpack: {s: stream, q: seq, o: origin peer id, htl?: u8, d: bytes}]
//! - PubsubInventory:[0x0a][msgpack: {s: stream, q: seq, o: origin peer id, b: bytes, htl?: u8}]
//! - PubsubWant:     [0x0b][msgpack: {s: stream, q: seq, o: origin peer id}]
//!
//! Fragmented responses include `i` (index) and `n` (total), unfragmented omit them.

use hashtree_core::Hash;
use serde::{Deserialize, Serialize};

fn default_htl() -> u8 {
    crate::types::MAX_HTL
}

fn is_max_htl(htl: &u8) -> bool {
    *htl == crate::types::MAX_HTL
}

/// Message type bytes (prefix before MessagePack body)
pub const MSG_TYPE_REQUEST: u8 = 0x00;
pub const MSG_TYPE_RESPONSE: u8 = 0x01;
pub const MSG_TYPE_QUOTE_REQUEST: u8 = 0x02;
pub const MSG_TYPE_QUOTE_RESPONSE: u8 = 0x03;
pub const MSG_TYPE_PAYMENT: u8 = 0x04;
pub const MSG_TYPE_PAYMENT_ACK: u8 = 0x05;
pub const MSG_TYPE_CHUNK: u8 = 0x06;
pub const MSG_TYPE_PEER_HINTS: u8 = 0x07;
pub const MSG_TYPE_PUBSUB_INTEREST: u8 = 0x08;
pub const MSG_TYPE_PUBSUB_FRAME: u8 = 0x09;
pub const MSG_TYPE_PUBSUB_INVENTORY: u8 = 0x0a;
pub const MSG_TYPE_PUBSUB_WANT: u8 = 0x0b;

/// Fragment size for large data.
pub const FRAGMENT_SIZE: usize = 32 * 1024;

/// Data request message body
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataRequest {
    /// 32-byte hash
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    /// Hops To Live (defaults to MAX_HTL when omitted on the wire)
    #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
    pub htl: u8,
    /// Optional quote identifier for paid retrieval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<u64>,
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

/// Quote request message body
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataQuoteRequest {
    /// 32-byte hash
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    /// Offered payment amount in sat.
    pub p: u64,
    /// Quote validity window in milliseconds.
    pub t: u32,
    /// Optional settlement mint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
}

/// Quote response message body
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataQuoteResponse {
    /// 32-byte hash
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    /// Whether the peer is willing and able to serve the request.
    pub a: bool,
    /// Quote identifier to include in the follow-up request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<u64>,
    /// Accepted payment amount in sat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<u64>,
    /// Quote validity window in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<u32>,
    /// Settlement mint URL accepted for this quote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
}

/// Payment message body for chunk-by-chunk settlement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPayment {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    pub q: u64,
    pub c: u32,
    pub p: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
    pub tok: String,
}

/// Payment acknowledgement for quoted chunk settlement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPaymentAck {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    pub q: u64,
    pub c: u32,
    pub a: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e: Option<String>,
}

/// Quoted data chunk delivered after payment negotiation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataChunk {
    #[serde(with = "serde_bytes")]
    pub h: Vec<u8>,
    pub q: u64,
    pub c: u32,
    pub n: u32,
    pub p: u64,
    #[serde(with = "serde_bytes")]
    pub d: Vec<u8>,
}

/// Private signaling hints exchanged over an established peer link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerHints {
    /// Daemon signaling endpoint base URLs for this peer, never public relay tags.
    #[serde(default, rename = "u")]
    pub signal_urls: Vec<String>,
}

/// Pubsub interest advertisement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubsubInterest {
    /// Stream/topic/author key.
    #[serde(rename = "s")]
    pub stream_id: String,
    /// Stable subscriber peer id that originated the interest.
    #[serde(rename = "sub")]
    pub subscriber_peer_id: String,
    /// Monotonic sequence scoped to `subscriber_peer_id`.
    #[serde(rename = "q")]
    pub seq: u64,
    /// Whether this is a subscribe (`true`) or unsubscribe (`false`).
    #[serde(rename = "a")]
    pub active: bool,
    /// Hops To Live (defaults to MAX_HTL when omitted on the wire).
    #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
    pub htl: u8,
}

/// Pubsub data frame delivered over mesh peer links.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubsubFrame {
    /// Stream/topic/author key.
    #[serde(rename = "s")]
    pub stream_id: String,
    /// Monotonic sequence scoped to `origin_peer_id + stream_id`.
    #[serde(rename = "q")]
    pub seq: u64,
    /// Peer id that originated this frame.
    #[serde(rename = "o")]
    pub origin_peer_id: String,
    /// Hops To Live (defaults to MAX_HTL when omitted on the wire).
    #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
    pub htl: u8,
    /// Live payload bytes.
    #[serde(with = "serde_bytes", rename = "d")]
    pub payload: Vec<u8>,
}

/// Pubsub inventory announcement for inventory-first delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubsubInventory {
    /// Stream/topic/author key.
    #[serde(rename = "s")]
    pub stream_id: String,
    /// Monotonic sequence scoped to `origin_peer_id + stream_id`.
    #[serde(rename = "q")]
    pub seq: u64,
    /// Peer id that originated this frame.
    #[serde(rename = "o")]
    pub origin_peer_id: String,
    /// Payload size in bytes.
    #[serde(rename = "b")]
    pub payload_bytes: u64,
    /// Hops To Live (defaults to MAX_HTL when omitted on the wire).
    #[serde(default = "default_htl", skip_serializing_if = "is_max_htl")]
    pub htl: u8,
}

/// Pubsub want request for inventory-first delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubsubWant {
    /// Stream/topic/author key.
    #[serde(rename = "s")]
    pub stream_id: String,
    /// Monotonic sequence scoped to `origin_peer_id + stream_id`.
    #[serde(rename = "q")]
    pub seq: u64,
    /// Peer id that originated the wanted frame.
    #[serde(rename = "o")]
    pub origin_peer_id: String,
}

/// Parsed data message
#[derive(Debug, Clone)]
pub enum DataMessage {
    Request(DataRequest),
    Response(DataResponse),
    QuoteRequest(DataQuoteRequest),
    QuoteResponse(DataQuoteResponse),
    Payment(DataPayment),
    PaymentAck(DataPaymentAck),
    Chunk(DataChunk),
    PeerHints(PeerHints),
    PubsubInterest(PubsubInterest),
    PubsubFrame(PubsubFrame),
    PubsubInventory(PubsubInventory),
    PubsubWant(PubsubWant),
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

/// Encode a quote request message to wire format.
pub fn encode_quote_request(req: &DataQuoteRequest) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(req).expect("Failed to encode quote request");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_QUOTE_REQUEST);
    result.extend(body);
    result
}

/// Encode a quote response message to wire format.
pub fn encode_quote_response(res: &DataQuoteResponse) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(res).expect("Failed to encode quote response");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_QUOTE_RESPONSE);
    result.extend(body);
    result
}

/// Encode a payment message to wire format.
pub fn encode_payment(req: &DataPayment) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(req).expect("Failed to encode payment");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PAYMENT);
    result.extend(body);
    result
}

/// Encode a payment acknowledgement to wire format.
pub fn encode_payment_ack(res: &DataPaymentAck) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(res).expect("Failed to encode payment ack");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PAYMENT_ACK);
    result.extend(body);
    result
}

/// Encode a quoted data chunk to wire format.
pub fn encode_chunk(chunk: &DataChunk) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(chunk).expect("Failed to encode chunk");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_CHUNK);
    result.extend(body);
    result
}

/// Encode private peer hints to wire format.
pub fn encode_peer_hints(hints: &PeerHints) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(hints).expect("Failed to encode peer hints");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PEER_HINTS);
    result.extend(body);
    result
}

/// Encode a pubsub interest to wire format.
pub fn encode_pubsub_interest(interest: &PubsubInterest) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(interest).expect("Failed to encode pubsub interest");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PUBSUB_INTEREST);
    result.extend(body);
    result
}

/// Encode a pubsub data frame to wire format.
pub fn encode_pubsub_frame(frame: &PubsubFrame) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(frame).expect("Failed to encode pubsub frame");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PUBSUB_FRAME);
    result.extend(body);
    result
}

/// Encode a pubsub inventory announcement to wire format.
pub fn encode_pubsub_inventory(inv: &PubsubInventory) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(inv).expect("Failed to encode pubsub inventory");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PUBSUB_INVENTORY);
    result.extend(body);
    result
}

/// Encode a pubsub want request to wire format.
pub fn encode_pubsub_want(want: &PubsubWant) -> Vec<u8> {
    let body = rmp_serde::to_vec_named(want).expect("Failed to encode pubsub want");
    let mut result = Vec::with_capacity(1 + body.len());
    result.push(MSG_TYPE_PUBSUB_WANT);
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
        MSG_TYPE_REQUEST => rmp_serde::from_slice::<DataRequest>(body)
            .ok()
            .map(DataMessage::Request),
        MSG_TYPE_RESPONSE => rmp_serde::from_slice::<DataResponse>(body)
            .ok()
            .map(DataMessage::Response),
        MSG_TYPE_QUOTE_REQUEST => rmp_serde::from_slice::<DataQuoteRequest>(body)
            .ok()
            .map(DataMessage::QuoteRequest),
        MSG_TYPE_QUOTE_RESPONSE => rmp_serde::from_slice::<DataQuoteResponse>(body)
            .ok()
            .map(DataMessage::QuoteResponse),
        MSG_TYPE_PAYMENT => rmp_serde::from_slice::<DataPayment>(body)
            .ok()
            .map(DataMessage::Payment),
        MSG_TYPE_PAYMENT_ACK => rmp_serde::from_slice::<DataPaymentAck>(body)
            .ok()
            .map(DataMessage::PaymentAck),
        MSG_TYPE_CHUNK => rmp_serde::from_slice::<DataChunk>(body)
            .ok()
            .map(DataMessage::Chunk),
        MSG_TYPE_PEER_HINTS => rmp_serde::from_slice::<PeerHints>(body)
            .ok()
            .map(DataMessage::PeerHints),
        MSG_TYPE_PUBSUB_INTEREST => rmp_serde::from_slice::<PubsubInterest>(body)
            .ok()
            .map(DataMessage::PubsubInterest),
        MSG_TYPE_PUBSUB_FRAME => rmp_serde::from_slice::<PubsubFrame>(body)
            .ok()
            .map(DataMessage::PubsubFrame),
        MSG_TYPE_PUBSUB_INVENTORY => rmp_serde::from_slice::<PubsubInventory>(body)
            .ok()
            .map(DataMessage::PubsubInventory),
        MSG_TYPE_PUBSUB_WANT => rmp_serde::from_slice::<PubsubWant>(body)
            .ok()
            .map(DataMessage::PubsubWant),
        _ => None,
    }
}

/// Create a request
pub fn create_request(hash: &Hash, htl: u8) -> DataRequest {
    DataRequest {
        h: hash.to_vec(),
        htl,
        q: None,
    }
}

/// Create a request that references a previously accepted quote.
pub fn create_request_with_quote(hash: &Hash, htl: u8, quote_id: u64) -> DataRequest {
    DataRequest {
        h: hash.to_vec(),
        htl,
        q: Some(quote_id),
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

/// Create a pubsub interest message.
pub fn create_pubsub_interest(
    stream_id: impl Into<String>,
    subscriber_peer_id: impl Into<String>,
    seq: u64,
    active: bool,
    htl: u8,
) -> PubsubInterest {
    PubsubInterest {
        stream_id: stream_id.into(),
        subscriber_peer_id: subscriber_peer_id.into(),
        seq,
        active,
        htl,
    }
}

/// Create a pubsub data frame.
pub fn create_pubsub_frame(
    stream_id: impl Into<String>,
    seq: u64,
    origin_peer_id: impl Into<String>,
    payload: Vec<u8>,
    htl: u8,
) -> PubsubFrame {
    PubsubFrame {
        stream_id: stream_id.into(),
        seq,
        origin_peer_id: origin_peer_id.into(),
        payload,
        htl,
    }
}

/// Create a pubsub inventory announcement.
pub fn create_pubsub_inventory(
    stream_id: impl Into<String>,
    seq: u64,
    origin_peer_id: impl Into<String>,
    payload_bytes: u64,
    htl: u8,
) -> PubsubInventory {
    PubsubInventory {
        stream_id: stream_id.into(),
        seq,
        origin_peer_id: origin_peer_id.into(),
        payload_bytes,
        htl,
    }
}

/// Create a pubsub want request.
pub fn create_pubsub_want(
    stream_id: impl Into<String>,
    seq: u64,
    origin_peer_id: impl Into<String>,
) -> PubsubWant {
    PubsubWant {
        stream_id: stream_id.into(),
        seq,
        origin_peer_id: origin_peer_id.into(),
    }
}

/// Create a quote request.
pub fn create_quote_request(
    hash: &Hash,
    ttl_ms: u32,
    payment_sat: u64,
    mint_url: Option<&str>,
) -> DataQuoteRequest {
    DataQuoteRequest {
        h: hash.to_vec(),
        p: payment_sat,
        t: ttl_ms,
        m: mint_url.map(str::to_string),
    }
}

/// Create an accepted quote response.
pub fn create_quote_response_available(
    hash: &Hash,
    quote_id: u64,
    payment_sat: u64,
    ttl_ms: u32,
    mint_url: Option<&str>,
) -> DataQuoteResponse {
    DataQuoteResponse {
        h: hash.to_vec(),
        a: true,
        q: Some(quote_id),
        p: Some(payment_sat),
        t: Some(ttl_ms),
        m: mint_url.map(str::to_string),
    }
}

/// Create a declined quote response.
pub fn create_quote_response_unavailable(hash: &Hash) -> DataQuoteResponse {
    DataQuoteResponse {
        h: hash.to_vec(),
        a: false,
        q: None,
        p: None,
        t: None,
        m: None,
    }
}

/// Create a fragmented response
pub fn create_fragment_response(
    hash: &Hash,
    data: Vec<u8>,
    index: u32,
    total: u32,
) -> DataResponse {
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
                assert_eq!(r.htl, 10);
            }
            _ => panic!("Expected request"),
        }
    }

    #[test]
    fn test_decode_request_without_explicit_htl_defaults_to_max() {
        #[derive(Serialize)]
        struct LegacyRequest {
            #[serde(with = "serde_bytes")]
            h: Vec<u8>,
        }

        let hash = [0x21; 32];
        let body = rmp_serde::to_vec_named(&LegacyRequest { h: hash.to_vec() }).unwrap();
        let mut encoded = vec![MSG_TYPE_REQUEST];
        encoded.extend(body);

        let parsed = parse_message(&encoded).unwrap();
        match parsed {
            DataMessage::Request(r) => {
                assert_eq!(r.h, hash.to_vec());
                assert_eq!(r.htl, crate::types::MAX_HTL);
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
    fn test_encode_decode_quote_request() {
        let hash = [0x44; 32];
        let req = create_quote_request(&hash, 7, 2_500, Some("https://mint.example"));
        let encoded = encode_quote_request(&req);

        assert_eq!(encoded[0], MSG_TYPE_QUOTE_REQUEST);

        let parsed = parse_message(&encoded).unwrap();
        match parsed {
            DataMessage::QuoteRequest(r) => {
                assert_eq!(r.h, hash.to_vec());
                assert_eq!(r.t, 7);
                assert_eq!(r.p, 2_500);
                assert_eq!(r.m.as_deref(), Some("https://mint.example"));
            }
            _ => panic!("Expected quote request"),
        }
    }

    #[test]
    fn test_encode_decode_quote_response_and_quoted_request() {
        let hash = [0x55; 32];
        let quote =
            create_quote_response_available(&hash, 19, 2_500, 7, Some("https://mint.example"));
        let encoded_quote = encode_quote_response(&quote);

        assert_eq!(encoded_quote[0], MSG_TYPE_QUOTE_RESPONSE);

        let parsed_quote = parse_message(&encoded_quote).unwrap();
        match parsed_quote {
            DataMessage::QuoteResponse(r) => {
                assert_eq!(r.h, hash.to_vec());
                assert!(r.a);
                assert_eq!(r.q, Some(19));
                assert_eq!(r.p, Some(2_500));
                assert_eq!(r.t, Some(7));
                assert_eq!(r.m.as_deref(), Some("https://mint.example"));
            }
            _ => panic!("Expected quote response"),
        }

        let req = create_request_with_quote(&hash, 9, 19);
        let encoded_req = encode_request(&req);
        let parsed_req = parse_message(&encoded_req).unwrap();
        match parsed_req {
            DataMessage::Request(r) => {
                assert_eq!(r.h, hash.to_vec());
                assert_eq!(r.htl, 9);
                assert_eq!(r.q, Some(19));
            }
            _ => panic!("Expected quoted request"),
        }
    }

    #[test]
    fn test_hash_conversions() {
        let hash = [0x12; 32];
        let bytes = hash_to_bytes(&hash);
        let back = bytes_to_hash(&bytes).unwrap();
        assert_eq!(hash, back);
    }

    #[test]
    fn test_encode_decode_payment_ack_and_chunk() {
        let payment = DataPayment {
            h: vec![0x61; 32],
            q: 9,
            c: 1,
            p: 3,
            m: Some("https://mint.example".to_string()),
            tok: "cashuBtoken".to_string(),
        };
        let payment_ack = DataPaymentAck {
            h: vec![0x62; 32],
            q: 9,
            c: 1,
            a: true,
            e: None,
        };
        let chunk = DataChunk {
            h: vec![0x63; 32],
            q: 9,
            c: 1,
            n: 2,
            p: 3,
            d: vec![1, 2, 3],
        };

        match parse_message(&encode_payment(&payment)).unwrap() {
            DataMessage::Payment(parsed) => {
                assert_eq!(parsed.q, payment.q);
                assert_eq!(parsed.p, payment.p);
                assert_eq!(parsed.m, payment.m);
            }
            _ => panic!("Expected payment"),
        }

        match parse_message(&encode_payment_ack(&payment_ack)).unwrap() {
            DataMessage::PaymentAck(parsed) => {
                assert_eq!(parsed.q, payment_ack.q);
                assert_eq!(parsed.c, payment_ack.c);
                assert!(parsed.a);
            }
            _ => panic!("Expected payment ack"),
        }

        match parse_message(&encode_chunk(&chunk)).unwrap() {
            DataMessage::Chunk(parsed) => {
                assert_eq!(parsed.q, chunk.q);
                assert_eq!(parsed.n, chunk.n);
                assert_eq!(parsed.d, chunk.d);
            }
            _ => panic!("Expected chunk"),
        }
    }

    #[test]
    fn test_encode_decode_peer_hints() {
        let hints = PeerHints {
            signal_urls: vec!["http://127.0.0.1:18080".to_string()],
        };

        match parse_message(&encode_peer_hints(&hints)).unwrap() {
            DataMessage::PeerHints(parsed) => {
                assert_eq!(parsed.signal_urls, hints.signal_urls);
            }
            _ => panic!("Expected peer hints"),
        }
    }

    #[test]
    fn test_encode_decode_pubsub_messages() {
        let interest = create_pubsub_interest("author:alice", "subscriber-a", 42, true, 5);
        match parse_message(&encode_pubsub_interest(&interest)).unwrap() {
            DataMessage::PubsubInterest(parsed) => {
                assert_eq!(parsed.stream_id, "author:alice");
                assert_eq!(parsed.subscriber_peer_id, "subscriber-a");
                assert_eq!(parsed.seq, 42);
                assert!(parsed.active);
                assert_eq!(parsed.htl, 5);
            }
            _ => panic!("Expected pubsub interest"),
        }

        let frame = create_pubsub_frame("author:alice", 7, "publisher-a", vec![1, 2, 3], 4);
        match parse_message(&encode_pubsub_frame(&frame)).unwrap() {
            DataMessage::PubsubFrame(parsed) => {
                assert_eq!(parsed.stream_id, "author:alice");
                assert_eq!(parsed.seq, 7);
                assert_eq!(parsed.origin_peer_id, "publisher-a");
                assert_eq!(parsed.htl, 4);
                assert_eq!(parsed.payload, vec![1, 2, 3]);
            }
            _ => panic!("Expected pubsub frame"),
        }

        let inv = create_pubsub_inventory("author:alice", 7, "publisher-a", 3, 4);
        match parse_message(&encode_pubsub_inventory(&inv)).unwrap() {
            DataMessage::PubsubInventory(parsed) => {
                assert_eq!(parsed.stream_id, "author:alice");
                assert_eq!(parsed.seq, 7);
                assert_eq!(parsed.origin_peer_id, "publisher-a");
                assert_eq!(parsed.payload_bytes, 3);
                assert_eq!(parsed.htl, 4);
            }
            _ => panic!("Expected pubsub inventory"),
        }

        let want = create_pubsub_want("author:alice", 7, "publisher-a");
        match parse_message(&encode_pubsub_want(&want)).unwrap() {
            DataMessage::PubsubWant(parsed) => {
                assert_eq!(parsed.stream_id, "author:alice");
                assert_eq!(parsed.seq, 7);
                assert_eq!(parsed.origin_peer_id, "publisher-a");
            }
            _ => panic!("Expected pubsub want"),
        }
    }
}
