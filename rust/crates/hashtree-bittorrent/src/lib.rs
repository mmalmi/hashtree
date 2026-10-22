//! BitTorrent interoperability is inert until an application explicitly starts a session.
//! The native engine additionally requires the non-default `libtorrent` feature.

mod config;
mod snapshot;
pub use config::{NetworkOptions, MAX_METADATA_BYTES, MAX_READ_BYTES};
pub use snapshot::{File, Snapshot};

#[cfg(feature = "libtorrent")]
mod ffi;
#[cfg(feature = "libtorrent")]
mod route;
#[cfg(feature = "libtorrent")]
mod torrent;
#[cfg(feature = "libtorrent")]
pub use route::{BlobMapping, TorrentRoute};
#[cfg(feature = "libtorrent")]
pub use torrent::{Format, Metadata, Seed, Stats, Torrent};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("BitTorrent: {0}")]
    Engine(String),
    #[error("torrent read timed out; content availability is unknown")]
    Timeout,
    #[error("torrent operation cancelled")]
    Cancelled,
    #[error("encrypted content requires explicit plaintext export permission")]
    PlaintextPermission,
    #[error("hashtree content is missing or invalid")]
    Content,
    #[error(transparent)]
    Store(#[from] hashtree_core::StoreError),
    #[error(transparent)]
    Tree(#[from] hashtree_core::HashTreeError),
    #[error("background torrent operation failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, Error>;
