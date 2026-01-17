//! Simulation tools for hashtree P2P protocols
//!
//! Provides simulation using the same code as production WebRTCStore with mock transports.
//!
//! ## Architecture
//!
//! - `webrtc_sim::Simulation` - uses GenericStore with mock transports
//! - `WsRelay` - WebSocket Nostr relay for integration testing

pub mod webrtc_sim;
pub mod ws_relay;

// Re-export main types from webrtc_sim
pub use webrtc_sim::{SimConfig, SimEvent, SimStats, Simulation, TopologyStats};
pub use ws_relay::WsRelay;

// Re-export types from hashtree-webrtc for convenience
pub use hashtree_webrtc::{PoolConfig, PoolSettings, SignalingMessage};

// Re-export hashtree types for convenience
pub use hashtree_core::{Cid, HashTree, HashTreeConfig, MemoryStore, Store};
