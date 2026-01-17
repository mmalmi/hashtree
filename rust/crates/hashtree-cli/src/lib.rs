pub mod config;
pub mod fetch;
pub mod server;
pub mod storage;
pub mod sync;

#[cfg(feature = "p2p")]
pub mod webrtc;
#[cfg(not(feature = "p2p"))]
pub mod webrtc_stub;
#[cfg(not(feature = "p2p"))]
pub use webrtc_stub as webrtc;

pub use config::Config;
pub use hashtree_resolver::nostr::{NostrRootResolver, NostrResolverConfig};
pub use hashtree_resolver::{Keys as NostrKeys, ResolverEntry, ResolverError, RootResolver, ToBech32 as NostrToBech32};
pub use server::HashtreeServer;
pub use storage::{
    CachedRoot, HashtreeStore, TreeMeta, StorageByPriority,
    PRIORITY_OWN, PRIORITY_FOLLOWED, PRIORITY_OTHER,
};
pub use fetch::{FetchConfig, Fetcher};
pub use sync::{BackgroundSync, SyncConfig, SyncPriority, SyncStatus, SyncTask};
pub use webrtc::{WebRTCState, ConnectionState};
#[cfg(feature = "p2p")]
pub use webrtc::{ContentStore, DataMessage, PeerClassifier, PeerId, PeerPool, PoolConfig, PoolSettings, WebRTCConfig, WebRTCManager};
