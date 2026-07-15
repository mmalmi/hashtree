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
//! HTL. Its integration here is gated by the `legacy-mesh` Cargo feature only
//! for compatibility with existing feature selections; the name does not mean
//! that HTL routing is deprecated or replaced. The TCP route does not silently
//! invoke the mesh or duplicate its framing; composition happens above it.

mod same_host;
mod tcp_blob;

pub use same_host::{SameHostBlobStore, SameHostBlobStoreConfig, SameHostBlobStoreError};
pub use tcp_blob::{
    TcpBlobPeerRoute, TcpBlobTransport, TcpBlobTransportConfig, TcpBlobTransportError,
    TCP_BLOB_CAPABILITY, TCP_BLOB_MAX_BYTES, TCP_BLOB_SERVICE_PORT,
};

#[cfg(feature = "legacy-mesh")]
mod legacy_mesh;
#[cfg(feature = "legacy-mesh")]
pub use legacy_mesh::*;
