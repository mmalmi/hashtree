#![allow(unexpected_cfgs)]

pub(crate) mod blob_cache;
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

pub mod webrtc_stub;
pub use webrtc_stub as webrtc;
pub mod p2p_common_stub;
pub use p2p_common_stub as p2p_common;

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
pub use webrtc::{ConnectionState, WebRTCState};
