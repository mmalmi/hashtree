use clap::{Parser, Subcommand, ValueEnum};
use git_remote_htree::nostr_client::PullRequestStateFilter;
use std::path::PathBuf;

const CLI_HELP_TEMPLATE: &str = "\
{about-with-newline}\
\n{usage-heading} {usage}\n\n\
Options:\n\
{options}\
{after-help}";

#[cfg(feature = "fuse")]
const CLI_GROUPED_COMMANDS: &str = "\
\nDaemon Commands:
  start        Start the hashtree daemon
  reload       Reload daemon config by restarting with saved launch args
  stop         Stop the hashtree daemon
  status       Show daemon status (peers, storage, etc.)
  peer         Show connected P2P peers

Content Commands:
  add          Add file or directory to hashtree (like ipfs add)
  pwa          Capture an installable web app into hashtree
  load         Load/prefetch content into local hashtree storage
  get          Get/download content by CID
  cat          Output file content to stdout (like cat)
  push         Push content to file servers (Blossom)
  info         Get information about a CID

Storage Commands:
  pin          Pin content
  unpin        Unpin content
  pins         List all pinned content
  stats        Get storage statistics
  gc           Run garbage collection
  storage      Manage storage limits and eviction
  mount        Mount a hashtree via FUSE
  mounts       List active hashtree mounts

Publishing & Git Commands:
  publish      Publish a hash to Nostr under a ref name
  release      Manage published release trees
  repos        List published git repositories for yourself or another user
  pr           Pull request management

Update Commands:
  install      Install or upgrade an app from a hashtree release
  update       Self-update the htree binary itself

Identity & Social Commands:
  user         Show or set your nostr identity
  profile      Show or update your Nostr profile
  mirror       Manage mirrored authors
  follow       Follow a user (adds to your contact list)
  unfollow     Unfollow a user (removes from your contact list)
  following    List users you follow
  mute         Mute a user (adds to your mute list)
  unmute       Unmute a user (removes from your mute list)
  muted        List users you mute
  socialgraph  Social graph utilities

Wallet Commands:
  cashu        Manage Cashu wallet and accepted mints

General Commands:
  help         Print this message or the help of the given subcommand(s)";

#[cfg(not(feature = "fuse"))]
const CLI_GROUPED_COMMANDS: &str = "\
\nDaemon Commands:
  start        Start the hashtree daemon
  reload       Reload daemon config by restarting with saved launch args
  stop         Stop the hashtree daemon
  status       Show daemon status (peers, storage, etc.)
  peer         Show connected P2P peers

Content Commands:
  add          Add file or directory to hashtree (like ipfs add)
  pwa          Capture an installable web app into hashtree
  load         Load/prefetch content into local hashtree storage
  get          Get/download content by CID
  cat          Output file content to stdout (like cat)
  push         Push content to file servers (Blossom)
  info         Get information about a CID

Storage Commands:
  pin          Pin content
  unpin        Unpin content
  pins         List all pinned content
  stats        Get storage statistics
  gc           Run garbage collection
  storage      Manage storage limits and eviction
  mounts       List active hashtree mounts

Publishing & Git Commands:
  publish      Publish a hash to Nostr under a ref name
  release      Manage published release trees
  repos        List published git repositories for yourself or another user
  pr           Pull request management

Update Commands:
  install      Install or upgrade an app from a hashtree release
  update       Self-update the htree binary itself

Identity & Social Commands:
  user         Show or set your nostr identity
  profile      Show or update your Nostr profile
  mirror       Manage mirrored authors
  follow       Follow a user (adds to your contact list)
  unfollow     Unfollow a user (removes from your contact list)
  following    List users you follow
  mute         Mute a user (adds to your mute list)
  unmute       Unmute a user (removes from your mute list)
  muted        List users you mute
  socialgraph  Social graph utilities

Wallet Commands:
  cashu        Manage Cashu wallet and accepted mints

General Commands:
  help         Print this message or the help of the given subcommand(s)";

#[derive(Parser)]
#[command(name = "htree")]
#[command(version)]
#[command(about = "Content-addressed filesystem", long_about = None)]
#[command(help_template = CLI_HELP_TEMPLATE)]
#[command(after_help = CLI_GROUPED_COMMANDS)]
pub(crate) struct Cli {
    /// Data directory (default: ~/.hashtree/data)
    #[arg(long, global = true, env = "HTREE_DATA_DIR")]
    pub(crate) data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

impl Cli {
    /// Get the data directory, defaulting to ~/.hashtree/data
    pub(crate) fn data_dir(&self) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| hashtree_cli::config::get_hashtree_dir().join("data"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum StartMode {
    Normal,
    #[value(alias = "signal-only")]
    Assist,
}

impl From<StartMode> for hashtree_cli::config::ServerMode {
    fn from(value: StartMode) -> Self {
        match value {
            StartMode::Normal => Self::Normal,
            StartMode::Assist => Self::Assist,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    // ── Daemon ──────────────────────────────────────────────
    /// Start the hashtree daemon
    Start {
        /// Override daemon bind address from config
        #[arg(long)]
        addr: Option<String>,
        /// Override Nostr relays (comma-separated)
        #[arg(long)]
        relays: Option<String>,
        /// Override daemon mode (`normal` or `assist`)
        #[arg(long, value_enum)]
        mode: Option<StartMode>,
        /// Run in background (daemonize)
        #[arg(long)]
        daemon: bool,
        /// Log file for daemon mode (default: ~/.hashtree/logs/htree.log)
        #[arg(long, requires = "daemon")]
        log_file: Option<PathBuf>,
        /// PID file for daemon mode (default: ~/.hashtree/htree.pid)
        #[arg(long, requires = "daemon")]
        pid_file: Option<PathBuf>,
    },

    /// Stop the hashtree daemon
    Stop {
        /// PID file (default: ~/.hashtree/htree.pid)
        #[arg(long)]
        pid_file: Option<PathBuf>,
    },

    /// Reload the hashtree daemon config by restarting the daemon
    Reload {
        /// PID file (default: ~/.hashtree/htree.pid)
        #[arg(long)]
        pid_file: Option<PathBuf>,
    },

    /// Show daemon status (peers, storage, etc.)
    Status {
        /// Daemon address (default: 127.0.0.1:8080)
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
    },

    /// Show connected P2P peers
    Peer {
        /// Daemon address (default: 127.0.0.1:8080)
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
    },

    // ── Content ─────────────────────────────────────────────
    /// Add file or directory to hashtree (like ipfs add)
    Add {
        /// Path to file or directory
        path: PathBuf,
        /// Only compute hash, don't store
        #[arg(long)]
        only_hash: bool,
        /// Store as raw plaintext blobs without CHK encryption
        #[arg(long = "unencrypted", alias = "public")]
        unencrypted: bool,
        /// Include files ignored by .gitignore and common OS junk filters
        #[arg(long)]
        no_ignore: bool,
        /// Publish to Nostr under this ref name (e.g., "mydata" -> npub.../mydata)
        #[arg(long)]
        publish: Option<String>,
        /// Override content chunk size in bytes for stored files
        #[arg(long)]
        chunk_size: Option<usize>,
        /// Don't push to file servers (local only)
        #[arg(long)]
        local: bool,
    },

    /// Capture an installable web app into hashtree
    Pwa {
        #[command(subcommand)]
        command: PwaCommands,
    },

    /// Get/download content by CID
    Get {
        /// CID to retrieve
        cid: String,
        /// Output path (default: current dir, uses CID as filename)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Load/prefetch content into local hashtree storage
    Load {
        /// CID or htree:// target to retrieve into local storage
        cid: String,
    },

    /// Output file content to stdout (like cat)
    Cat {
        /// CID to read
        cid: String,
    },

    /// Push content to file servers (Blossom)
    Push {
        /// CID (hash or hash:key) to push
        cid: String,
        /// File server URL (overrides config)
        #[arg(long, short)]
        server: Option<String>,
    },

    /// Get information about a CID
    Info {
        /// CID to inspect
        cid: String,
    },

    // ── Pinning ─────────────────────────────────────────────
    /// Pin content
    Pin {
        /// CID, npub/repo, or htree:// target to pin
        cid: String,
    },

    /// Unpin content
    Unpin {
        /// CID, npub/repo, or htree:// target to unpin
        cid: String,
    },

    /// List all pinned content
    Pins,

    // ── Storage ─────────────────────────────────────────────
    /// Get storage statistics
    Stats {
        /// Daemon address for peer/network stats (default: 127.0.0.1:8080)
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
    },

    /// Run garbage collection
    Gc,

    /// Manage storage limits and eviction
    Storage {
        #[command(subcommand)]
        command: StorageCommands,
    },

    /// Mount a hashtree via FUSE
    #[cfg(feature = "fuse")]
    Mount {
        /// Target to mount (nhash, self/tree, npub/tree, or htree:// URL)
        target: String,
        /// Mount point directory (defaults to a new ./<target-name> directory; explicit mountpoints may be missing or empty)
        mountpoint: Option<PathBuf>,
        /// Visibility: public, link-visible, or private
        #[arg(long)]
        visibility: Option<String>,
        /// Link key for link-visible trees (hex)
        #[arg(long)]
        link_key: Option<String>,
        /// Use private visibility (NIP-44 to self)
        #[arg(long)]
        private: bool,
        /// Override Nostr relays (comma-separated)
        #[arg(long)]
        relays: Option<String>,
        /// Allow other users to access the mount
        #[arg(long)]
        allow_other: bool,
    },

    /// List active hashtree mounts
    Mounts {
        /// Print registry entries as JSON
        #[arg(long)]
        json: bool,
    },

    // ── Publishing & Git ────────────────────────────────────
    /// Publish a hash to Nostr under a ref name
    Publish {
        /// The ref name to publish under (e.g., "mydata" -> npub.../mydata)
        ref_name: String,
        /// The hash to publish (hex encoded)
        hash: String,
        /// Optional decryption key (hex encoded, for encrypted content)
        #[arg(long)]
        key: Option<String>,
    },

    /// Manage published release trees
    Release {
        #[command(subcommand)]
        command: ReleaseCommands,
    },

    /// Install or upgrade an app from a hashtree release reference
    Install {
        /// htree:// reference to the release latest pointer
        reference: String,
        /// Where to install (default: ~/.local/bin/<asset name> for plain
        /// binaries / binary-archives, current_exe()'s parent for app-bundle
        /// and appimage when in place)
        #[arg(long)]
        to: Option<PathBuf>,
        /// Don't download or install — just print the matched asset
        #[arg(long, conflicts_with = "download_only")]
        check: bool,
        /// Download only, don't install. Implies --to is the file path
        #[arg(long, conflicts_with = "check")]
        download_only: bool,
        /// Current installed version (used to compute "newer than" / skip
        /// re-install when --only-if-newer is set)
        #[arg(long, default_value = "0.0.0")]
        current_version: String,
        /// Override the target triple (defaults to the current host)
        #[arg(long)]
        target: Option<String>,
        /// Path within the release dir to read the manifest from
        #[arg(long, default_value = "release.json")]
        manifest_path: String,
        /// Override the asset kind (binary, app-bundle, appimage,
        /// binary-archive). Default: from manifest.
        #[arg(long)]
        kind: Option<String>,
        /// Set the executable bit after install (binary kind only)
        #[arg(long)]
        executable: bool,
        /// For binary-archive kind: name of the entry inside the archive
        /// to extract (eg `iris/iris`). Overrides manifest's `executable`.
        #[arg(long = "archive-entry")]
        archive_entry: Option<String>,
        /// Skip install if the manifest version is not newer than current
        #[arg(long)]
        only_if_newer: bool,
    },

    /// Self-update the htree binary itself.
    ///
    /// Refuses by default when the binary lives under a package-manager
    /// path (cargo, brew) so the package manager's metadata stays in sync.
    /// Pass `--force` to bypass and replace the binary directly.
    Update {
        /// Don't install, just print what would happen
        #[arg(long)]
        check: bool,
        /// Replace the binary in place even if it lives under cargo/brew
        #[arg(long)]
        force: bool,
    },

    /// Internal: detached background self-update check. Spawned by the
    /// startup hook; not meant to be run by users.
    #[command(hide = true, name = "__bg_check")]
    BgCheck,

    /// List published git repositories for yourself or another user
    Repos {
        /// Owner identity (defaults to self). Accepts alias, npub, or hex pubkey.
        owner: Option<String>,
    },

    /// Pull request management
    Pr {
        #[command(subcommand)]
        command: PrCommands,
    },

    // ── Identity & Social ───────────────────────────────────
    /// Show or set your nostr identity
    User {
        /// npub or nsec to set as active identity (omit to show current)
        identity: Option<String>,
    },

    /// Show or update your Nostr profile
    Profile {
        /// Set display name
        #[arg(long)]
        name: Option<String>,
        /// Set about/bio
        #[arg(long)]
        about: Option<String>,
        /// Set profile picture URL
        #[arg(long)]
        picture: Option<String>,
    },

    /// Manage mirrored authors
    Mirror {
        #[command(subcommand)]
        command: MirrorCommands,
    },

    /// Follow a user (adds to your contact list)
    Follow {
        /// npub of user to follow
        npub: String,
    },

    /// Unfollow a user (removes from your contact list)
    Unfollow {
        /// npub of user to unfollow
        npub: String,
    },

    /// List users you follow
    Following,

    /// Mute a user (adds to your mute list)
    Mute {
        /// npub of user to mute
        npub: String,
        /// Optional reason to include in the mute list
        #[arg(long)]
        reason: Option<String>,
    },

    /// Unmute a user (removes from your mute list)
    Unmute {
        /// npub of user to unmute
        npub: String,
    },

    /// List users you mute
    Muted,

    /// Social graph utilities
    Socialgraph {
        #[command(subcommand)]
        command: SocialGraphCommands,
    },

    // ── Wallet ──────────────────────────────────────────────
    /// Manage Cashu wallet and accepted mints
    Cashu {
        #[command(subcommand)]
        command: CashuCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum PrCommands {
    /// Create a pull request
    Create {
        /// Target repository (git remote alias, npub/reponame, or htree:// URL of the repo to PR into)
        repo: Option<String>,
        /// PR title
        #[arg(long, short)]
        title: String,
        /// PR description
        #[arg(long, short)]
        description: Option<String>,
        /// Source branch name (default: current branch)
        #[arg(long)]
        branch: Option<String>,
        /// Target branch (default: master)
        #[arg(long, default_value = "master")]
        target_branch: String,
        /// Clone URL for source repo (default: htree://self/<reponame>)
        #[arg(long)]
        clone_url: Option<String>,
    },
    /// List pull requests
    List {
        /// Target repository (git remote alias, npub/reponame, or htree:// URL)
        repo: Option<String>,
        /// PR state filter (default: open)
        #[arg(long, value_enum, default_value_t = PrListState::Open)]
        state: PrListState,
    },
}

#[derive(Subcommand)]
pub(crate) enum MirrorCommands {
    /// Mirror all readable hashtree trees for an author
    Add {
        /// npub of author to mirror continuously
        npub: String,
    },
    /// Stop mirroring an author's hashtree trees
    #[command(name = "rm", alias = "remove")]
    Rm {
        /// npub of author to stop mirroring
        npub: String,
    },
    /// List authors whose hashtree trees are mirrored continuously
    #[command(name = "ls", alias = "list")]
    Ls,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum PrListState {
    Open,
    Applied,
    Closed,
    Draft,
    All,
}

impl PrListState {
    pub(crate) fn to_filter(self) -> PullRequestStateFilter {
        match self {
            Self::Open => PullRequestStateFilter::Open,
            Self::Applied => PullRequestStateFilter::Applied,
            Self::Closed => PullRequestStateFilter::Closed,
            Self::Draft => PullRequestStateFilter::Draft,
            Self::All => PullRequestStateFilter::All,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum StorageCommands {
    /// Show storage usage statistics by priority tier
    Stats,
    /// List all indexed trees
    Trees,
    /// Manually trigger eviction
    Evict,
    /// Compact LMDB environments to reclaim freed pages on disk
    Compact {
        /// Specific LMDB environment directory to compact (repeatable)
        #[arg(long = "env-dir")]
        env_dirs: Vec<PathBuf>,
        /// Keep the original data.mdb as a .bak file after swapping
        #[arg(long)]
        keep_backup: bool,
    },
    /// Trim a specific LMDB blob environment down to a logical size limit
    TrimLmdb {
        /// LMDB environment directory to trim
        #[arg(long = "env-dir")]
        env_dir: PathBuf,
        /// Logical size target in GB
        #[arg(long = "max-gb")]
        max_gb: u64,
    },
    /// Verify blob integrity and delete corrupted entries
    Verify {
        /// Actually delete corrupted entries (default: dry-run)
        #[arg(long)]
        delete: bool,
        /// Also verify R2/S3 storage (slower)
        #[arg(long)]
        r2: bool,
    },
    /// Import missing blobs from configured R2/S3 storage into local storage
    ImportR2 {
        /// Concurrent object downloads for missing blobs
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Only list/compare source objects against local storage; do not download
        #[arg(long)]
        check_only: bool,
        /// Resume an interrupted scan from the saved last key
        #[arg(long)]
        resume: bool,
        /// Load the local blob list once and compare in memory
        #[arg(long)]
        fast_list: bool,
        /// Compare each R2 listing page against LMDB with bounded range scans
        #[arg(long)]
        stream_merge: bool,
        /// Import one explicit blob key or bare SHA-256 hash; repeatable
        #[arg(long = "key")]
        keys: Vec<String>,
        /// Import explicit blob keys or bare SHA-256 hashes from a newline-delimited file
        #[arg(long)]
        keys_file: Option<PathBuf>,
        /// Explicit R2/S3 key to start after
        #[arg(long)]
        start_after: Option<String>,
        /// Only scan R2/S3 keys whose names start with this prefix after the configured bucket prefix
        #[arg(long)]
        scan_prefix: Option<String>,
        /// Persist scan progress here
        #[arg(long)]
        state_file: Option<PathBuf>,
        /// Stop after this many listed objects, useful for spot checks
        #[arg(long)]
        max_objects: Option<usize>,
        /// Print progress every N listed objects
        #[arg(long, default_value_t = 5_000)]
        progress_every: usize,
        /// Sleep this many milliseconds between canonical object checks
        #[arg(long, default_value_t = 0)]
        scan_delay_ms: u64,
    },
}

#[derive(Subcommand)]
pub(crate) enum CashuCommands {
    /// Show Cashu wallet balances
    #[command(visible_alias = "status")]
    Balance {
        /// Show only one mint
        #[arg(long)]
        mint: Option<String>,
    },
    /// Create a Cashu top-up quote from the selected mint
    #[command(visible_alias = "load")]
    Topup {
        /// Amount in satoshis
        amount_sat: u64,
        /// Mint to use (defaults to configured default mint)
        #[arg(long)]
        mint: Option<String>,
    },
    /// Manage accepted Cashu mints
    Mint {
        #[command(subcommand)]
        command: CashuMintCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum PwaCommands {
    /// Export an installable web app into a pinned htree:// bundle
    Export {
        /// The source web app URL
        url: String,
        /// Print the export result as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum CashuMintCommands {
    /// List accepted mints
    List,
    /// Add an accepted mint
    Add {
        /// Mint base URL
        url: String,
        /// Also set as default mint
        #[arg(long = "default")]
        make_default: bool,
    },
    /// Remove an accepted mint
    Remove {
        /// Mint base URL
        url: String,
    },
    /// Set the default mint
    Default {
        /// Mint base URL
        url: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum SocialGraphCommands {
    /// Filter JSONL Nostr events to those within the social graph
    Filter {
        /// Max follow distance to allow (default: config nostr.max_write_distance)
        #[arg(long)]
        max_distance: Option<u32>,
        /// Overmute threshold (muters * threshold > followers)
        #[arg(long, default_value_t = 1.0)]
        overmute_threshold: f64,
    },
    /// Show local social graph statistics
    Stats,
    /// Warm the local social graph without building a post index
    Warm {
        /// Warm the social graph for this many seconds
        #[arg(long, default_value_t = 60)]
        secs: u64,
        /// Graph crawl depth to use while warming (default: config nostr.social_graph_crawl_depth)
        #[arg(long)]
        crawl_depth: Option<u32>,
        /// Ignore existing graph frontier state and refetch from the root
        #[arg(long, default_value_t = false)]
        full_graph_recrawl: bool,
        /// Relay URLs to use for this warm run (repeatable, overrides config relays)
        #[arg(long = "relay")]
        relays: Vec<String>,
        /// Relay query author batch size
        #[arg(long, default_value_t = 64)]
        author_batch_size: usize,
        /// Number of relay author batches to fetch concurrently
        #[arg(long, default_value_t = 4)]
        concurrent_batches: usize,
    },
    /// Save a social graph snapshot (nostr-social-graph binary format)
    Snapshot {
        /// Output file path (use "-" for stdout)
        #[arg(long, short)]
        out: PathBuf,
        /// Maximum number of nodes
        #[arg(long)]
        max_nodes: Option<usize>,
        /// Maximum number of edges
        #[arg(long)]
        max_edges: Option<usize>,
        /// Maximum follow distance
        #[arg(long)]
        max_distance: Option<u32>,
        /// Maximum edges per node
        #[arg(long)]
        max_edges_per_node: Option<usize>,
    },
    /// Rebuild the profile search index from trusted locally stored kind-0 events
    RebuildProfileIndex,
    /// Rebuild stored Nostr event indexes from trusted local event blobs
    RebuildEventIndex,
    /// Crawl and index Nostr events for authors in the social graph
    Index {
        /// Warm the social graph for this many seconds before indexing
        #[arg(long, default_value_t = 0)]
        warm_secs: u64,
        /// Graph crawl depth to use while warming (default: config nostr.social_graph_crawl_depth)
        #[arg(long)]
        crawl_depth: Option<u32>,
        /// Ignore existing graph frontier state and refetch from the root
        #[arg(long, default_value_t = false)]
        full_graph_recrawl: bool,
        /// Maximum follow distance to include in the post index (default: config nostr.social_graph_crawl_depth)
        #[arg(long)]
        max_follow_distance: Option<u32>,
        /// Maximum number of authors to crawl from the graph
        #[arg(long, default_value_t = 64)]
        max_authors: usize,
        /// Maximum live index size in MiB
        #[arg(long, default_value_t = 256)]
        max_live_mb: u64,
        /// Maximum number of kept events per author
        #[arg(long, default_value_t = 256)]
        per_author_event_limit: usize,
        /// Maximum kept bytes per author before the global live cap is applied
        #[arg(long)]
        per_author_live_bytes: Option<u64>,
        /// Relay query author batch size
        #[arg(long, default_value_t = 64)]
        author_batch_size: usize,
        /// Number of graph-crawl author batches to fetch concurrently during warmup
        #[arg(long, default_value_t = 4)]
        concurrent_batches: usize,
        /// Relay fetch timeout in seconds
        #[arg(long, default_value_t = 10)]
        fetch_timeout_secs: u64,
        /// Maximum event size accepted from relays, in bytes
        #[arg(long)]
        relay_event_max_bytes: Option<u32>,
        /// Fetch recent relay pages without author filters and filter locally by social graph
        #[arg(long, default_value_t = false)]
        global_relay_scan: bool,
        /// Page each author's relay history until exhausted instead of keeping only the newest per-author window
        #[arg(long, default_value_t = false)]
        full_author_history: bool,
        /// HTTP URL returning newline-delimited author pubkeys to index
        #[arg(long)]
        author_allowlist_url: Option<String>,
        /// Only use relays that advertise NIP-77 negentropy support via NIP-11
        #[arg(long, default_value_t = false)]
        negentropy_only: bool,
        /// Number of events to request per relay page in global relay scan mode
        #[arg(long, default_value_t = 1_000)]
        relay_page_size: usize,
        /// Maximum pages to fetch per relay in global relay scan mode
        #[arg(long, default_value_t = 10)]
        max_relay_pages: usize,
        /// Stop after seeing at least this many raw relay events
        #[arg(long)]
        max_events_seen: Option<usize>,
        /// Restrict indexing to these kinds (repeatable)
        #[arg(long = "kind")]
        kinds: Vec<u16>,
        /// Relay URLs to use for this index run (repeatable, overrides config relays)
        #[arg(long = "relay")]
        relays: Vec<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ReleaseCommands {
    /// Publish a version directory CID into a mutable release tree
    Publish {
        /// Mutable release tree name (repo releases usually use "releases/<repo>")
        tree_name: String,
        /// Version path within the release tree (for example: "v0.2.3" or "releases/v0.2.3")
        version_path: String,
        /// CID or nhash for the release directory to publish
        cid: String,
        /// Publish the version and repoint the sibling draft pointer instead of latest
        #[arg(long)]
        draft: bool,
        /// Don't push the updated release root to file servers
        #[arg(long)]
        local: bool,
    },
}

/// htree's own published release reference for self-update.
pub(crate) const HTREE_SELF_REFERENCE: &str =
    "htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhashtree/latest";
