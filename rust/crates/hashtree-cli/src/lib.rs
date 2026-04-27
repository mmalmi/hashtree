pub mod blossom_push;
pub mod bootstrap;
#[cfg(feature = "cashu")]
pub mod cashu;
#[cfg(feature = "cashu")]
pub mod cashu_cli;
pub mod cashu_helper;
pub mod config;
pub mod daemon;
pub mod diagnostics;
pub mod eviction;
pub mod fetch;
pub mod ignore_rules;
pub mod nostr_mirror;
pub mod nostr_relay;
pub mod pwa;
pub mod server;
pub mod storage;
pub mod sync;

#[cfg(feature = "p2p")]
pub mod webrtc;
#[cfg(not(feature = "p2p"))]
pub mod webrtc_stub;
#[cfg(not(feature = "p2p"))]
pub use webrtc_stub as webrtc;
#[cfg(feature = "p2p")]
pub mod p2p_common;

pub mod socialgraph;

#[cfg(test)]
pub mod test_support;

pub use config::Config;
pub use eviction::{spawn_background_eviction_task, BACKGROUND_EVICTION_INTERVAL};
pub use fetch::{FetchConfig, FetchProgress, FetchProgressSnapshot, Fetcher};
pub use hashtree_resolver::nostr::{NostrResolverConfig, NostrRootResolver};
pub use hashtree_resolver::{
    Keys as NostrKeys, ResolverEntry, ResolverError, RootResolver, ToBech32 as NostrToBech32,
};
pub use server::HashtreeServer;
pub use storage::{
    CachedRoot, HashtreeStore, StorageByPriority, TreeMeta, PRIORITY_FOLLOWED, PRIORITY_OTHER,
    PRIORITY_OWN,
};
pub use sync::{BackgroundSync, SyncConfig, SyncPriority, SyncStatus, SyncTask};
#[cfg(feature = "p2p")]
pub use webrtc::{
    BluetoothBackendState, BluetoothConfig, ContentStore, DataMessage, LocalNostrBus,
    PeerClassifier, PeerId, PeerPool, PoolConfig, PoolSettings, SharedLocalNostrBus, WebRTCConfig,
    WebRTCManager,
};
pub use webrtc::{ConnectionState, WebRTCState};
