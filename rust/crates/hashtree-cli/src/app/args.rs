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
  nostr-index  Query local Nostr event indexes
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
  nostr-index  Query local Nostr event indexes
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
        /// Store locally only; skip configured Blossom/file-server pushes
        #[arg(long, alias = "no-blossom-push")]
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
        /// Send blob bodies even when the server's preflight says they exist
        #[arg(long)]
        force: bool,
        /// Push only the root blob, not the full reachable DAG
        #[arg(long)]
        shallow: bool,
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

    /// Query local hashtree-backed Nostr event indexes
    NostrIndex {
        #[command(subcommand)]
        command: Box<NostrIndexCommands>,
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
        /// Build and retain the recovery copy under this directory before replacing data.mdb
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
        /// Keep the original data.mdb as a .bak file after swapping
        #[arg(long)]
        keep_backup: bool,
    },
    /// Remove blobs unreachable from the root in a closed, dedicated Nostr index store
    RetainNostrRoot {
        /// Durable crawl-state JSON whose root is authoritative
        #[arg(long)]
        state_file: PathBuf,
        /// Delete unreachable blobs; without this flag the command is a dry run
        #[arg(long)]
        apply: bool,
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
    /// Manage the local adaptive LMDB storage pool
    Pool {
        #[command(subcommand)]
        command: PoolCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum PoolCommands {
    /// Show members, placement usage, lifecycle state, and configured limits
    Status,
    /// Add one empty storage member; creates the pool catalog if needed
    Add {
        /// Empty LMDB directory for this member
        path: PathBuf,
        /// Logical member capacity in GB
        #[arg(long)]
        capacity_gb: u64,
        /// LMDB virtual map size in GB (defaults to capacity)
        #[arg(long)]
        map_size_gb: Option<u64>,
        /// Optional directory for large external blob packs
        #[arg(long)]
        external_dir: Option<PathBuf>,
        /// Spill blobs at or above this size
        #[arg(long, default_value_t = 65_536, requires = "external_dir")]
        external_min_bytes: u64,
        /// Target size in MiB for external pack files
        #[arg(long, requires = "external_dir")]
        external_pack_mib: Option<u64>,
        /// Do not fsync external blob files before committing their references
        #[arg(long, requires = "external_dir")]
        external_no_sync: bool,
        /// Per-process concurrent read limit for this member
        #[arg(long, default_value_t = 64)]
        max_reads: u32,
        /// Per-process concurrent write limit for this member
        #[arg(long, default_value_t = 16)]
        max_writes: u32,
        /// Prefer writes elsewhere and stop promotion at this fill percentage
        #[arg(long, default_value_t = 85)]
        temperature_high_percent: u8,
        /// Under pressure, demote cold blobs until this fill percentage
        #[arg(long, default_value_t = 70)]
        temperature_low_percent: u8,
    },
    /// Change capacity, concurrency, and/or temperature watermarks
    Configure {
        /// Stable member UUID from `storage pool status`
        id: String,
        /// New logical member capacity in GB
        #[arg(long)]
        capacity_gb: Option<u64>,
        /// New per-process concurrent read limit
        #[arg(long)]
        max_reads: Option<u32>,
        /// New per-process concurrent write limit
        #[arg(long)]
        max_writes: Option<u32>,
        /// New automatic promotion high watermark percentage
        #[arg(long)]
        temperature_high_percent: Option<u8>,
        /// New automatic cold-demotion low watermark percentage
        #[arg(long)]
        temperature_low_percent: Option<u8>,
    },
    /// Stop new placement on a member and begin verified evacuation
    Drain { id: String },
    /// Run a bounded drain/rebalance maintenance pass
    Maintain {
        #[arg(long, default_value_t = 1_000)]
        max_items: usize,
        /// Maximum blobs committed together; memory is also capped internally
        #[arg(long, default_value_t = 256)]
        batch_items: usize,
    },
    /// Run one bounded automatic temperature-balancing cycle now
    BalanceTemperature {
        /// Maximum blobs attempted in this cycle
        #[arg(long)]
        max_moves: Option<usize>,
        /// Maximum GiB streamed in this cycle
        #[arg(long)]
        max_bytes_gb: Option<u64>,
        /// Maximum simultaneous streamed moves
        #[arg(long)]
        max_concurrency: Option<usize>,
    },
    /// Hash-verified, resumable copy from an existing LMDB into the pool
    MigrateLmdb {
        /// Existing source LMDB directory
        #[arg(long)]
        source: PathBuf,
        /// Existing source external-blob directory, if configured
        #[arg(long)]
        source_external_dir: Option<PathBuf>,
        /// Durable cursor file unique to this source
        #[arg(long)]
        state_file: PathBuf,
        /// Blobs per committed migration batch
        #[arg(long, default_value_t = 256)]
        batch_size: usize,
        /// Maximum MiB of complete blob payloads retained per pool write
        #[arg(long, default_value_t = 64)]
        max_buffer_mib: u64,
        /// Concurrent reads for distinct unpacked source external-blob files
        #[arg(long, default_value_t = 4)]
        source_read_concurrency: usize,
        /// Close and reopen source/target LMDB mappings after this many batches
        #[arg(long, default_value_t = 256)]
        reopen_batches: usize,
        /// Stop after this many blobs in this invocation
        #[arg(long)]
        max_items: Option<usize>,
        /// Continue an interrupted pass; a completed cursor starts a fresh pass
        #[arg(long)]
        resume: bool,
    },
    /// Remove a fully drained member from the manifest
    Remove { id: String },
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
    /// Publish existing profile index roots without rebuilding them
    PublishProfileIndexes,
    /// Rebuild stored Nostr event indexes from trusted local event blobs
    RebuildEventIndex,
    /// Crawl and index Nostr events for authors in the social graph
    Index {
        #[command(flatten)]
        options: Box<SocialGraphIndexArgs>,
    },
}

#[derive(clap::Args)]
pub(crate) struct SocialGraphIndexArgs {
    /// Warm the social graph for this many seconds before indexing
    #[arg(long, default_value_t = 0)]
    pub(crate) warm_secs: u64,
    /// Graph crawl depth to use while warming (default: config nostr.social_graph_crawl_depth)
    #[arg(long)]
    pub(crate) crawl_depth: Option<u32>,
    /// Ignore existing graph frontier state and refetch from the root
    #[arg(long, default_value_t = false)]
    pub(crate) full_graph_recrawl: bool,
    /// Maximum follow distance to include in the post index (default: config nostr.social_graph_crawl_depth)
    #[arg(long)]
    pub(crate) max_follow_distance: Option<u32>,
    /// Maximum number of authors to crawl from the graph
    #[arg(long, default_value_t = 64)]
    pub(crate) max_authors: usize,
    /// Maximum authors processed by this process before a durable resumable exit
    #[arg(long)]
    pub(crate) max_authors_per_run: Option<usize>,
    /// Maximum live index size in MiB
    #[arg(long, default_value_t = 256)]
    pub(crate) max_live_mb: u64,
    /// Maximum number of kept events per author
    #[arg(long, default_value_t = 256)]
    pub(crate) per_author_event_limit: usize,
    /// Maximum kept events per author and kind (overrides the aggregate author limit)
    #[arg(long)]
    pub(crate) per_author_kind_event_limit: Option<usize>,
    /// Maximum kept bytes per author before the global live cap is applied
    #[arg(long)]
    pub(crate) per_author_live_bytes: Option<u64>,
    /// Relay query author batch size
    #[arg(long, default_value_t = 64)]
    pub(crate) author_batch_size: usize,
    /// Authors committed to the resumable index root per durable checkpoint
    #[arg(long, default_value_t = 8)]
    pub(crate) checkpoint_authors: usize,
    /// Events applied per bounded Hashtree index commit
    #[arg(long, default_value_t = 32_768)]
    pub(crate) index_commit_batch_size: usize,
    /// Fetch and durably stage individual events without building query indexes
    #[arg(long, default_value_t = false)]
    pub(crate) stage_only: bool,
    /// Build query indexes from durably staged events without contacting relays
    #[arg(long, default_value_t = false)]
    pub(crate) project_staged: bool,
    /// Build a staged corpus through a disk-backed ordered spool, then
    /// stream each final index once instead of incrementally rewriting it
    #[arg(long, default_value_t = false, requires = "project_staged")]
    pub(crate) bulk_project_staged: bool,
    /// Separate Hashtree data directory for staged event blobs and fetch state
    #[arg(long)]
    pub(crate) staging_data_dir: Option<PathBuf>,
    /// Maximum staged authors combined into one projection batch
    #[arg(long, default_value_t = 64)]
    pub(crate) projection_authors: usize,
    /// Soft maximum staged events combined into one projection batch
    #[arg(long, default_value_t = 65_536)]
    pub(crate) projection_event_limit: usize,
    /// Wait for the staging watermark when projection catches up
    #[arg(long, default_value_t = false)]
    pub(crate) projection_follow: bool,
    /// Maximum children per Hashtree B-tree node
    #[arg(long, default_value_t = 256)]
    pub(crate) btree_order: usize,
    /// Maximum independent B-tree subtrees updated concurrently
    #[arg(long, default_value_t = 4)]
    pub(crate) btree_update_concurrency: usize,
    /// Number of graph-crawl author batches to fetch concurrently during warmup
    #[arg(long, default_value_t = 4)]
    pub(crate) concurrent_batches: usize,
    /// Relay fetch timeout in seconds
    #[arg(long, default_value_t = 10)]
    pub(crate) fetch_timeout_secs: u64,
    /// Maximum event size accepted from relays, in bytes
    #[arg(long)]
    pub(crate) relay_event_max_bytes: Option<u32>,
    /// Fetch recent relay pages without author filters and filter locally by social graph
    #[arg(long, default_value_t = false)]
    pub(crate) global_relay_scan: bool,
    /// Page each author's relay history until exhausted instead of keeping only the newest per-author window
    #[arg(long, default_value_t = false)]
    pub(crate) full_author_history: bool,
    /// HTTP URL returning newline-delimited author pubkeys to index
    #[arg(long)]
    pub(crate) author_allowlist_url: Option<String>,
    /// Only use relays that advertise NIP-77 negentropy support via NIP-11
    #[arg(long, default_value_t = false)]
    pub(crate) negentropy_only: bool,
    /// Number of events to request per relay page in global relay scan mode
    #[arg(long, default_value_t = 1_000)]
    pub(crate) relay_page_size: usize,
    /// Maximum pages to fetch per relay in global relay scan mode
    #[arg(long, default_value_t = 10)]
    pub(crate) max_relay_pages: usize,
    /// Stop after seeing at least this many raw relay events
    #[arg(long)]
    pub(crate) max_events_seen: Option<usize>,
    /// Restrict indexing to these kinds (repeatable)
    #[arg(long = "kind")]
    pub(crate) kinds: Vec<u16>,
    /// Relay URLs to use for this index run (repeatable, overrides config relays)
    #[arg(long = "relay")]
    pub(crate) relays: Vec<String>,
}

#[derive(Subcommand)]
pub(crate) enum NostrIndexCommands {
    /// Import signed Nostr events into the local hashtree-backed index
    Import {
        /// Stored event index root to append to (nhash or raw CID; defaults to latest local index root)
        #[arg(long)]
        root: Option<String>,
        /// JSON file containing an event array or object with an events array
        #[arg(long = "events")]
        events_file: PathBuf,
        /// Output path for the import report (default stdout; use "-" for stdout)
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Query stored events with normal Nostr filter JSON
    Query {
        /// Stored event index root (nhash or raw CID; defaults to latest local index root)
        #[arg(long)]
        root: Option<String>,
        /// Nostr filter JSON object, filter array, or REQ envelope
        #[arg(
            long,
            required_unless_present = "filter_file",
            conflicts_with = "filter_file"
        )]
        filter: Option<String>,
        /// Path to a Nostr filter JSON file
        #[arg(
            long = "filter-file",
            required_unless_present = "filter",
            conflicts_with = "filter"
        )]
        filter_file: Option<PathBuf>,
        /// Maximum events to return after merging all matching filters
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Output path (default stdout; use "-" for stdout)
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Exhaustively audit a completed bulk projection using read-only live-data views
    AuditBulkProjection {
        /// Durable staging directory whose exact frozen state produced the candidate
        #[arg(long = "staging-data-dir")]
        staging_data_dir: PathBuf,
        /// Required CAS pin for the selected v2 or v3 bulk-projection state
        #[arg(long = "expected-state-sha256")]
        expected_state_sha256: String,
        /// Audit the exact v3 Candidate and derive all authority from its sealed state
        #[arg(long = "v3-candidate")]
        v3_candidate: bool,
        /// Required CAS pin for nostr-stage/crawl-state.json
        #[arg(
            long = "expected-stage-state-sha256",
            required_unless_present = "v3_candidate",
            conflicts_with = "v3_candidate"
        )]
        expected_stage_state_sha256: Option<String>,
        /// Required SHA-256 of canonical JSON for the trusted crawl policy
        #[arg(
            long = "expected-policy-sha256",
            required_unless_present = "v3_candidate",
            conflicts_with = "v3_candidate"
        )]
        expected_policy_sha256: Option<String>,
        /// Optional additional pin for the retained v2 profile-distance seal
        #[arg(
            long = "expected-profile-distance-seal-sha256",
            conflicts_with = "v3_candidate"
        )]
        expected_profile_distance_seal_sha256: Option<String>,
        /// Canonical profile-search v3 rank-decisions JSONL
        #[arg(
            long = "profile-rank-decisions-file",
            required_unless_present_any = ["allow_recovery_tranche", "v3_candidate"],
            conflicts_with = "v3_candidate"
        )]
        profile_rank_decisions_file: Option<PathBuf>,
        /// Exact SHA-256 of the rank-decisions JSONL bytes
        #[arg(
            long = "expected-profile-rank-decisions-file-sha256",
            required_unless_present_any = ["allow_recovery_tranche", "v3_candidate"],
            conflicts_with = "v3_candidate"
        )]
        expected_profile_rank_decisions_file_sha256: Option<String>,
        /// Profile-search v3 rank-decision provenance report
        #[arg(
            long = "profile-rank-decisions-report",
            required_unless_present_any = ["allow_recovery_tranche", "v3_candidate"],
            conflicts_with = "v3_candidate"
        )]
        profile_rank_decisions_report: Option<PathBuf>,
        /// Exact SHA-256 of the rank-decision report bytes
        #[arg(
            long = "expected-profile-rank-decisions-report-sha256",
            required_unless_present_any = ["allow_recovery_tranche", "v3_candidate"],
            conflicts_with = "v3_candidate"
        )]
        expected_profile_rank_decisions_report_sha256: Option<String>,
        /// Trusted complete allowlist author count
        #[arg(
            long = "expected-full-author-count",
            required_unless_present = "v3_candidate",
            conflicts_with = "v3_candidate"
        )]
        expected_full_author_count: Option<usize>,
        /// Permit a non-cutover internal recovery-tranche audit
        #[arg(long = "allow-recovery-tranche", conflicts_with = "v3_candidate")]
        allow_recovery_tranche: bool,
        /// B-tree order used by a v2 bulk projection (v3 derives it from sealed state)
        #[arg(long, conflicts_with = "v3_candidate")]
        btree_order: Option<usize>,
        /// Maximum exact key/CID rows read per parity page
        #[arg(long, default_value_t = 4096)]
        page_size: usize,
        /// Number of real events compared for each deterministic list query
        #[arg(long, default_value_t = 32)]
        query_limit: usize,
        /// New absolute output path outside both audited data trees
        #[arg(long, short)]
        out: PathBuf,
    },
    /// Pin an audited v2 candidate and rotate into crash-safe v3 append mode
    PrepareBulkTranche {
        /// Durable staging directory containing the immutable author segments
        #[arg(long = "staging-data-dir")]
        staging_data_dir: PathBuf,
        /// Exact ordered author allowlist used by the pinned crawl policy
        #[arg(long = "eligible-authors")]
        eligible_authors: PathBuf,
        /// Required CAS pin for nostr-index/bulk-projection-v2/state.json
        #[arg(long = "expected-v2-state-sha256")]
        expected_v2_state_sha256: String,
        /// Required CAS pin for nostr-stage/crawl-state.json
        #[arg(long = "expected-stage-state-sha256")]
        expected_stage_state_sha256: String,
        /// Exhaustive audit JSON for the exact terminal v2 candidate
        #[arg(long = "audit-evidence")]
        audit_evidence: PathBuf,
        /// Canonical Iris Social profile rank-decisions JSONL
        #[arg(long = "profile-rank-decisions-file")]
        profile_rank_decisions_file: PathBuf,
        /// Exact SHA-256 of the profile rank-decisions JSONL bytes
        #[arg(long = "expected-profile-rank-decisions-file-sha256")]
        expected_profile_rank_decisions_file_sha256: String,
        /// Canonical Iris Social rank-decisions provenance report
        #[arg(long = "profile-rank-decisions-report")]
        profile_rank_decisions_report: PathBuf,
        /// Exact SHA-256 of the profile rank-decisions report bytes
        #[arg(long = "expected-profile-rank-decisions-report-sha256")]
        expected_profile_rank_decisions_report_sha256: String,
        /// Actual root resolved from the externally serving network event
        #[arg(long = "serving-root")]
        serving_root: String,
        /// Exact signed externally resolved serving event JSON
        #[arg(long = "serving-event")]
        serving_event: PathBuf,
        /// Exact event id returned by the external resolver
        #[arg(long = "serving-event-id")]
        serving_event_id: String,
        /// Authoritative publisher pubkey expected to own the serving pointer
        #[arg(long = "serving-publisher-pubkey")]
        serving_publisher_pubkey: String,
        /// Tree name resolved by the serving event
        #[arg(long = "serving-tree-name")]
        serving_tree_name: String,
        /// B-tree order pinned for subsequent tranche builds
        #[arg(long, default_value_t = 64)]
        btree_order: usize,
        /// Maximum independent B-tree subtree updates pinned for later builds
        #[arg(long, default_value_t = 4)]
        btree_update_concurrency: usize,
        /// Maximum staged events durably replayed before a cursor checkpoint
        #[arg(long, default_value_t = 8192)]
        index_commit_batch_size: usize,
        /// Output path for transition evidence (default stdout; use "-" for stdout)
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Replay immutable staged segments into the v3 working spool and stores
    AppendBulkTranche {
        /// Durable staging directory containing the immutable author segments
        #[arg(long = "staging-data-dir")]
        staging_data_dir: PathBuf,
        /// Required CAS pin for nostr-index/bulk-projection-v3/state.json
        #[arg(long = "expected-state-sha256")]
        expected_state_sha256: String,
        /// Maximum complete staged segments to append in this invocation
        #[arg(long = "max-segments", default_value_t = 1024)]
        max_segments: usize,
        /// Output path for transition evidence (default stdout; use "-" for stdout)
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Freeze the exact durable v3 working prefix before index construction
    FreezeBulkTranche {
        /// Durable staging directory containing the immutable author segments
        #[arg(long = "staging-data-dir")]
        staging_data_dir: PathBuf,
        /// Required CAS pin for nostr-index/bulk-projection-v3/state.json
        #[arg(long = "expected-state-sha256")]
        expected_state_sha256: String,
        /// Exact durable author boundary to freeze
        #[arg(long = "through-author")]
        through_author: usize,
        /// Output path for transition evidence (default stdout; use "-" for stdout)
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Build a frozen v3 tranche into a crash-resumable canonical candidate
    BuildBulkTranche {
        /// Durable staging directory containing the frozen author segments
        #[arg(long = "staging-data-dir")]
        staging_data_dir: PathBuf,
        /// Required CAS pin for nostr-index/bulk-projection-v3/state.json
        #[arg(long = "expected-state-sha256")]
        expected_state_sha256: String,
        /// Maximum missing index roots to build in this invocation
        #[arg(long = "max-indexes", default_value_t = 9)]
        max_indexes: usize,
        /// Output path for transition evidence (default stdout; use "-" for stdout)
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Detach damaged derived indexes before closed-store retention and rebuild
    PrepareTimeRepair {
        /// Durable crawl state whose root will be replaced atomically
        #[arg(long = "state-file")]
        state_file: PathBuf,
        /// Publish the prepared root; without this flag the command only validates inputs
        #[arg(long)]
        apply: bool,
    },
    /// Rebuild every derived event index from the authoritative durable index
    RepairReplaceable {
        /// Durable crawl state whose root will be replaced atomically
        #[arg(long = "state-file")]
        state_file: PathBuf,
        /// Durable staging store used to recover any missing indexed events
        #[arg(long = "staging-data-dir")]
        staging_data_dir: PathBuf,
        /// Ordered author allowlist used by the durable crawl state
        #[arg(long = "eligible-authors")]
        eligible_authors: PathBuf,
        /// Maximum author/kind/time entries held per scan page
        #[arg(long, default_value_t = 8192)]
        page_size: usize,
        /// B-tree order used for the rebuilt index
        #[arg(long, default_value_t = 64)]
        btree_order: usize,
        /// Apply the repair; without this flag the command only validates inputs
        #[arg(long)]
        apply: bool,
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
