use clap::{Args, Subcommand, ValueEnum};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Subcommand)]
pub(crate) enum TorrentCommand {
    /// List files in a .torrent without opening network connections
    Inspect { torrent: PathBuf },
    /// Create torrent metadata from a locally available immutable snapshot
    Export {
        #[command(flatten)]
        snapshot: SnapshotArgs,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Seed a locally available immutable snapshot until interrupted
    Seed {
        #[command(flatten)]
        snapshot: SnapshotArgs,
        /// Optional .torrent destination (must not already exist)
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        network: NetworkArgs,
    },
    /// Read a selected file or byte range from a .torrent or magnet
    Get {
        source: String,
        /// Exact path from 'torrent inspect', or numeric file index
        file: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        /// Byte count; omitted means through the end of the file
        #[arg(long)]
        length: Option<u64>,
        /// Destination must not exist; omitted writes binary data to stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        network: NetworkArgs,
    },
    /// Import one file into normal encrypted hashtree storage and pin it
    Import {
        source: String,
        file: String,
        /// Store imported content publicly, without hashtree encryption
        #[arg(long)]
        unencrypted: bool,
        #[command(flatten)]
        network: NetworkArgs,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum TorrentFormat {
    V1,
    V2,
    Hybrid,
}

#[derive(Args)]
pub(crate) struct SnapshotArgs {
    /// Immutable nhash or raw hash; mutable names must first be resolved explicitly
    pub cid: String,
    /// Public torrent file/directory name
    #[arg(long)]
    pub name: String,
    /// Explicitly publish decrypted bytes from encrypted content
    #[arg(long)]
    pub allow_plaintext: bool,
    #[arg(long, value_enum, default_value = "hybrid")]
    pub format: TorrentFormat,
    #[arg(long, default_value_t = 262144)]
    pub piece_size: usize,
}

#[derive(Args)]
pub(crate) struct NetworkArgs {
    /// TCP/uTP listen address; set a reachable interface for public seeding
    #[arg(long, default_value = "127.0.0.1:0")]
    pub listen: SocketAddr,
    /// Connect to a numeric peer address (repeatable)
    #[arg(long)]
    pub peer: Vec<SocketAddr>,
    /// Enable DHT discovery
    #[arg(long)]
    pub dht: bool,
    /// Numeric DHT bootstrap address (repeatable; requires --dht)
    #[arg(long, requires = "dht")]
    pub bootstrap: Vec<SocketAddr>,
    /// Allow tracker URLs supplied by the torrent or magnet
    #[arg(long)]
    pub trackers: bool,
    /// Maximum bytes retained in the in-memory piece cache
    #[arg(long, default_value_t = 67108864)]
    pub cache_bytes: usize,
    /// Timeout for each range/metadata request, in seconds
    #[arg(long, default_value_t = 60)]
    pub timeout: u64,
    #[arg(long, default_value_t = 40)]
    pub connections: u16,
    /// Bytes per second; 0 is unlimited
    #[arg(long, default_value_t = 0)]
    pub upload_limit: u32,
    /// Bytes per second; 0 is unlimited
    #[arg(long, default_value_t = 0)]
    pub download_limit: u32,
}

impl NetworkArgs {
    pub fn options(self) -> hashtree_bittorrent::NetworkOptions {
        hashtree_bittorrent::NetworkOptions {
            listen: self.listen,
            peers: self.peer,
            dht: self.dht,
            bootstrap: self.bootstrap,
            trackers: self.trackers,
            cache_bytes: self.cache_bytes,
            timeout: std::time::Duration::from_secs(self.timeout),
            connections: self.connections,
            upload_limit: self.upload_limit,
            download_limit: self.download_limit,
        }
    }
}
