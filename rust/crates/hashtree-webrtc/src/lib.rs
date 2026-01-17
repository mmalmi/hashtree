//! WebRTC P2P transport for HashTree
//!
//! This crate provides WebRTC-based peer-to-peer data exchange for hashtree,
//! using Nostr relays for peer discovery and signaling.
//!
//! # Overview
//!
//! - **Peer Discovery**: Uses Nostr relay network for signaling
//! - **Data Exchange**: WebRTC data channels for binary data transfer
//! - **Protocol**: Request/response with hash-based addressing
//! - **Adaptive Selection**: Intelligent peer selection based on performance
//!
//! # Example
//!
//! ```rust,no_run
//! use hashtree_core::MemoryStore;
//! use hashtree_webrtc::{WebRTCStore, WebRTCStoreConfig};
//! use nostr_sdk::prelude::*;
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let local_store = Arc::new(MemoryStore::new());
//!     let config = WebRTCStoreConfig::default();
//!
//!     let mut store = WebRTCStore::new(local_store, config);
//!
//!     // Generate or load Nostr keys
//!     let keys = Keys::generate();
//!
//!     // Start P2P network
//!     store.start(keys).await?;
//!
//!     // Now store.get() will try local first, then fetch from peers
//!
//!     Ok(())
//! }
//! ```

pub mod channel;
pub mod generic_store;
pub mod mock;
pub mod nostr;
pub mod peer;
pub mod peer_selector;
pub mod protocol;
pub mod real_factory;
pub mod signaling;
pub mod store;
pub mod transport;
pub mod types;

pub use channel::{ChannelError, LatencyChannel, MockChannel, PeerChannel};
pub use peer::{ForwardRequestCallback, Peer, PeerError};
pub use peer_selector::{PeerSelector, PeerStats, SelectionStrategy, SelectorSummary};
pub use protocol::{
    bytes_to_hash, create_fragment_response, create_request, create_response, encode_request,
    encode_response, hash_to_bytes, hash_to_key, is_fragmented, parse_message, DataMessage,
    DataRequest, DataResponse, FRAGMENT_SIZE, MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE,
};
pub use store::{WebRTCStore, WebRTCStoreError};
pub use types::{
    classifier_channel, is_polite_peer, should_forward, ClassifierRx, ClassifierTx,
    ClassifyRequest, ForwardRequest, ForwardRx, ForwardTx, IceCandidate, PeerId, PeerHTLConfig,
    PeerPool, PeerState, PoolConfig, PoolSettings, SignalingMessage, WebRTCStats,
    WebRTCStoreConfig, DATA_CHANNEL_LABEL, MAX_HTL, NOSTR_KIND_HASHTREE,
};
pub use transport::{
    DataChannel, PeerConnectionFactory, RelayTransport, SignalingConfig, TransportError,
};
pub use mock::{
    clear_channel_registry, MockConnectionFactory, MockDataChannel, MockRelay, MockRelayTransport,
};
pub use nostr::NostrRelayTransport;
pub use real_factory::RealPeerConnectionFactory;
pub use signaling::{PeerEntry, SignalingManager};
pub use generic_store::{GenericStore, SimStore, ProductionStore};
