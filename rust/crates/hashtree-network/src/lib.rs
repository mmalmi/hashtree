//! Adaptive routing across opaque, hash-verified blob sources.
//!
//! Transport addresses, peer selection, settlement, and application writes
//! belong to route implementations and their owners. This crate supplies only
//! the read-only outer router and its bounded in-memory outcome state.

mod blob_router;

pub use blob_router::{
    BlobRouteEntry, BlobRouteIdentity, BlobRouteOutcomeSnapshot, BlobRouter, BlobRouterConfig,
};
