//! Mesh signaling and transport types compatible with hashtree-ts.

pub use hashtree_network::{
    decrement_htl_with_policy, should_forward_htl, validate_mesh_frame, DataChunk, DataMessage,
    DataPayment, DataPaymentAck, DataQuoteRequest, DataQuoteResponse, DataRequest, DataResponse,
    HtlMode, HtlPolicy, IceCandidate, MeshNostrFrame, MeshNostrPayload, PeerDirection,
    PeerHTLConfig, PeerId, PeerPool, PeerStateEvent, PeerStatus, PoolConfig, PoolSettings,
    RequestDispatchConfig, SelectionStrategy, SignalingMessage, TimedSeenSet, WebRTCConfig,
    BLOB_REQUEST_POLICY, DECREMENT_AT_MAX_PROB, DECREMENT_AT_MIN_PROB, MAX_HTL, MESH_DEFAULT_HTL,
    MESH_EVENT_POLICY, MESH_MAX_HTL, MESH_PROTOCOL, MESH_PROTOCOL_VERSION, MSG_TYPE_CHUNK,
    MSG_TYPE_PAYMENT, MSG_TYPE_PAYMENT_ACK, MSG_TYPE_QUOTE_REQUEST, MSG_TYPE_QUOTE_RESPONSE,
    MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE,
};

pub fn decrement_htl(htl: u8, config: &PeerHTLConfig) -> u8 {
    decrement_htl_with_policy(htl, &BLOB_REQUEST_POLICY, config)
}

pub fn should_forward(htl: u8) -> bool {
    should_forward_htl(htl)
}

pub const WEBRTC_KIND: u64 = hashtree_network::MESH_SIGNALING_EVENT_KIND as u64;
pub const HELLO_TAG: &str = "hello";
pub const WEBRTC_TAG: &str = "webrtc";

pub fn encode_request(req: &DataRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_request(req))
}

pub fn encode_response(res: &DataResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_response(res))
}

pub fn encode_quote_request(req: &DataQuoteRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_quote_request(req))
}

pub fn encode_quote_response(res: &DataQuoteResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_quote_response(res))
}

pub fn encode_payment(req: &DataPayment) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_payment(req))
}

pub fn encode_payment_ack(res: &DataPaymentAck) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_payment_ack(res))
}

pub fn encode_chunk(chunk: &DataChunk) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    Ok(hashtree_network::encode_chunk(chunk))
}

pub fn parse_message(data: &[u8]) -> Result<DataMessage, rmp_serde::decode::Error> {
    let msg_type = data.first().copied().unwrap_or_default();
    hashtree_network::parse_message(data)
        .ok_or(rmp_serde::decode::Error::LengthMismatch(msg_type as u32))
}

pub fn hash_to_hex(hash: &[u8]) -> String {
    hashtree_network::hash_to_key(hash)
}

pub fn encode_message(msg: &DataMessage) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    match msg {
        DataMessage::Request(req) => encode_request(req),
        DataMessage::Response(res) => encode_response(res),
        DataMessage::QuoteRequest(req) => encode_quote_request(req),
        DataMessage::QuoteResponse(res) => encode_quote_response(res),
        DataMessage::Payment(req) => encode_payment(req),
        DataMessage::PaymentAck(res) => encode_payment_ack(res),
        DataMessage::Chunk(chunk) => encode_chunk(chunk),
        DataMessage::PeerHints(hints) => Ok(hashtree_network::encode_peer_hints(hints)),
        DataMessage::PubsubInterest(interest) => {
            Ok(hashtree_network::encode_pubsub_interest(interest))
        }
        DataMessage::PubsubFrame(frame) => Ok(hashtree_network::encode_pubsub_frame(frame)),
        DataMessage::PubsubInventory(inv) => Ok(hashtree_network::encode_pubsub_inventory(inv)),
        DataMessage::PubsubWant(want) => Ok(hashtree_network::encode_pubsub_want(want)),
    }
}
