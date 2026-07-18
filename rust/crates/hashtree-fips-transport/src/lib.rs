//! Hashtree blob routes over FIPS endpoint bytes.
//!
//! FIPS owns authenticated identity, peer discovery, signaling, routing, and
//! underlay transports. The TCP/FIPS route provides verified blob requests and
//! optional same-host service reuse. It is a fast path within a higher-level
//! Store or resolver composition, which may continue through other routes when
//! this route returns no result.
//!
//! [`hashtree-network`](https://docs.rs/hashtree-network) remains Hashtree's
//! canonical decentralized content-routing layer and owns mesh forwarding and
//! HTL. This crate is the route-agnostic process and peer adapter: it carries
//! the same `BlobRequest { hash, htl }` contract without duplicating mesh
//! selection, forwarding, or framing. Resolver composition happens above it.

mod endpoint;
mod provider_route;
mod tcp_blob;

pub use endpoint::*;
pub use fips_core::{FipsEndpoint, PeerIdentity};
pub use provider_route::{FipsBlobRoute, FipsBlobRouteError};
pub use tcp_blob::{
    InboundBlobPolicy, TcpBlobPeerRoute, TcpBlobTransport, TcpBlobTransportConfig,
    TcpBlobTransportError, WeakTcpBlobPeerRoute, TCP_BLOB_CAPABILITY, TCP_BLOB_MAX_BYTES,
    TCP_BLOB_SERVICE_PORT,
};
