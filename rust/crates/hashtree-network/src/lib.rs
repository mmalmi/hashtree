//! Mesh transport primitives for HashTree.
//!
//! This crate provides the reusable router, signaling, peer-link, and store
//! layers for hashtree mesh networking. The default production composition uses
//! Nostr websockets for signaling and WebRTC for direct links, but the same
//! abstractions support LAN buses, Bluetooth transports, and simulation.
//!
//! # Overview
//!
//! - **Storage Backend**: Any [`hashtree_core::Store`] implementation
//! - **Peer Discovery**: Any [`SignalingTransport`] implementation
//! - **Data Exchange**: Any [`PeerLink`] / [`PeerLinkFactory`] implementation
//! - **Protocol**: Request/response with hash-based addressing
//! - **Adaptive Selection**: Intelligent peer selection based on performance
//!
//! # Example
//!
//! ```rust,no_run
//! use hashtree_core::MemoryStore;
//! use hashtree_network::{MeshStore, MeshStoreConfig};
//! use nostr_sdk::prelude::*;
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let local_store = Arc::new(MemoryStore::new());
//!     let config = MeshStoreConfig::default();
//!
//!     let mut store = MeshStore::new(local_store, config);
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

pub mod bluetooth;
pub mod bluetooth_peer;
pub mod cashu;
pub mod channel;
pub mod local_bus;
pub mod manager;
pub mod mesh_session;
pub mod mesh_store_core;
pub mod mock;
pub mod multicast;
pub mod nostr;
pub mod peer;
pub mod peer_selector;
pub mod protocol;
pub mod pubsub_strategy;
pub mod real_factory;
pub mod relay_bridge;
pub mod root_events;
pub mod runtime_control;
pub mod runtime_peer;
pub mod runtime_state;
pub mod session;
pub mod signaling;
pub mod store;
pub mod transport;
pub mod types;
pub mod wifi_aware;

pub use bluetooth::{
    install_mobile_bluetooth_bridge, BluetoothBackendState, BluetoothConfig, BluetoothMesh,
    BluetoothPeerRegistrar, BluetoothRuntimeContext, MobileBluetoothBridge, PendingBluetoothLink,
};
pub use bluetooth_peer::{BluetoothFrame, BluetoothLink, BluetoothPeer};
pub use cashu::{
    cashu_mint_metadata_path, CashuMintMetadataRecord, CashuMintMetadataStore, CashuQuoteState,
    CashuRoutingConfig, ExpectedSettlement, NegotiatedQuote, CASHU_MINT_METADATA_VERSION,
};
pub use channel::{ChannelError, LatencyChannel, MockChannel, PeerChannel};
pub use local_bus::{LocalNostrBus, SharedLocalNostrBus};
pub use manager::{
    PeerClassifier, PeerEntry as RuntimePeerEntry, PeerStatus, WebRTCConfig, WebRTCManager,
    WebRTCState,
};
pub use mesh_session::{
    forward_mesh_frame_to_sessions, resolve_root_from_local_buses_with_source,
    resolve_root_from_peer_sessions, MeshSession,
};
pub use mesh_store_core::{
    build_hedged_wave_plan, normalize_dispatch_config, run_hedged_waves, sync_selector_peers,
    DataPumpStats, HedgedWaveAction, MeshReadSource, MeshRoutingConfig, MeshStoreCore,
    ProductionMeshStore, RequestDispatchConfig, ResponseBehaviorConfig, SimMeshStore,
};
pub use mock::{
    clear_channel_registry, MockConnectionFactory, MockDataChannel, MockLatencyMode, MockRelay,
    MockRelayTransport,
};
pub use multicast::{MulticastConfig, MulticastNostrBus};
pub use nostr::{decode_signaling_event, encode_signaling_event, NostrRelayTransport};
pub use peer::{ContentStore, Peer, PendingRequest};
pub use peer_selector::{
    peer_principal, PeerMetadataSnapshot, PeerSelector, PeerStats, PersistedPeerMetadata,
    SelectionStrategy, SelectorSummary, PEER_METADATA_SNAPSHOT_VERSION,
};
pub use protocol::{
    bytes_to_hash, create_fragment_response, create_quote_request, create_quote_response_available,
    create_quote_response_unavailable, create_request, create_request_with_quote, create_response,
    encode_chunk, encode_payment, encode_payment_ack, encode_peer_hints, encode_quote_request,
    encode_quote_response, encode_request, encode_response, hash_to_bytes, hash_to_key,
    is_fragmented, parse_message, DataChunk, DataMessage, DataPayment, DataPaymentAck,
    DataQuoteRequest, DataQuoteResponse, DataRequest, DataResponse, PeerHints, FRAGMENT_SIZE,
    MSG_TYPE_CHUNK, MSG_TYPE_PAYMENT, MSG_TYPE_PAYMENT_ACK, MSG_TYPE_PEER_HINTS,
    MSG_TYPE_QUOTE_REQUEST, MSG_TYPE_QUOTE_RESPONSE, MSG_TYPE_REQUEST, MSG_TYPE_RESPONSE,
};
pub use pubsub_strategy::{
    reciprocal_upload_weight, reciprocal_virtual_finish, select_reciprocal_outbound_job,
    stable_pubsub_score, OutboundJobCandidate, OutboundJobSelection, PeerTrafficSnapshot,
    PubsubCandidate, PubsubSchedulerConfig, PubsubSchedulingPolicy, PubsubSelection,
};
pub use real_factory::WebRtcPeerLinkFactory;
pub use relay_bridge::{
    MeshEventStore, MeshRelayClient, SharedMeshEventStore, SharedMeshRelayClient,
};
pub use root_events::{
    build_root_filter, hashtree_event_identifier, is_hashtree_labeled_event, pick_latest_event,
    root_event_from_peer, PeerRootEvent, HASHTREE_KIND, HASHTREE_LABEL,
};
pub use runtime_control::{
    can_track_source_peer, cleanup_stale_peers, create_signaling_event, dispatch_signaling_message,
    forward_mesh_frame_from_runtime, handle_peer_state_event, handle_signaling_event,
    handle_signaling_message, PeerStateEvent,
};
pub use runtime_peer::{
    can_track_signal_path_peer, remember_peer_signal_path, ConnectionState, MeshPeerEntry,
    PeerDirection, PeerSignalPath, PeerTransport, TransportPeerRegistrar,
};
pub use runtime_state::MeshRuntimeState;
pub use session::MeshPeer;
#[cfg(test)]
pub use session::TestMeshPeer;
pub use signaling::{MeshRouter, PeerEntry};
pub use store::{MeshStore, MeshStoreError};
pub use transport::{PeerLink, PeerLinkFactory, SignalingTransport, TransportError};
pub use types::{
    classifier_channel, decrement_htl_with_policy, is_polite_peer, should_forward,
    should_forward_htl, validate_mesh_frame, ClassifierRx, ClassifierTx, ClassifyRequest, HtlMode,
    HtlPolicy, IceCandidate, KnownPeerRecord, KnownPeerSnapshot, MeshNostrFrame, MeshNostrPayload,
    MeshStats, MeshStoreConfig, PeerHTLConfig, PeerId, PeerPool, PeerState, PoolConfig,
    PoolSettings, SignalingMessage, TimedSeenSet, WebRTCStats, BLOB_REQUEST_POLICY,
    DATA_CHANNEL_LABEL, DECREMENT_AT_MAX_PROB, DECREMENT_AT_MIN_PROB, MAX_HTL, MESH_DEFAULT_HTL,
    MESH_EVENT_POLICY, MESH_MAX_HTL, MESH_PROTOCOL, MESH_PROTOCOL_VERSION,
    MESH_SIGNALING_EVENT_KIND, NOSTR_KIND_HASHTREE,
};
pub use wifi_aware::{
    install_mobile_wifi_aware_bridge, mobile_wifi_aware_bridge, MobileWifiAwareBridge,
    WifiAwareConfig, WifiAwareEvent, WifiAwareNostrBus, WIFI_AWARE_SOURCE,
};
