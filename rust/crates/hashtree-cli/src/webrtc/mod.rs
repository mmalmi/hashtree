//! WebRTC peer-to-peer connectivity for hashtree data exchange
//!
//! Uses Nostr relays for signaling with the same protocol as iris-client:
//! - Event kind: 30078 (KIND_APP_DATA)
//! - Tag: ["l", "webrtc"]
//! - Message types: hello, offer, answer, candidate

mod peer;
mod signaling;
pub mod types;

#[cfg(test)]
mod tests;

pub use peer::{ContentStore, Peer, PendingRequest};
pub use signaling::{ConnectionState, PeerClassifier, PeerEntry, WebRTCManager, WebRTCState};
pub use types::{DataMessage, DataRequest, PeerDirection, PeerId, PeerPool, PoolConfig, PoolSettings, SignalingMessage, WebRTCConfig, encode_request, MAX_HTL};
