//! Mesh transport primitives for HashTree.
//!
//! This crate provides the reusable router, signaling, peer-link, and store
//! layers for hashtree mesh networking. Production transports live outside this
//! crate; the core abstractions here are intentionally transport-neutral so the
//! same logic can be driven by FIPS, local mocks, Nostr relay fixtures, or other
//! byte-link implementations.
//!
//! # Overview
//!
//! - **Storage Backend**: Any [`hashtree_core::Store`] implementation
//! - **Peer Discovery**: Any [`SignalingTransport`] implementation
//! - **Data Exchange**: Any [`PeerLink`] / [`PeerLinkFactory`] implementation
//! - **Protocol**: Request/response with hash-based addressing
//! - **Adaptive Selection**: Intelligent peer selection based on performance
//!
pub mod blob_resolver;
pub mod cashu;
pub mod channel;
pub mod local_bus;
pub mod mesh_session;
pub mod mesh_store_core;
pub mod mock;
pub mod multicast;
pub mod nostr;
pub mod peer_selector;
pub mod protocol;
pub mod pubsub_strategy;
pub mod relay_bridge;
pub mod root_events;
pub mod runtime_control;
pub mod runtime_peer;
pub mod runtime_state;
pub mod signaling;
pub mod transport;
pub mod types;

pub use blob_resolver::{blob_resolver, BlobResolver, RouteOnlyPeerLinks, RouteOnlySignaling};
pub use cashu::{
    cashu_mint_metadata_path, CashuMintMetadataRecord, CashuMintMetadataStore, CashuQuoteState,
    CashuRoutingConfig, ExpectedSettlement, NegotiatedQuote, CASHU_MINT_METADATA_VERSION,
};
pub use channel::{ChannelError, LatencyChannel, MockChannel, PeerChannel};
pub use local_bus::{LocalNostrBus, SharedLocalNostrBus};
pub use mesh_session::{
    forward_mesh_frame_to_sessions, resolve_root_from_local_buses_with_source,
    resolve_root_from_peer_sessions, MeshSession,
};
pub use mesh_store_core::{
    build_hedged_wave_plan, normalize_dispatch_config, run_hedged_waves, sync_selector_peers,
    BlobRouteKind, DataPumpStats, HedgedWaveAction, MeshReadSource, MeshRoutingConfig,
    MeshStoreCore, NamedBlobRoute, PubsubDeliveryMode, PubsubEvent, PubsubPublishStats,
    RequestDispatchConfig, ResponseBehaviorConfig, SimMeshStore, VerifiedBlockDelivery,
    VerifiedBlockDeliveryBatch,
};
pub use mock::{
    clear_channel_registry, MockConnectionFactory, MockDataChannel, MockLatencyMode, MockRelay,
    MockRelayTransport,
};
pub use multicast::{MulticastConfig, MulticastNostrBus};
pub use nostr::{decode_signaling_event, encode_signaling_event, NostrRelayTransport};
pub use peer_selector::{
    peer_principal, PeerMetadataSnapshot, PeerSelector, PeerStats, PersistedPeerMetadata,
    SelectionStrategy, SelectorSummary, PEER_METADATA_SNAPSHOT_VERSION,
};
pub use protocol::{
    bytes_to_hash, create_fragment_response, create_pubsub_frame, create_pubsub_interest,
    create_pubsub_inventory, create_pubsub_want, create_quote_request,
    create_quote_response_available, create_quote_response_unavailable, create_request,
    create_request_with_quote, create_response, encode_chunk, encode_payment, encode_payment_ack,
    encode_peer_hints, encode_pubsub_frame, encode_pubsub_interest, encode_pubsub_inventory,
    encode_pubsub_want, encode_quote_request, encode_quote_response, encode_request,
    encode_response, hash_to_bytes, hash_to_key, is_fragmented, parse_message, DataChunk,
    DataMessage, DataPayment, DataPaymentAck, DataQuoteRequest, DataQuoteResponse, DataRequest,
    DataResponse, PeerHints, PubsubFrame, PubsubInterest, PubsubInventory, PubsubWant,
    FRAGMENT_SIZE, MSG_TYPE_CHUNK, MSG_TYPE_PAYMENT, MSG_TYPE_PAYMENT_ACK, MSG_TYPE_PEER_HINTS,
    MSG_TYPE_PUBSUB_FRAME, MSG_TYPE_PUBSUB_INTEREST, MSG_TYPE_PUBSUB_INVENTORY,
    MSG_TYPE_PUBSUB_WANT, MSG_TYPE_QUOTE_REQUEST, MSG_TYPE_QUOTE_RESPONSE, MSG_TYPE_REQUEST,
    MSG_TYPE_RESPONSE,
};
pub use pubsub_strategy::{
    reciprocal_upload_weight, reciprocal_virtual_finish, select_reciprocal_outbound_job,
    stable_pubsub_score, OutboundJobCandidate, OutboundJobSelection, PeerTrafficSnapshot,
    PubsubCandidate, PubsubSchedulerConfig, PubsubSchedulingPolicy, PubsubSelection,
};
pub use relay_bridge::{
    MeshEventStore, MeshRelayClient, SharedMeshEventStore, SharedMeshRelayClient,
};
pub use root_events::{
    build_root_filter, hashtree_event_identifier, hashtree_root_kinds, is_hashtree_labeled_event,
    is_hashtree_root_kind, pick_latest_event, root_event_from_peer, PeerRootEvent, HASHTREE_KIND,
    HASHTREE_LABEL, HASHTREE_LEGACY_KIND,
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
pub use signaling::{MeshRouter, PeerEntry};
pub use transport::{PeerLink, PeerLinkFactory, SignalingTransport, TransportError};
pub use types::{
    classifier_channel, decrement_htl_with_policy, is_polite_peer, should_forward,
    should_forward_htl, validate_mesh_frame, ClassifierRx, ClassifierTx, ClassifyRequest, HtlMode,
    HtlPolicy, IceCandidate, KnownPeerRecord, KnownPeerSnapshot, MeshNostrFrame, MeshNostrPayload,
    MeshStats, MeshStoreConfig, PeerHTLConfig, PeerId, PeerPool, PeerState, PoolConfig,
    PoolSettings, SignalingMessage, TimedSeenSet, BLOB_REQUEST_POLICY, DECREMENT_AT_MAX_PROB,
    DECREMENT_AT_MIN_PROB, MAX_HTL, MESH_DEFAULT_HTL, MESH_EVENT_POLICY, MESH_MAX_HTL,
    MESH_PROTOCOL, MESH_PROTOCOL_VERSION, MESH_SIGNALING_EVENT_KIND, NOSTR_KIND_HASHTREE,
};
