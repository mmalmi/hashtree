//! Simulation tools for hashtree mesh protocols.
//!
//! Provides simulation and router tests using the same router and store core as
//! production, but with mock transports.
//!
//! ## Architecture
//!
//! - `mesh_sim::Simulation` - uses `MeshStoreCore` with mock transports
//! - Shared router tests - exercise the production signaling/router core directly
//! - `WsRelay` - WebSocket Nostr relay for integration testing

pub mod author_pubsub;
pub mod cashu_test_mint;
pub mod mesh_pubsub;
pub mod mesh_sim;
pub mod mint_client;
mod mock_registry;
#[cfg(feature = "nostr")]
pub mod nostr_mesh;
pub mod ws_relay;

// Re-export main types from mesh_sim
pub use cashu_test_mint::{
    ChannelSettlement, ChannelState, LocalTestCashuMint, MintError, MintStats,
};
pub use mesh_pubsub::{
    compute_workload_peer_graph, run_mesh_pubsub_htl_baseline_on_graph,
    run_mesh_pubsub_htl_flood_baseline, run_mesh_pubsub_htl_gossipsub_baseline,
    run_mesh_pubsub_htl_gossipsub_v11_baseline,
    run_mesh_pubsub_htl_gossipsub_v11_topic_mesh_baseline, run_mesh_pubsub_htl_inv_want_baseline,
    run_mesh_pubsub_htl_plumtree_baseline, run_mesh_pubsub_htl_plumtree_baseline_with_timer,
    run_mesh_pubsub_htl_plumtree_topic_mesh_baseline, run_mesh_pubsub_sweep,
    run_mesh_pubsub_workload, FollowDistribution, HtlBaselineMode, MeshPubsubSweepResult,
    MeshPubsubWorkloadConfig, MeshPubsubWorkloadReport, NostrWorkloadParams, WorkloadTopology,
};
pub use mesh_sim::{
    run_parameter_sweep, CashuIncentiveConfig, LocalResourceStats, NodeStrategyProfile,
    RetrievalStats, RetrievalTimingMode, SimConfig, SimEvent, SimStats, Simulation, SweepResult,
    TopologyStats,
};
pub use mint_client::{LocalMintClient, MintClient};
#[cfg(feature = "nostr")]
pub use nostr_mesh::NostrMesh;
pub use ws_relay::WsRelay;

// Re-export types from hashtree-network for convenience
pub use hashtree_network::{
    PoolConfig, PoolSettings, RequestDispatchConfig, ResponseBehaviorConfig, SelectionStrategy,
    SignalingMessage,
};

// Re-export hashtree types for convenience
pub use hashtree_core::{Cid, HashTree, HashTreeConfig, MemoryStore, Store};
