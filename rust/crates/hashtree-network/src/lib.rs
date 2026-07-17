//! Adaptive routing across opaque, hash-verified blob sources.
//!
//! Transport addresses, peer selection, settlement, and application writes
//! belong to route implementations and their owners. This crate supplies the
//! read-only outer router, a Store-shaped routed-read facade, and the explicit
//! boundary that owns one Hashtree mesh-forwarding decision.

mod blob_router;
mod mesh_forwarding_route;
mod routed_store;

pub use blob_router::{
    BlobRouteEntry, BlobRouteIdentity, BlobRouteOutcomeSnapshot, BlobRouter, BlobRouterConfig,
};
pub use mesh_forwarding_route::MeshForwardingRoute;
pub use routed_store::RoutedStore;
