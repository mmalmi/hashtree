//! Hashtree blob exchange over FIPS endpoint bytes.
//!
//! FIPS owns peer discovery, signaling, and underlay transports. The minimal
//! surface provides verified TCP blob requests and optional same-host service
//! reuse. The older mesh request and pubsub APIs are isolated behind the
//! explicit `legacy-mesh` feature for existing CLI integration; the TCP path
//! never falls back to that protocol.

mod same_host;
mod tcp_blob;

pub use same_host::{SameHostBlobStore, SameHostBlobStoreConfig, SameHostBlobStoreError};
pub use tcp_blob::{
    encode_tcp_blob_request, encode_tcp_blob_response_header, TcpBlobTransport,
    TcpBlobTransportConfig, TcpBlobTransportError, TCP_BLOB_CAPABILITY, TCP_BLOB_MAX_BYTES,
    TCP_BLOB_SERVICE_PORT,
};

#[cfg(feature = "legacy-mesh")]
mod legacy_mesh;
#[cfg(feature = "legacy-mesh")]
pub use legacy_mesh::*;
