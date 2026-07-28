use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{Context, Result};
use futures::{stream, StreamExt};
use hashtree_core::{nhash_decode, nhash_encode_full, Cid, NHashData};
use hashtree_nostr::{
    stored_event_from_nostr_sdk_event, CrawlConfig, CrawlReport, ListEventsOptions, NostrBridge,
    NostrEventStore, NostrEventStoreOptions, RelayFetchMode, StoredNostrEvent, VerifiedEvent,
};
use nostr::{EventId, Keys};
use nostr_sdk::{
    pool::RelayLimits, Client as NostrClient, ClientOptions, Event as NostrSdkEvent,
    Filter as NostrFilter,
};
use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use hashtree_cli::config::{ensure_keys, parse_npub};
use hashtree_cli::socialgraph::{self, SocialGraphBackend, SocialGraphCrawler};
use hashtree_cli::{Config, HashtreeStore};

mod bulk_projection;
pub(crate) use bulk_projection::{
    BulkEventBlobRepairOptions, BulkProfileRepairOptions, BulkProjectionAuditOptions,
    BulkTrancheAppendOptions, BulkTrancheBuildOptions, BulkTrancheFreezeOptions,
    BulkTranchePrepareOptions, BulkTrancheTransitionOutput,
};

const INDEX_DIR: &str = "nostr-index";
const LATEST_ROOT_FILE: &str = "latest-root.txt";
const LATEST_REPORT_FILE: &str = "latest-report.json";
const CHECKPOINT_ROOT_FILE: &str = "checkpoint-root.txt";
const CHECKPOINT_REPORT_FILE: &str = "checkpoint-report.json";
const CRAWL_STATE_FILE: &str = "crawl-state.json";
const CRAWL_LOCK_FILE: &str = "crawl.lock";
const CRAWL_STATE_VERSION: u32 = 1;
const STAGE_DIR: &str = "nostr-stage";
const STAGE_SEGMENTS_DIR: &str = "segments";
const STAGE_SEGMENT_CLAIMS_DIR: &str = "segment-claims";
const STAGE_STATE_FILE: &str = "crawl-state.json";
const STAGE_LOCK_FILE: &str = "crawl.lock";
const STAGE_FORMAT_VERSION: u32 = 1;
const STAGE_SEGMENT_CLAIM_VERSION: u32 = 1;
const IMMUTABLE_PENDING_SUFFIX: &str = ".pending";
const MAX_STAGED_LIVE_BYTES_AHEAD: u64 = 8 * 1024 * 1024 * 1024;
const MIRROR_STATE_DIR: &str = "nostr-mirror";
const MIRROR_UPLOADED_EVENT_ROOT_FILE: &str = "nostr-event-index.uploaded-root";
const TOP_ITEMS_LIMIT: usize = 20;
const NEGENTROPY_NIP: u16 = 77;

#[cfg(test)]
thread_local! {
    static STAGE_SEGMENT_IO_COUNTS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
fn note_stage_segment_directory_scan() {
    STAGE_SEGMENT_IO_COUNTS.with(|counts| {
        let (directory_scans, file_reads) = counts.get();
        counts.set((directory_scans + 1, file_reads));
    });
}

#[cfg(not(test))]
fn note_stage_segment_directory_scan() {}

#[cfg(test)]
fn note_stage_segment_file_read() {
    STAGE_SEGMENT_IO_COUNTS.with(|counts| {
        let (directory_scans, file_reads) = counts.get();
        counts.set((directory_scans, file_reads + 1));
    });
}

#[cfg(not(test))]
fn note_stage_segment_file_read() {}

#[cfg(test)]
fn reset_stage_segment_io_counts() {
    STAGE_SEGMENT_IO_COUNTS.with(|counts| counts.set((0, 0)));
}

#[cfg(test)]
fn stage_segment_io_counts() -> (usize, usize) {
    STAGE_SEGMENT_IO_COUNTS.with(std::cell::Cell::get)
}

#[derive(Debug, Deserialize)]
struct RelayInfoDocument {
    #[serde(default)]
    supported_nips: Vec<u16>,
}

#[derive(Debug, Clone)]
pub(crate) struct SocialGraphIndexOptions {
    pub(crate) warm_graph_for: Duration,
    pub(crate) graph_crawl_depth: u32,
    pub(crate) full_graph_recrawl: bool,
    pub(crate) relays: Option<Vec<String>>,
    pub(crate) author_allowlist_url: Option<String>,
    pub(crate) max_events_seen: Option<usize>,
    pub(crate) max_authors: usize,
    pub(crate) max_authors_per_run: Option<usize>,
    pub(crate) max_follow_distance: Option<u32>,
    pub(crate) max_live_bytes: u64,
    pub(crate) author_batch_size: usize,
    pub(crate) checkpoint_authors: usize,
    pub(crate) index_commit_batch_size: usize,
    pub(crate) stage_only: bool,
    pub(crate) project_staged: bool,
    pub(crate) bulk_project_staged: bool,
    pub(crate) staging_data_dir: Option<PathBuf>,
    pub(crate) projection_authors: usize,
    pub(crate) projection_event_limit: usize,
    pub(crate) projection_follow: bool,
    pub(crate) btree_order: usize,
    pub(crate) btree_update_concurrency: usize,
    pub(crate) concurrent_batches: usize,
    pub(crate) per_author_event_limit: usize,
    pub(crate) per_author_kind_event_limit: Option<usize>,
    pub(crate) per_author_live_bytes: Option<u64>,
    pub(crate) fetch_timeout: Duration,
    pub(crate) relay_event_max_bytes: Option<u32>,
    pub(crate) global_relay_scan: bool,
    pub(crate) full_author_history: bool,
    pub(crate) negentropy_only: bool,
    pub(crate) relay_page_size: usize,
    pub(crate) max_relay_pages: usize,
    pub(crate) kinds: Option<Vec<u16>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct RankedCount {
    pub(crate) key: String,
    pub(crate) count: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct RecentIndexedEvent {
    pub(crate) id: String,
    pub(crate) pubkey: String,
    pub(crate) created_at: u64,
    pub(crate) kind: u32,
    pub(crate) hashtags: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct IndexedNostrReport {
    pub(crate) root: Option<String>,
    pub(crate) profile_search_root: Option<String>,
    pub(crate) authors_considered: usize,
    pub(crate) authors_processed: usize,
    pub(crate) events_seen: usize,
    pub(crate) events_selected: usize,
    pub(crate) live_bytes_selected: u64,
    pub(crate) warm_graph_seconds: u64,
    pub(crate) graph_crawl_depth: u32,
    pub(crate) full_graph_recrawl: bool,
    pub(crate) max_events_seen: Option<usize>,
    pub(crate) max_follow_distance: Option<u32>,
    pub(crate) max_authors: usize,
    pub(crate) max_live_bytes: u64,
    pub(crate) per_author_live_bytes: Option<u64>,
    pub(crate) relay_event_max_bytes: Option<u32>,
    pub(crate) global_relay_scan: bool,
    pub(crate) full_author_history: bool,
    pub(crate) negentropy_only: bool,
    pub(crate) relay_page_size: usize,
    pub(crate) max_relay_pages: usize,
    pub(crate) relays: Vec<String>,
    pub(crate) top_authors: Vec<RankedCount>,
    pub(crate) top_kinds: Vec<RankedCount>,
    pub(crate) top_hashtags: Vec<RankedCount>,
    pub(crate) recent_events: Vec<RecentIndexedEvent>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct IndexedNostrCrawlPolicy {
    base_root: Option<String>,
    author_allowlist_sha256: String,
    author_count: usize,
    relays: Vec<String>,
    require_all_relays: bool,
    max_events_seen: Option<usize>,
    max_authors: usize,
    max_follow_distance: Option<u32>,
    max_live_bytes: u64,
    author_batch_size: usize,
    checkpoint_authors: usize,
    per_author_event_limit: usize,
    #[serde(default)]
    per_author_kind_event_limit: Option<usize>,
    per_author_live_bytes: Option<u64>,
    fetch_timeout_millis: u64,
    relay_event_max_bytes: Option<u32>,
    global_relay_scan: bool,
    full_author_history: bool,
    negentropy_only: bool,
    relay_page_size: usize,
    max_relay_pages: usize,
    kinds: Option<Vec<u16>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct IndexedNostrCrawlState {
    version: u32,
    /// Diagnostic only: loopback URLs may change while the ordered content
    /// digest remains the same crawl identity.
    author_allowlist_source: Option<String>,
    policy: IndexedNostrCrawlPolicy,
    next_author: usize,
    /// Number of event blobs already durably projected from the staged
    /// segment beginning at `next_author`. Older state files predate partial
    /// segment checkpoints and therefore resume at offset zero.
    #[serde(default)]
    staged_segment_event_offset: usize,
    root: Option<String>,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct StagedNostrCrawlState {
    version: u32,
    /// Diagnostic only; the ordered allowlist digest in `policy` is the
    /// durable crawl identity.
    author_allowlist_source: Option<String>,
    policy: IndexedNostrCrawlPolicy,
    next_author: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StagedSegmentClaim {
    version: u32,
    start_author: usize,
    end_author: usize,
    body_sha256: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct StagedAuthorSegment {
    version: u32,
    start_author: usize,
    end_author: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
    event_cids: Vec<String>,
}

struct StagePaths<'a> {
    staging: &'a Path,
    projection: &'a Path,
}

#[derive(Clone, Copy)]
struct ProjectionStores<'a> {
    durable: &'a HashtreeStore,
    staging: &'a HashtreeStore,
    graph: &'a socialgraph::SocialGraphStore,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct IndexedNostrCheckpointReport {
    root: Option<String>,
    authors_considered: usize,
    authors_processed: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
    max_live_bytes: u64,
    negentropy_only: bool,
    relays: Vec<String>,
}

struct CrawlStateLock {
    file: File,
}

impl CrawlStateLock {
    fn acquire(data_dir: &Path) -> Result<Self> {
        Self::acquire_in(data_dir, INDEX_DIR, CRAWL_LOCK_FILE)
    }

    fn acquire_stage(data_dir: &Path) -> Result<Self> {
        Self::acquire_in(data_dir, STAGE_DIR, STAGE_LOCK_FILE)
    }

    fn acquire_shared(data_dir: &Path) -> Result<Self> {
        Self::acquire_shared_in(data_dir, INDEX_DIR, CRAWL_LOCK_FILE)
    }

    fn acquire_stage_shared(data_dir: &Path) -> Result<Self> {
        Self::acquire_shared_in(data_dir, STAGE_DIR, STAGE_LOCK_FILE)
    }

    fn acquire_in(data_dir: &Path, state_dir: &str, lock_file: &str) -> Result<Self> {
        let output_dir = data_dir.join(state_dir);
        std::fs::create_dir_all(&output_dir)
            .with_context(|| format!("create {}", output_dir.display()))?;
        let path = output_dir.join(lock_file);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open crawl lock {}", path.display()))?;

        #[cfg(unix)]
        {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                anyhow::bail!(
                    "another Nostr index crawl owns {}: {}",
                    path.display(),
                    error
                );
            }
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!(
                "resumable Nostr index crawls require an operating-system advisory file lock"
            );
        }

        Ok(Self { file })
    }

    fn acquire_shared_in(data_dir: &Path, state_dir: &str, lock_file: &str) -> Result<Self> {
        let path = data_dir.join(state_dir).join(lock_file);
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .with_context(|| format!("open existing crawl lock {} read-only", path.display()))?;

        #[cfg(unix)]
        {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                anyhow::bail!(
                    "cannot audit while a Nostr index crawl owns {}: {}",
                    path.display(),
                    error
                );
            }
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!(
                "read-only Nostr index audits require an operating-system advisory file lock"
            );
        }

        Ok(Self { file })
    }
}

impl Drop for CrawlStateLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NostrIndexImportOptions {
    pub(crate) root: Option<String>,
    pub(crate) events_file: PathBuf,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct NostrIndexImportOutput {
    pub(crate) root: String,
    pub(crate) imported: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct NostrIndexQueryOptions {
    pub(crate) root: Option<String>,
    pub(crate) filter_json: String,
    pub(crate) limit: usize,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct NostrIndexQueryOutput {
    pub(crate) root: String,
    pub(crate) count: usize,
    pub(crate) events: Vec<StoredNostrEvent>,
}

pub(crate) async fn run_nostr_bulk_projection_audit(
    data_dir: PathBuf,
    options: BulkProjectionAuditOptions,
) -> Result<()> {
    // Shared, non-creating locks make this command fail while either the
    // projection replay or staging crawler is active. The audit then rechecks
    // byte-for-byte state and catalog digests after its exhaustive traversal.
    let _stage_lock = CrawlStateLock::acquire_stage_shared(&options.staging_data_dir)?;
    let _projection_lock = CrawlStateLock::acquire_shared(&data_dir)?;
    bulk_projection::audit_bulk_projection(&data_dir, options).await?;
    Ok(())
}

pub(crate) fn run_nostr_bulk_tranche_prepare(
    data_dir: PathBuf,
    options: BulkTranchePrepareOptions,
) -> Result<BulkTrancheTransitionOutput> {
    let _stage_lock = CrawlStateLock::acquire_stage(&options.staging_data_dir)?;
    let _projection_lock = CrawlStateLock::acquire(&data_dir)?;
    bulk_projection::prepare_bulk_tranche(&data_dir, options)
}

pub(crate) async fn run_nostr_bulk_tranche_append(
    data_dir: PathBuf,
    options: BulkTrancheAppendOptions,
) -> Result<BulkTrancheTransitionOutput> {
    let _projection_lock = CrawlStateLock::acquire(&data_dir)?;
    let config = Config::load()?;
    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let durable_store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);
    let staging_store = if options.staging_data_dir == data_dir {
        Arc::clone(&durable_store)
    } else {
        Arc::new(HashtreeStore::with_options(
            &options.staging_data_dir,
            config.storage.s3.as_ref(),
            max_size_bytes,
        )?)
    };
    let graph = socialgraph::open_social_graph_store_with_storage(
        &data_dir,
        durable_store.store_arc(),
        Some(
            config
                .nostr
                .db_max_size_gb
                .saturating_mul(1024 * 1024 * 1024),
        ),
    )
    .context("initialize social graph store for v3 tranche append")?;
    graph.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);
    let stores = ProjectionStores {
        durable: durable_store.as_ref(),
        staging: staging_store.as_ref(),
        graph: graph.as_ref(),
    };
    bulk_projection::append_bulk_tranche(stores, &data_dir, options).await
}

pub(crate) fn run_nostr_bulk_tranche_freeze(
    data_dir: PathBuf,
    options: BulkTrancheFreezeOptions,
) -> Result<BulkTrancheTransitionOutput> {
    let _stage_lock = CrawlStateLock::acquire_stage(&options.staging_data_dir)?;
    let _projection_lock = CrawlStateLock::acquire(&data_dir)?;
    bulk_projection::freeze_bulk_tranche(&data_dir, options)
}

pub(crate) async fn run_nostr_bulk_tranche_build(
    data_dir: PathBuf,
    options: BulkTrancheBuildOptions,
) -> Result<BulkTrancheTransitionOutput> {
    let _stage_lock = CrawlStateLock::acquire_stage_shared(&options.staging_data_dir)?;
    let _projection_lock = CrawlStateLock::acquire(&data_dir)?;
    let config = Config::load()?;
    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let durable_store =
        HashtreeStore::with_options(&data_dir, config.storage.s3.as_ref(), max_size_bytes)?;
    let graph = socialgraph::open_social_graph_store_with_storage(
        &data_dir,
        durable_store.store_arc(),
        Some(
            config
                .nostr
                .db_max_size_gb
                .saturating_mul(1024 * 1024 * 1024),
        ),
    )
    .context("initialize social graph store for v3 tranche build")?;
    graph.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);
    bulk_projection::build_bulk_tranche(&durable_store, graph.as_ref(), &data_dir, options).await
}

pub(crate) async fn run_nostr_bulk_profile_repair(
    data_dir: PathBuf,
    options: BulkProfileRepairOptions,
) -> Result<()> {
    for variable in ["HTREE_LMDB_NO_SYNC", "HTREE_LMDB_NO_META_SYNC"] {
        require_durable_profile_repair_lmdb_value(variable, std::env::var_os(variable).as_deref())?;
    }
    require_durable_external_blob_sync_value(
        "HTREE_LMDB_EXTERNAL_BLOB_SYNC",
        std::env::var_os("HTREE_LMDB_EXTERNAL_BLOB_SYNC").as_deref(),
    )?;
    require_writable_profile_repair_pool_value(
        hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV,
        std::env::var_os(hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV).as_deref(),
    )?;
    let _stage_lock = CrawlStateLock::acquire_stage_shared(&options.staging_data_dir)?;
    let _projection_lock = CrawlStateLock::acquire(&data_dir)?;
    let mut writer_config = None;
    let open_writer = || {
        if writer_config.is_none() {
            writer_config = Some(Config::load()?);
        }
        let config = writer_config
            .as_ref()
            .context("profile repair writer configuration was not captured")?;
        let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
        let durable_store = HashtreeStore::with_options_and_backend(
            &data_dir,
            None,
            max_size_bytes,
            config.storage.evict_orphans,
            &hashtree_config::StorageBackend::Lmdb,
        )?;
        let local_store = durable_store.router().local_store();
        let hashtree_cli::storage::LocalStore::Pool(pool) = local_store.as_ref() else {
            anyhow::bail!("profile repair requires the exact writable native PoolStore backend");
        };
        pool.stop_temperature_worker()
            .context("stop PoolStore temperature balancing for exact profile repair")?;
        for member in pool
            .members()
            .context("inspect exact profile repair Pool members")?
        {
            if member.external_blob_dir.is_some() && !member.external_blob_sync {
                anyhow::bail!("Pool member {} has external_blob_sync disabled", member.id);
            }
            if !member.available {
                anyhow::bail!(
                    "Pool member {} is unavailable: {}",
                    member.id,
                    member.last_error.as_deref().unwrap_or("unknown error")
                );
            }
        }
        drop(local_store);
        let graph = socialgraph::open_social_graph_store_with_storage(
            &data_dir,
            durable_store.store_arc(),
            Some(
                config
                    .nostr
                    .db_max_size_gb
                    .saturating_mul(1024 * 1024 * 1024),
            ),
        )
        .context("initialize social graph store for v2 profile repair")?;
        graph.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);
        Ok((durable_store, graph))
    };
    bulk_projection::repair_bulk_projection_profiles(&data_dir, options, open_writer).await
}

pub(crate) async fn run_nostr_bulk_event_blob_repair(
    data_dir: PathBuf,
    options: BulkEventBlobRepairOptions,
) -> Result<()> {
    for variable in ["HTREE_LMDB_NO_SYNC", "HTREE_LMDB_NO_META_SYNC"] {
        require_durable_profile_repair_lmdb_value(variable, std::env::var_os(variable).as_deref())?;
    }
    require_durable_external_blob_sync_value(
        "HTREE_LMDB_EXTERNAL_BLOB_SYNC",
        std::env::var_os("HTREE_LMDB_EXTERNAL_BLOB_SYNC").as_deref(),
    )?;
    require_writable_profile_repair_pool_value(
        hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV,
        std::env::var_os(hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV).as_deref(),
    )?;
    let _stage_lock = CrawlStateLock::acquire_stage_shared(&options.staging_data_dir)?;
    let _projection_lock = CrawlStateLock::acquire(&data_dir)?;
    socialgraph::bootstrap_profile_root_pair_transaction_lock(&data_dir)
        .context("bootstrap event/profile root-pair transaction lock")?;
    bulk_projection::repair_bulk_projection_event_blobs(&data_dir, options).await
}

fn require_durable_profile_repair_lmdb_value(
    variable: &str,
    value: Option<&std::ffi::OsStr>,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value
        .to_str()
        .with_context(|| format!("{variable} is not valid UTF-8"))?;
    if !matches!(value, "0" | "false" | "FALSE") {
        anyhow::bail!("{variable} must be unset or explicitly false for durable profile repair");
    }
    Ok(())
}

fn require_durable_external_blob_sync_value(
    variable: &str,
    value: Option<&std::ffi::OsStr>,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value
        .to_str()
        .with_context(|| format!("{variable} is not valid UTF-8"))?
        .trim();
    if value != "1" && !value.eq_ignore_ascii_case("true") && !value.eq_ignore_ascii_case("yes") {
        anyhow::bail!("{variable} must be unset or explicitly true for durable profile repair");
    }
    Ok(())
}

fn require_writable_profile_repair_pool_value(
    variable: &str,
    value: Option<&std::ffi::OsStr>,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value == std::ffi::OsStr::new("0") {
        return Ok(());
    }
    anyhow::bail!("{variable} must be unset or exactly 0 for writable profile repair");
}

#[derive(Debug, Clone)]
pub(crate) struct NostrReplaceableRepairOptions {
    pub(crate) state_file: PathBuf,
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) eligible_authors: PathBuf,
    pub(crate) page_size: usize,
    pub(crate) btree_order: usize,
    pub(crate) apply: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct NostrTimeRepairPreparationOptions {
    pub(crate) state_file: PathBuf,
    pub(crate) apply: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct NostrReplaceableRepairOutput {
    pub(crate) previous_root: String,
    pub(crate) root: String,
    pub(crate) entries_scanned: u64,
    pub(crate) replaceable_entries: usize,
    pub(crate) parameterized_replaceable_entries: usize,
    pub(crate) recovered_event_blobs: usize,
    pub(crate) applied: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct NostrTimeRepairPreparationOutput {
    pub(crate) previous_root: String,
    pub(crate) root: String,
    pub(crate) applied: bool,
}

async fn fetch_missing_events_from_relays(
    event_ids: &[String],
    relays: &[String],
    timeout: Duration,
    relay_event_max_bytes: Option<u32>,
) -> Result<Vec<StoredNostrEvent>> {
    if event_ids.is_empty() {
        return Ok(Vec::new());
    }
    if relays.is_empty() {
        anyhow::bail!("durable crawl policy has no relays for missing-event recovery");
    }

    let client = if let Some(max_size) = relay_event_max_bytes {
        let mut limits = RelayLimits::default();
        limits.events.max_size = Some(max_size);
        NostrClient::builder()
            .signer(Keys::generate())
            .opts(ClientOptions::new().relay_limits(limits))
            .build()
    } else {
        NostrClient::new(Keys::generate())
    };
    for relay in relays {
        if let Err(error) = client.add_relay(relay).await {
            eprintln!("Skipping repair relay {relay}: {error}");
        }
    }
    client.connect().await;
    client
        .wait_for_connection(timeout.min(Duration::from_secs(2)))
        .await;

    let parsed_ids = event_ids
        .iter()
        .map(|event_id| {
            EventId::from_hex(event_id)
                .with_context(|| format!("parse missing Nostr event id {event_id}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let requests = parsed_ids
        .chunks(64)
        .flat_map(|ids| {
            relays
                .iter()
                .map(move |relay| (relay.clone(), ids.to_vec()))
        })
        .collect::<Vec<_>>();
    let concurrency = relays.len().saturating_mul(2).max(1);
    let batches = stream::iter(requests)
        .map(|(relay, ids)| {
            let client = client.clone();
            async move {
                let filter = NostrFilter::new().ids(ids);
                match client
                    .fetch_events_from([relay.as_str()], filter, timeout)
                    .await
                {
                    Ok(events) => events.to_vec(),
                    Err(error) => {
                        eprintln!("Skipping failed repair request to {relay}: {error}");
                        Vec::new()
                    }
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    client.disconnect().await;

    let wanted = event_ids.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut recovered = BTreeMap::<String, StoredNostrEvent>::new();
    for event in batches.into_iter().flatten() {
        let id = event.id.to_hex();
        if wanted.contains(id.as_str()) {
            recovered
                .entry(id)
                .or_insert_with(|| stored_event_from_nostr_sdk_event(&event));
        }
    }
    Ok(recovered.into_values().collect())
}

pub(crate) async fn run_nostr_index_import(
    data_dir: PathBuf,
    options: NostrIndexImportOptions,
) -> Result<NostrIndexImportOutput> {
    let root = if let Some(root) = options.root.as_deref() {
        Some(parse_root_text(root).context("parse --root")?)
    } else {
        load_existing_root(&data_dir)?
    };
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&options.events_file)
            .with_context(|| format!("read {}", options.events_file.display()))?,
    )
    .with_context(|| {
        format!(
            "parse Nostr events JSON from {}",
            options.events_file.display()
        )
    })?;
    let events = parse_nostr_events_json(&value)?;
    if events.is_empty() && root.is_none() {
        anyhow::bail!("Nostr event import did not contain any events");
    }
    let stored = events
        .into_iter()
        .map(|event| {
            VerifiedEvent::try_from(event)
                .map(|verified| verified.to_stored_event().into_stored())
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let imported = stored.len();

    let config = Config::load()?;
    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);
    let event_store = NostrEventStore::new(store.store_arc());
    let next_root = event_store
        .build(root.as_ref(), stored)
        .await
        .context("import signed Nostr events into local index")?
        .or(root)
        .context("Nostr event import did not produce an index root")?;
    persist_latest_root(&data_dir, &next_root)?;

    let output = NostrIndexImportOutput {
        root: cid_to_nhash(&next_root)?,
        imported,
    };
    write_nostr_index_import_output(&output, options.out.as_deref())?;
    Ok(output)
}

pub(crate) async fn run_nostr_index_query(
    data_dir: PathBuf,
    options: NostrIndexQueryOptions,
) -> Result<NostrIndexQueryOutput> {
    let root = if let Some(root) = options.root.as_deref() {
        parse_root_text(root).context("parse --root")?
    } else {
        load_existing_root(&data_dir)?
            .context("missing Nostr index root; pass --root or run socialgraph index first")?
    };
    let filters = parse_nostr_filters_json(&options.filter_json)?;

    let config = Config::load()?;
    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);
    let event_store = NostrEventStore::new(store.store_arc());

    let mut seen = HashSet::new();
    let mut events = Vec::new();
    for filter in &filters {
        for event in event_store
            .query_events(Some(&root), filter, options.limit)
            .await
            .context("query stored Nostr events")?
        {
            if seen.insert(event.id.clone()) {
                events.push(event);
            }
        }
    }
    events.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    events.truncate(options.limit);

    let output = NostrIndexQueryOutput {
        root: cid_to_nhash(&root)?,
        count: events.len(),
        events,
    };
    write_nostr_index_query_output(&output, options.out.as_deref())?;
    Ok(output)
}

pub(crate) async fn run_nostr_time_repair_preparation(
    data_dir: PathBuf,
    options: NostrTimeRepairPreparationOptions,
) -> Result<NostrTimeRepairPreparationOutput> {
    let expected_state_file = data_dir.join(INDEX_DIR).join(CRAWL_STATE_FILE);
    if options.state_file != expected_state_file {
        anyhow::bail!(
            "repair state file {} does not match data directory state {}",
            options.state_file.display(),
            expected_state_file.display()
        );
    }
    let state_bytes = std::fs::read(&options.state_file)
        .with_context(|| format!("read durable crawl state {}", options.state_file.display()))?;
    let mut state: IndexedNostrCrawlState = serde_json::from_slice(&state_bytes)
        .with_context(|| format!("parse durable crawl state {}", options.state_file.display()))?;
    let previous_root = state
        .root
        .clone()
        .context("durable crawl state has no root")?;
    let root = parse_root_text(&previous_root).context("parse durable crawl root")?;

    if !options.apply {
        return Ok(NostrTimeRepairPreparationOutput {
            previous_root: previous_root.clone(),
            root: previous_root,
            applied: false,
        });
    }

    let _lock = CrawlStateLock::acquire(&data_dir)?;
    let config = Config::load()?;
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        0,
    )?);
    let event_store = NostrEventStore::new(store.store_arc());
    let prepared_root = event_store
        .clear_derived_indexes(&root)
        .await
        .context("write temporary derived-index repair manifest")?;
    store
        .force_sync()
        .context("force-sync derived-index repair manifest")?;
    let prepared_root = cid_to_nhash(&prepared_root)?;
    state.root = Some(prepared_root.clone());
    persist_crawl_state(&data_dir, &state)?;

    Ok(NostrTimeRepairPreparationOutput {
        previous_root,
        root: prepared_root,
        applied: true,
    })
}

pub(crate) async fn run_nostr_replaceable_repair(
    data_dir: PathBuf,
    options: NostrReplaceableRepairOptions,
) -> Result<NostrReplaceableRepairOutput> {
    let expected_state_file = data_dir.join(INDEX_DIR).join(CRAWL_STATE_FILE);
    if options.state_file != expected_state_file {
        anyhow::bail!(
            "repair state file {} does not match data directory state {}",
            options.state_file.display(),
            expected_state_file.display()
        );
    }
    if options.page_size == 0 || options.btree_order < 2 {
        anyhow::bail!("repair page size must be non-zero and B-tree order must be at least 2");
    }
    let state_bytes = std::fs::read(&options.state_file)
        .with_context(|| format!("read durable crawl state {}", options.state_file.display()))?;
    let mut state: IndexedNostrCrawlState = serde_json::from_slice(&state_bytes)
        .with_context(|| format!("parse durable crawl state {}", options.state_file.display()))?;
    let previous_root = state
        .root
        .clone()
        .context("durable crawl state has no root")?;
    let root = parse_root_text(&previous_root).context("parse durable crawl root")?;

    if !options.apply {
        return Ok(NostrReplaceableRepairOutput {
            previous_root: previous_root.clone(),
            root: previous_root,
            entries_scanned: 0,
            replaceable_entries: 0,
            parameterized_replaceable_entries: 0,
            recovered_event_blobs: 0,
            applied: false,
        });
    }

    let _lock = CrawlStateLock::acquire(&data_dir)?;
    let _stage_lock = CrawlStateLock::acquire_stage(&options.staging_data_dir)?;
    let config = Config::load()?;
    // Closed-writer repair must never invoke quota eviction while opening an
    // intentionally large dedicated index.
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        0,
    )?);
    let event_store = NostrEventStore::with_options(
        store.store_arc(),
        NostrEventStoreOptions {
            btree_order: Some(options.btree_order),
            btree_update_concurrency: None,
            index_commit_batch_size: Some(2048),
        },
    );
    let missing = event_store
        .missing_parameterized_event_links(&root, options.page_size)
        .await
        .context("scan missing parameterized event blobs")?;
    let mut working_root = root;
    let mut superseded_nodes = Vec::new();
    let recovered_event_blobs = if missing.is_empty() {
        0
    } else {
        let allowlist_text =
            std::fs::read_to_string(&options.eligible_authors).with_context(|| {
                format!(
                    "read ordered author allowlist {}",
                    options.eligible_authors.display()
                )
            })?;
        let authors = parse_author_allowlist(&allowlist_text, usize::MAX);
        let mut allowlist_hash = Sha256::new();
        for author in &authors {
            allowlist_hash.update(author.as_bytes());
            allowlist_hash.update(b"\n");
        }
        let allowlist_hash = hex::encode(allowlist_hash.finalize());
        if authors.len() != state.policy.author_count
            || allowlist_hash != state.policy.author_allowlist_sha256
        {
            anyhow::bail!("repair author allowlist does not match the durable crawl identity");
        }
        let author_positions = authors
            .iter()
            .enumerate()
            .map(|(index, author)| (author.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut targets_by_author = BTreeMap::<usize, HashSet<String>>::new();
        for missing_link in &missing {
            let parts = missing_link.key.split(':').collect::<Vec<_>>();
            if parts.len() != 4 || parts[0].len() != 64 || parts[3].len() != 64 {
                anyhow::bail!("invalid missing author-kind-time key {}", missing_link.key);
            }
            let author_index = author_positions.get(parts[0]).copied().with_context(|| {
                format!(
                    "missing event author {} is absent from the crawl allowlist",
                    parts[0]
                )
            })?;
            if author_index > state.next_author
                || (author_index == state.next_author && state.staged_segment_event_offset == 0)
            {
                anyhow::bail!(
                    "missing event belongs to unprojected author {author_index}; refusing repair"
                );
            }
            targets_by_author
                .entry(author_index)
                .or_default()
                .insert(parts[3].to_string());
        }

        let staging_store = Arc::new(HashtreeStore::with_options(
            &options.staging_data_dir,
            config.storage.s3.as_ref(),
            0,
        )?);
        let staging_event_store = NostrEventStore::new(staging_store.store_arc());
        let mut recovered = BTreeMap::<String, StoredNostrEvent>::new();
        for (author_index, mut target_ids) in targets_by_author {
            let path =
                stage_segment_path(&options.staging_data_dir, author_index, author_index + 1);
            let segment_bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("read staged repair segment {}", path.display()));
                }
            };
            let segment: StagedAuthorSegment = serde_json::from_slice(&segment_bytes)
                .with_context(|| format!("parse staged repair segment {}", path.display()))?;
            if segment.start_author != author_index || segment.end_author != author_index + 1 {
                anyhow::bail!(
                    "staged repair segment {} has the wrong author range",
                    path.display()
                );
            }
            let projected_len = if author_index < state.next_author {
                segment.event_cids.len()
            } else {
                state
                    .staged_segment_event_offset
                    .min(segment.event_cids.len())
            };
            for cid_texts in segment.event_cids[..projected_len].chunks(2048) {
                let cids = cid_texts
                    .iter()
                    .map(|cid| parse_root_text(cid))
                    .collect::<Result<Vec<_>>>()?;
                for event in staging_event_store
                    .load_event_blobs(cids)
                    .await
                    .with_context(|| {
                        format!("load staged repair events for author {author_index}")
                    })?
                {
                    if target_ids.remove(&event.id) {
                        recovered.insert(event.id.clone(), event);
                    }
                }
                if target_ids.is_empty() {
                    break;
                }
            }
        }

        let missing_event_ids = missing
            .iter()
            .map(|missing_link| {
                missing_link
                    .key
                    .rsplit_once(':')
                    .map(|(_, event_id)| event_id.to_string())
                    .context("missing author-kind-time key has no event id")
            })
            .collect::<Result<Vec<_>>>()?;
        let unresolved = missing_event_ids
            .iter()
            .filter(|event_id| !recovered.contains_key(event_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for event in fetch_missing_events_from_relays(
            &unresolved,
            &state.policy.relays,
            Duration::from_millis(state.policy.fetch_timeout_millis),
            state.policy.relay_event_max_bytes,
        )
        .await
        .context("fetch missing indexed events from crawl relays")?
        {
            recovered.insert(event.id.clone(), event);
        }
        if recovered.len() != missing_event_ids.len() {
            let first_unresolved = missing_event_ids
                .iter()
                .find(|event_id| !recovered.contains_key(event_id.as_str()))
                .map(String::as_str)
                .unwrap_or("unknown");
            anyhow::bail!(
                "recovered {} of {} missing indexed events; first unresolved id={first_unresolved}",
                recovered.len(),
                missing_event_ids.len()
            );
        }

        let repair_base = event_store
            .clear_replaceable_indexes(&working_root)
            .await
            .context("write temporary repair manifest")?;
        let event_repair = event_store
            .repair_missing_event_blobs_with_superseded_nodes(&repair_base, recovered.into_values())
            .await
            .context("reindex recovered missing events")?;
        working_root = event_repair
            .root
            .context("recovered events did not produce an index root")?;
        superseded_nodes.extend(event_repair.superseded_nodes);
        missing.len()
    };
    let rebuild_base = event_store
        .clear_derived_indexes(&working_root)
        .await
        .context("write derived-index rebuild manifest")?;
    let report = event_store
        .rebuild_derived_indexes(&rebuild_base, options.page_size)
        .await
        .context("rebuild derived event indexes")?;
    store
        .force_sync()
        .context("force-sync repaired Nostr index")?;
    let repaired_root = cid_to_nhash(&report.root)?;
    state.root = Some(repaired_root.clone());
    persist_crawl_state(&data_dir, &state)?;
    event_store
        .delete_superseded_nodes(&superseded_nodes)
        .await
        .context("delete superseded nodes after publishing repaired root")?;

    Ok(NostrReplaceableRepairOutput {
        previous_root,
        root: repaired_root,
        entries_scanned: report.entries_scanned,
        replaceable_entries: report.replaceable_entries,
        parameterized_replaceable_entries: report.parameterized_replaceable_entries,
        recovered_event_blobs,
        applied: true,
    })
}

pub(crate) async fn run_socialgraph_index_from_cli(
    data_dir: PathBuf,
    options: SocialGraphIndexOptions,
) -> Result<IndexedNostrReport> {
    let config = Config::load()?;
    let (keys, _) = ensure_keys()?;
    run_socialgraph_index(data_dir, &config, keys, options).await
}

fn uses_durable_author_checkpoints(options: &SocialGraphIndexOptions) -> bool {
    options.author_allowlist_url.is_some()
        && (options.full_author_history || options.negentropy_only)
}

pub(crate) async fn run_socialgraph_index(
    data_dir: PathBuf,
    config: &Config,
    keys: Keys,
    options: SocialGraphIndexOptions,
) -> Result<IndexedNostrReport> {
    let checkpointed_allowlist = uses_durable_author_checkpoints(&options);
    let staging_data_dir = options
        .staging_data_dir
        .clone()
        .unwrap_or_else(|| data_dir.clone());
    if options.stage_only && options.project_staged {
        anyhow::bail!("--stage-only and --project-staged are mutually exclusive");
    }
    if options.bulk_project_staged && !options.project_staged {
        anyhow::bail!("--bulk-project-staged requires --project-staged");
    }
    if (options.stage_only || options.project_staged) && !checkpointed_allowlist {
        anyhow::bail!(
            "two-phase Nostr indexing requires --author-allowlist-url and either --full-author-history or --negentropy-only"
        );
    }
    if options.project_staged
        && (options.projection_authors == 0 || options.projection_event_limit == 0)
    {
        anyhow::bail!(
            "--projection-authors and --projection-event-limit must be greater than zero"
        );
    }
    if checkpointed_allowlist && options.checkpoint_authors == 0 {
        anyhow::bail!("--checkpoint-authors must be greater than zero");
    }
    if checkpointed_allowlist && options.author_batch_size == 0 {
        anyhow::bail!("--author-batch-size must be greater than zero");
    }
    if options.max_authors_per_run == Some(0) {
        anyhow::bail!("--max-authors-per-run must be greater than zero");
    }
    if options.max_authors_per_run.is_some()
        && (!checkpointed_allowlist || options.stage_only || options.project_staged)
    {
        anyhow::bail!("--max-authors-per-run requires one-phase durable allowlist indexing");
    }
    if options.index_commit_batch_size == 0 {
        anyhow::bail!("--index-commit-batch-size must be greater than zero");
    }
    if options.btree_order < 2 {
        anyhow::bail!("--btree-order must be at least 2");
    }
    if checkpointed_allowlist && options.max_events_seen == Some(0) {
        anyhow::bail!("--max-events-seen must be greater than zero");
    }
    if options.per_author_kind_event_limit == Some(0) {
        anyhow::bail!("--per-author-kind-event-limit must be greater than zero");
    }
    let _crawl_lock = if options.stage_only {
        Some(CrawlStateLock::acquire_stage(&staging_data_dir)?)
    } else if checkpointed_allowlist {
        Some(CrawlStateLock::acquire(&data_dir)?)
    } else {
        None
    };
    if options.project_staged && bulk_projection::load_bulk_tranche_progress(&data_dir)?.is_some() {
        anyhow::bail!(
            "legacy staged projection is disabled after v3 tranche state exists; use `htree nostr-index append-bulk-tranche`"
        );
    }

    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);

    let graph_store = socialgraph::open_social_graph_store_with_storage(
        &data_dir,
        store.store_arc(),
        Some(
            config
                .nostr
                .db_max_size_gb
                .saturating_mul(1024 * 1024 * 1024),
        ),
    )
    .context("Failed to initialize social graph store")?;
    graph_store.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);

    let root_pk = if let Some(root_npub) = config.nostr.socialgraph_root.as_deref() {
        parse_npub(root_npub).unwrap_or_else(|_| keys.public_key().to_bytes())
    } else {
        keys.public_key().to_bytes()
    };
    socialgraph::set_social_graph_root(&graph_store, &root_pk);
    let relays = options
        .relays
        .clone()
        .filter(|relays| !relays.is_empty())
        .unwrap_or_else(|| config.nostr.relays.clone());
    let relays = resolve_index_relays(relays, options.negentropy_only)
        .await
        .context("resolve index relay set")?;

    let saved_crawl_state = if checkpointed_allowlist {
        load_crawl_state(&data_dir)?
    } else {
        None
    };

    if !options.warm_graph_for.is_zero() && saved_crawl_state.is_none() {
        warm_social_graph(
            graph_store.clone(),
            keys.clone(),
            relays.clone(),
            options.graph_crawl_depth,
            options.full_graph_recrawl,
            options.concurrent_batches,
            options.warm_graph_for,
        )
        .await?;
    }

    let author_allowlist =
        load_author_allowlist(options.author_allowlist_url.as_deref(), options.max_authors).await?;

    let existing_root = match saved_crawl_state.as_ref() {
        Some(state) => state
            .policy
            .base_root
            .as_deref()
            .map(parse_root_text)
            .transpose()
            .context("parse resumable crawl base root")?,
        None => load_existing_root(&data_dir)?,
    };
    let event_store_options = nostr_event_store_options(&options);
    let event_store = NostrEventStore::with_options(store.store_arc(), event_store_options.clone());

    if checkpointed_allowlist {
        let authors = author_allowlist
            .as_ref()
            .context("checkpointed crawl requires an explicit author allowlist")?;
        validate_reachable_root(&event_store, existing_root.as_ref(), "crawl base root").await?;
        let policy = build_crawl_policy(&options, &relays, authors, existing_root.as_ref())?;
        if options.stage_only {
            if let Some(state) = saved_crawl_state.as_ref() {
                validate_crawl_state(state, &policy, authors.len())?;
            }
            let staging_store = if staging_data_dir == data_dir {
                Arc::clone(&store)
            } else {
                Arc::new(HashtreeStore::with_options(
                    &staging_data_dir,
                    config.storage.s3.as_ref(),
                    max_size_bytes,
                )?)
            };
            let report = stage_allowlist_in_checkpoints(
                staging_store.as_ref(),
                StagePaths {
                    staging: &staging_data_dir,
                    projection: &data_dir,
                },
                &options,
                &relays,
                authors,
                policy,
                saved_crawl_state.as_ref(),
            )
            .await?;
            let index_report = build_bounded_report(
                &relays,
                &options,
                report,
                graph_store
                    .profile_search_root()?
                    .as_ref()
                    .map(cid_to_nhash)
                    .transpose()?,
            )?;
            print_report(&index_report, &data_dir);
            return Ok(index_report);
        }
        let mut state = match saved_crawl_state {
            Some(state) => {
                validate_crawl_state(&state, &policy, authors.len())?;
                let root = state
                    .root
                    .as_deref()
                    .map(parse_root_text)
                    .transpose()
                    .context("parse resumable crawl root")?;
                validate_reachable_root(&event_store, root.as_ref(), "resumable crawl root")
                    .await?;
                state
            }
            None => IndexedNostrCrawlState {
                version: CRAWL_STATE_VERSION,
                author_allowlist_source: options.author_allowlist_url.clone(),
                policy: policy.clone(),
                next_author: 0,
                staged_segment_event_offset: 0,
                root: existing_root.as_ref().map(cid_to_nhash).transpose()?,
                events_seen: 0,
                events_selected: 0,
                live_bytes_selected: 0,
            },
        };
        state.author_allowlist_source = options.author_allowlist_url.clone();
        state.policy = policy;
        if !options.project_staged && state.staged_segment_event_offset != 0 {
            anyhow::bail!(
                "Nostr crawl has a partial staged projection at author {} offset {}; resume with --project-staged",
                state.next_author,
                state.staged_segment_event_offset
            );
        }
        persist_crawl_state(&data_dir, &state)?;
        let report = if options.project_staged {
            let projection_policy = state.policy.clone();
            let staging_store = if staging_data_dir == data_dir {
                Arc::clone(&store)
            } else {
                Arc::new(HashtreeStore::with_options(
                    &staging_data_dir,
                    config.storage.s3.as_ref(),
                    max_size_bytes,
                )?)
            };
            let stores = ProjectionStores {
                durable: store.as_ref(),
                staging: staging_store.as_ref(),
                graph: graph_store.as_ref(),
            };
            if options.bulk_project_staged {
                bulk_projection::project_staged_allowlist_bulk(
                    stores,
                    &data_dir,
                    &staging_data_dir,
                    &options,
                    authors,
                    &projection_policy,
                    &mut state,
                )
                .await?
            } else {
                project_staged_allowlist(
                    stores,
                    &data_dir,
                    &staging_data_dir,
                    &options,
                    authors,
                    &projection_policy,
                    &mut state,
                )
                .await?
            }
        } else {
            crawl_allowlist_in_checkpoints(
                store.as_ref(),
                graph_store.as_ref(),
                &data_dir,
                &options,
                &relays,
                authors,
                &mut state,
            )
            .await?
        };
        let completed_allowlist = report.authors_processed >= authors.len();
        let profile_search_root = graph_store
            .profile_search_root()?
            .as_ref()
            .map(cid_to_nhash)
            .transpose()?;
        let index_report = build_bounded_report(&relays, &options, report, profile_search_root)?;
        if !completed_allowlist {
            eprintln!(
                "Nostr index process tranche complete: authors={}/{}; restart to resume from the durable checkpoint",
                index_report.authors_processed,
                index_report.authors_considered
            );
            return Ok(index_report);
        }
        persist_report(&data_dir, &index_report)?;
        clear_checkpoint(&data_dir)?;
        print_report(&index_report, &data_dir);
        return Ok(index_report);
    }

    let bridge = NostrBridge::with_event_store_options(
        store.store_arc(),
        CrawlConfig {
            relays: relays.clone(),
            author_allowlist,
            max_live_bytes: Some(options.max_live_bytes),
            max_events_seen: options.max_events_seen,
            max_authors: Some(options.max_authors),
            max_follow_distance: options.max_follow_distance,
            author_batch_size: options.author_batch_size,
            per_author_event_limit: options.per_author_event_limit,
            per_author_kind_event_limit: options.per_author_kind_event_limit,
            per_author_live_bytes: options.per_author_live_bytes,
            fetch_timeout: options.fetch_timeout,
            relay_event_max_size: options.relay_event_max_bytes,
            relay_fetch_mode: if options.full_author_history || options.negentropy_only {
                RelayFetchMode::AuthorBatches
            } else if options.global_relay_scan {
                RelayFetchMode::GlobalRecent
            } else {
                RelayFetchMode::AuthorBatches
            },
            require_negentropy: options.negentropy_only,
            relay_page_size: options.relay_page_size,
            max_relay_pages: options.max_relay_pages,
            full_author_history: options.full_author_history,
            kinds: options.kinds.clone(),
        },
        event_store_options,
    );

    let report = bridge
        .crawl_with_progress(graph_store.as_ref(), existing_root.as_ref(), |progress| {
            if let Err(err) = persist_checkpoint(&data_dir, progress, &options, &relays) {
                eprintln!("Failed to persist nostr index checkpoint: {err}");
            }
        })
        .await?;
    sync_socialgraph_profile_index_from_root(
        graph_store.as_ref(),
        &event_store,
        report.root.as_ref(),
    )
    .await?;
    let mut index_report = build_report(&event_store, &relays, &options, report).await?;
    index_report.profile_search_root = graph_store
        .profile_search_root()?
        .as_ref()
        .map(cid_to_nhash)
        .transpose()?;
    persist_report(&data_dir, &index_report)?;
    clear_checkpoint(&data_dir)?;
    print_report(&index_report, &data_dir);
    Ok(index_report)
}

fn build_crawl_config(
    options: &SocialGraphIndexOptions,
    relays: &[String],
    author_allowlist: Option<Vec<String>>,
    max_live_bytes: u64,
    max_events_seen: Option<usize>,
    author_batch_size: usize,
) -> CrawlConfig {
    CrawlConfig {
        relays: relays.to_vec(),
        author_allowlist,
        max_live_bytes: Some(max_live_bytes),
        max_events_seen,
        max_authors: Some(author_batch_size),
        max_follow_distance: options.max_follow_distance,
        author_batch_size,
        per_author_event_limit: options.per_author_event_limit,
        per_author_kind_event_limit: options.per_author_kind_event_limit,
        per_author_live_bytes: options.per_author_live_bytes,
        fetch_timeout: options.fetch_timeout,
        relay_event_max_size: options.relay_event_max_bytes,
        relay_fetch_mode: RelayFetchMode::AuthorBatches,
        require_negentropy: options.negentropy_only,
        relay_page_size: options.relay_page_size,
        max_relay_pages: options.max_relay_pages,
        full_author_history: options.full_author_history,
        kinds: options.kinds.clone(),
    }
}

fn nostr_event_store_options(options: &SocialGraphIndexOptions) -> NostrEventStoreOptions {
    NostrEventStoreOptions {
        btree_order: Some(options.btree_order),
        btree_update_concurrency: Some(options.btree_update_concurrency),
        index_commit_batch_size: Some(options.index_commit_batch_size),
    }
}

async fn crawl_allowlist_in_checkpoints(
    durable_store: &HashtreeStore,
    graph_store: &socialgraph::SocialGraphStore,
    data_dir: &Path,
    options: &SocialGraphIndexOptions,
    relays: &[String],
    authors: &[String],
    state: &mut IndexedNostrCrawlState,
) -> Result<CrawlReport> {
    let store = durable_store.store_arc();
    let checkpoint_authors = options
        .checkpoint_authors
        .min(options.author_batch_size)
        .max(1);
    let event_store_options = nostr_event_store_options(options);
    let event_store =
        NostrEventStore::with_options(Arc::clone(&store), event_store_options.clone());
    let process_author_limit = options
        .max_authors_per_run
        .map(|limit| state.next_author.saturating_add(limit).min(authors.len()))
        .unwrap_or(authors.len());

    while state.next_author < process_author_limit {
        if state.live_bytes_selected >= options.max_live_bytes {
            anyhow::bail!(
                "Nostr index live-byte budget exhausted after {}/{} authors; the durable checkpoint was preserved",
                state.next_author,
                authors.len()
            );
        }
        if options
            .max_events_seen
            .is_some_and(|limit| state.events_seen >= limit)
        {
            anyhow::bail!(
                "Nostr index relay-event budget exhausted after {}/{} authors; the durable checkpoint was preserved",
                state.next_author,
                authors.len()
            );
        }

        let end = state
            .next_author
            .saturating_add(checkpoint_authors)
            .min(process_author_limit)
            .min(authors.len());
        let author_batch = authors[state.next_author..end].to_vec();
        let remaining_live_bytes = options
            .max_live_bytes
            .saturating_sub(state.live_bytes_selected);
        let remaining_events_seen = options
            .max_events_seen
            .map(|limit| limit.saturating_sub(state.events_seen));
        let current_root = state
            .root
            .as_deref()
            .map(parse_root_text)
            .transpose()
            .context("parse checkpoint root before author batch")?;
        validate_reachable_root(&event_store, current_root.as_ref(), "checkpoint root").await?;

        let started = Instant::now();
        let bridge = NostrBridge::with_event_store_options(
            Arc::clone(&store),
            build_crawl_config(
                options,
                relays,
                Some(author_batch),
                remaining_live_bytes,
                remaining_events_seen,
                end - state.next_author,
            ),
            event_store_options.clone(),
        )
        // The social-graph projection consumes metadata only. Avoid retaining
        // a duplicate copy of every content event while Hashtree indexes are
        // being committed.
        .retaining_applied_event_kinds([0]);
        let report = bridge
            .crawl(graph_store, current_root.as_ref())
            .await
            .with_context(|| format!("crawl allowlisted authors {}..{}", state.next_author, end))?;
        if report.authors_processed != end - state.next_author {
            anyhow::bail!(
                "author batch stopped after {} of {} authors; checkpoint cursor was not advanced",
                report.authors_processed,
                end - state.next_author
            );
        }
        validate_reachable_root(&event_store, report.root.as_ref(), "new checkpoint root").await?;

        if !report.applied_events.is_empty() {
            let parsed = report
                .applied_events
                .iter()
                .map(|event| event.to_nostr_sdk_event().map_err(anyhow::Error::from))
                .collect::<Result<Vec<_>>>()?;
            graph_store
                .sync_profile_index_for_events(&parsed)
                .context("sync checkpointed profile search batch")?;
        }

        state.next_author = end;
        state.root = report.root.as_ref().map(cid_to_nhash).transpose()?;
        state.events_seen = state.events_seen.saturating_add(report.events_seen);
        state.events_selected = state.events_selected.saturating_add(report.events_selected);
        state.live_bytes_selected = state
            .live_bytes_selected
            .saturating_add(report.live_bytes_selected);
        let checkpoint_sync_started = Instant::now();
        graph_store
            .force_sync()
            .context("force-sync social graph checkpoint storage")?;
        durable_store
            .force_sync()
            .context("force-sync Nostr index checkpoint storage")?;
        let checkpoint_sync_ms = checkpoint_sync_started.elapsed().as_millis();
        persist_crawl_state(data_dir, state)?;
        let gc_started = Instant::now();
        let superseded_nodes_deleted = event_store
            .delete_superseded_nodes(&report.superseded_nodes)
            .await
            .context("delete superseded Nostr checkpoint nodes")?;
        let gc_ms = gc_started.elapsed().as_millis();
        // Large B-tree change maps are intentionally short-lived, but glibc
        // can retain their freed arenas across durable author checkpoints.
        // Return those pages before starting the next checkpoint so a bounded
        // crawler does not accumulate heap until its cgroup limit is reached.
        hashtree_cli::diagnostics::trim_process_allocations();
        eprintln!(
            "Nostr index checkpoint: authors={}/{} events_seen={} events_selected={} live_bytes={} relay_fetch_select_ms={} index_build_ms={} checkpoint_sync_ms={} superseded_nodes_deleted={} gc_ms={} batch_elapsed_ms={}",
            state.next_author,
            authors.len(),
            state.events_seen,
            state.events_selected,
            state.live_bytes_selected,
            report.relay_fetch_select_ms,
            report.index_build_ms,
            checkpoint_sync_ms,
            superseded_nodes_deleted,
            gc_ms,
            started.elapsed().as_millis()
        );
    }

    Ok(CrawlReport {
        root: state.root.as_deref().map(parse_root_text).transpose()?,
        authors_considered: authors.len(),
        authors_processed: state.next_author,
        events_seen: state.events_seen,
        events_selected: state.events_selected,
        live_bytes_selected: state.live_bytes_selected,
        applied_events: Vec::new(),
        relay_fetch_select_ms: 0,
        index_build_ms: 0,
        superseded_nodes: Vec::new(),
    })
}

async fn stage_allowlist_in_checkpoints(
    durable_store: &HashtreeStore,
    paths: StagePaths<'_>,
    options: &SocialGraphIndexOptions,
    relays: &[String],
    authors: &[String],
    policy: IndexedNostrCrawlPolicy,
    indexed_state: Option<&IndexedNostrCrawlState>,
) -> Result<CrawlReport> {
    let store = durable_store.store_arc();
    let event_store =
        NostrEventStore::with_options(Arc::clone(&store), nostr_event_store_options(options));
    let mut state = match load_stage_state(paths.staging)? {
        Some(state) => {
            validate_stage_state(&state, &policy, authors.len())?;
            state
        }
        None => StagedNostrCrawlState {
            version: STAGE_FORMAT_VERSION,
            author_allowlist_source: options.author_allowlist_url.clone(),
            policy: policy.clone(),
            next_author: indexed_state.map_or(0, |state| state.next_author),
            events_seen: indexed_state.map_or(0, |state| state.events_seen),
            events_selected: indexed_state.map_or(0, |state| state.events_selected),
            live_bytes_selected: indexed_state.map_or(0, |state| state.live_bytes_selected),
        },
    };
    while let Some(recovered) = recover_uncheckpointed_stage_segment(paths.staging, &state)? {
        let cids = recovered
            .event_cids
            .iter()
            .map(|root| parse_root_text(root))
            .collect::<Result<Vec<_>>>()?;
        event_store.load_event_blobs(cids).await.with_context(|| {
            format!(
                "verify recovered durable staged event blobs for authors {}..{}",
                recovered.start_author, recovered.end_author
            )
        })?;
        advance_stage_state_from_segment(&mut state, &recovered)?;
        persist_stage_state(paths.staging, &state)?;
        eprintln!(
            "Recovered staged segment after publish/checkpoint crash: authors={}/{} segment={}..{} events_seen={} events_selected={} live_bytes={}",
            state.next_author,
            authors.len(),
            recovered.start_author,
            recovered.end_author,
            state.events_seen,
            state.events_selected,
            state.live_bytes_selected
        );
    }
    state.author_allowlist_source = options.author_allowlist_url.clone();
    state.policy = policy;
    persist_stage_state(paths.staging, &state)?;

    let checkpoint_authors = options
        .checkpoint_authors
        .min(options.author_batch_size)
        .max(1);
    while state.next_author < authors.len() {
        let mut announced_backpressure = false;
        loop {
            let v3_progress = bulk_projection::load_bulk_tranche_progress(paths.projection)?;
            let projected = if v3_progress.is_none() {
                load_crawl_state(paths.projection)?
            } else {
                None
            };
            let projected_author = v3_progress
                .map(|(next_author, _)| next_author)
                .or_else(|| projected.as_ref().map(|state| state.next_author))
                .unwrap_or(0);
            let projected_live_bytes = v3_progress
                .map(|(_, live_bytes_selected)| live_bytes_selected)
                .or_else(|| projected.as_ref().map(|state| state.live_bytes_selected))
                .unwrap_or(0);
            let authors_ahead = state.next_author.saturating_sub(projected_author);
            let live_bytes_ahead = state
                .live_bytes_selected
                .saturating_sub(projected_live_bytes);
            // Authors vary by orders of magnitude and empty-author segments are
            // tiny, so an author-count limit can re-couple fast relay fetching
            // to slow projection while barely using the staging disk. Bound the
            // queue by selected live bytes instead.
            if live_bytes_ahead < MAX_STAGED_LIVE_BYTES_AHEAD {
                break;
            }
            if !announced_backpressure {
                eprintln!(
                    "Nostr staging backpressure: fetched_authors={} projected_authors={} authors_ahead={} live_bytes_ahead={}",
                    state.next_author,
                    projected_author,
                    authors_ahead,
                    live_bytes_ahead
                );
                announced_backpressure = true;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if state.live_bytes_selected >= options.max_live_bytes {
            anyhow::bail!(
                "Nostr staging live-byte budget exhausted after {}/{} authors; the durable fetch cursor was preserved",
                state.next_author,
                authors.len()
            );
        }
        if options
            .max_events_seen
            .is_some_and(|limit| state.events_seen >= limit)
        {
            anyhow::bail!(
                "Nostr staging relay-event budget exhausted after {}/{} authors; the durable fetch cursor was preserved",
                state.next_author,
                authors.len()
            );
        }

        let start = state.next_author;
        let end = start.saturating_add(checkpoint_authors).min(authors.len());
        let remaining_live_bytes = options
            .max_live_bytes
            .saturating_sub(state.live_bytes_selected);
        let remaining_events_seen = options
            .max_events_seen
            .map(|limit| limit.saturating_sub(state.events_seen));
        let started = Instant::now();
        let bridge = NostrBridge::with_event_store_options(
            Arc::clone(&store),
            build_crawl_config(
                options,
                relays,
                Some(authors[start..end].to_vec()),
                remaining_live_bytes,
                remaining_events_seen,
                end - start,
            ),
            nostr_event_store_options(options),
        );
        let report = bridge
            .fetch_authors(&authors[start..end])
            .await
            .with_context(|| format!("fetch allowlisted authors {start}..{end}"))?;
        if report.authors_processed != end - start {
            anyhow::bail!(
                "author fetch stopped after {} of {} authors; staging cursor was not advanced",
                report.authors_processed,
                end - start
            );
        }

        let event_cids = event_store
            .store_event_blobs(report.applied_events)
            .await
            .with_context(|| {
                format!("persist individual event blobs for authors {start}..{end}")
            })?;
        durable_store
            .force_sync()
            .context("force-sync staged Nostr event blobs")?;
        // The staging cursor is a promise that every referenced event can be
        // consumed without contacting a relay again. Verify that promise only
        // after the blob store's durability boundary and before publishing the
        // segment or advancing crawl-state.json.
        for (chunk_index, event_cid_chunk) in event_cids
            .chunks(options.index_commit_batch_size.max(1))
            .enumerate()
        {
            event_store
                .load_event_blobs(event_cid_chunk.iter().cloned())
                .await
                .with_context(|| {
                    let chunk_start = chunk_index * options.index_commit_batch_size.max(1);
                    let chunk_end = chunk_start + event_cid_chunk.len();
                    format!(
                        "verify durable staged event blobs for authors {start}..{end} at offsets {chunk_start}..{chunk_end}; staging cursor was not advanced"
                    )
                })?;
        }
        let fetched_segment = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: start,
            end_author: end,
            events_seen: report.events_seen,
            events_selected: report.events_selected,
            live_bytes_selected: report.live_bytes_selected,
            event_cids: event_cids
                .iter()
                .map(cid_to_nhash)
                .collect::<Result<Vec<_>>>()?,
        };
        let segment = persist_stage_segment(paths.staging, &fetched_segment, &state.policy)?;

        advance_stage_state_from_segment(&mut state, &segment)?;
        persist_stage_state(paths.staging, &state)?;
        hashtree_cli::diagnostics::trim_process_allocations();
        eprintln!(
            "Nostr staging checkpoint: authors={}/{} events_seen={} events_selected={} live_bytes={} event_blobs={} relay_fetch_select_ms={} batch_elapsed_ms={}",
            state.next_author,
            authors.len(),
            state.events_seen,
            state.events_selected,
            state.live_bytes_selected,
            segment.event_cids.len(),
            report.relay_fetch_select_ms,
            started.elapsed().as_millis()
        );
    }

    Ok(CrawlReport {
        root: indexed_state
            .and_then(|state| state.root.as_deref())
            .map(parse_root_text)
            .transpose()?,
        authors_considered: authors.len(),
        authors_processed: state.next_author,
        events_seen: state.events_seen,
        events_selected: state.events_selected,
        live_bytes_selected: state.live_bytes_selected,
        ..CrawlReport::default()
    })
}

async fn project_staged_allowlist(
    stores: ProjectionStores<'_>,
    data_dir: &Path,
    staging_data_dir: &Path,
    options: &SocialGraphIndexOptions,
    authors: &[String],
    policy: &IndexedNostrCrawlPolicy,
    state: &mut IndexedNostrCrawlState,
) -> Result<CrawlReport> {
    let event_store = NostrEventStore::with_options(
        stores.durable.store_arc(),
        nostr_event_store_options(options),
    );
    let staging_event_store = NostrEventStore::with_options(
        stores.staging.store_arc(),
        nostr_event_store_options(options),
    );
    let mut total_index_build_ms = 0u128;

    loop {
        let stage = load_stage_state(staging_data_dir)?.context(
            "no durable Nostr staging state exists; start --stage-only before projection",
        )?;
        validate_stage_state(&stage, policy, authors.len())?;
        if state.next_author >= stage.next_author {
            if state.next_author >= authors.len() || !options.projection_follow {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        let projection_started = Instant::now();
        let start = state.next_author;
        let segment = load_stage_segment(staging_data_dir, start, policy)?;
        if segment.end_author > stage.next_author || segment.end_author > authors.len() {
            anyhow::bail!(
                "staged segment at author {start} extends beyond the durable staging cursor"
            );
        }
        if state.staged_segment_event_offset > segment.event_cids.len() {
            anyhow::bail!(
                "durable projection offset {} exceeds the {} events in staged segment at author {start}",
                state.staged_segment_event_offset,
                segment.event_cids.len()
            );
        }

        // Match the outer durability unit to the event store's bounded index
        // commit. This prevents a complete 65k-event author from remaining in
        // memory (and uncheckpointed) while all nine indexes are constructed.
        let chunk_size = options
            .index_commit_batch_size
            .min(options.projection_event_limit)
            .max(1);
        let event_offset = state.staged_segment_event_offset;
        let event_end = event_offset
            .saturating_add(chunk_size)
            .min(segment.event_cids.len());
        let cids = segment.event_cids[event_offset..event_end]
            .iter()
            .map(|root| parse_root_text(root))
            .collect::<Result<Vec<_>>>()?;
        let events = staging_event_store
            .load_event_blobs(cids)
            .await
            .with_context(|| {
                format!(
                    "load staged event blobs for authors {start}..{} at offsets {event_offset}..{event_end}",
                    segment.end_author
                )
            })?;
        let profile_events = events
            .iter()
            .filter(|event| event.kind == 0)
            .cloned()
            .collect::<Vec<_>>();
        let current_root = state
            .root
            .as_deref()
            .map(parse_root_text)
            .transpose()
            .context("parse projection root")?;
        validate_reachable_root(&event_store, current_root.as_ref(), "projection root").await?;
        let index_started = Instant::now();
        let build_report = event_store
            .build_with_superseded_nodes(current_root.as_ref(), events)
            .await
            .with_context(|| {
                format!(
                    "project staged authors {start}..{} at offsets {event_offset}..{event_end}",
                    segment.end_author
                )
            })?;
        let next_root = build_report.root;
        let index_build_ms = index_started.elapsed().as_millis();
        total_index_build_ms = total_index_build_ms.saturating_add(index_build_ms);
        validate_reachable_root(&event_store, next_root.as_ref(), "new projection root").await?;

        if !profile_events.is_empty() {
            let parsed = profile_events
                .iter()
                .map(|event| event.to_nostr_sdk_event().map_err(anyhow::Error::from))
                .collect::<Result<Vec<_>>>()?;
            stores
                .graph
                .sync_profile_index_for_events(&parsed)
                .context("sync projected profile search batch")?;
        }

        let completed_segment = apply_projected_segment_checkpoint(
            state,
            &segment,
            event_end,
            next_root.as_ref().map(cid_to_nhash).transpose()?,
        )?;
        let checkpoint_sync_started = Instant::now();
        stores
            .graph
            .force_sync()
            .context("force-sync projected social graph storage")?;
        stores
            .durable
            .force_sync()
            .context("force-sync projected Nostr indexes")?;
        let checkpoint_sync_ms = checkpoint_sync_started.elapsed().as_millis();
        persist_crawl_state(data_dir, state)?;
        let gc_started = Instant::now();
        let superseded_nodes_deleted = event_store
            .delete_superseded_nodes(&build_report.superseded_nodes)
            .await
            .context("delete superseded projection index nodes")?;
        let gc_ms = gc_started.elapsed().as_millis();
        hashtree_cli::diagnostics::trim_process_allocations();
        eprintln!(
            "Nostr projection checkpoint: authors={}/{} staged_authors={} events_seen={} events_selected={} live_bytes={} segment_authors={} segment_event_offset={}/{} projected_events={} completed_segment={} index_build_ms={} checkpoint_sync_ms={} superseded_nodes_deleted={} gc_ms={} batch_elapsed_ms={}",
            state.next_author,
            authors.len(),
            stage.next_author,
            state.events_seen,
            state.events_selected,
            state.live_bytes_selected,
            segment.end_author.saturating_sub(segment.start_author),
            state.staged_segment_event_offset,
            segment.event_cids.len(),
            event_end.saturating_sub(event_offset),
            completed_segment,
            index_build_ms,
            checkpoint_sync_ms,
            superseded_nodes_deleted,
            gc_ms,
            projection_started.elapsed().as_millis()
        );
    }

    Ok(CrawlReport {
        root: state.root.as_deref().map(parse_root_text).transpose()?,
        authors_considered: authors.len(),
        authors_processed: state.next_author,
        events_seen: state.events_seen,
        events_selected: state.events_selected,
        live_bytes_selected: state.live_bytes_selected,
        index_build_ms: total_index_build_ms,
        ..CrawlReport::default()
    })
}

fn apply_projected_segment_checkpoint(
    state: &mut IndexedNostrCrawlState,
    segment: &StagedAuthorSegment,
    event_end: usize,
    root: Option<String>,
) -> Result<bool> {
    if segment.start_author != state.next_author {
        anyhow::bail!(
            "staged segment starts at author {}, but projection cursor is {}",
            segment.start_author,
            state.next_author
        );
    }
    if event_end < state.staged_segment_event_offset || event_end > segment.event_cids.len() {
        anyhow::bail!(
            "invalid projected event range {}..{} for {}-event segment at author {}",
            state.staged_segment_event_offset,
            event_end,
            segment.event_cids.len(),
            segment.start_author
        );
    }

    state.root = root;
    let completed_segment = event_end == segment.event_cids.len();
    if completed_segment {
        state.next_author = segment.end_author;
        state.staged_segment_event_offset = 0;
        state.events_seen = state.events_seen.saturating_add(segment.events_seen);
        state.events_selected = state
            .events_selected
            .saturating_add(segment.events_selected);
        state.live_bytes_selected = state
            .live_bytes_selected
            .saturating_add(segment.live_bytes_selected);
    } else {
        state.staged_segment_event_offset = event_end;
    }
    Ok(completed_segment)
}

async fn validate_reachable_root(
    event_store: &NostrEventStore<hashtree_cli::storage::StorageRouter>,
    root: Option<&Cid>,
    label: &str,
) -> Result<()> {
    let Some(root) = root else {
        return Ok(());
    };
    event_store
        .validate_index_root(Some(root))
        .await
        .with_context(|| {
            format!(
                "validate {label} {}",
                cid_to_nhash(root).unwrap_or_default()
            )
        })
}

fn build_bounded_report(
    relays: &[String],
    options: &SocialGraphIndexOptions,
    crawl_report: CrawlReport,
    profile_search_root: Option<String>,
) -> Result<IndexedNostrReport> {
    Ok(IndexedNostrReport {
        root: crawl_report.root.as_ref().map(cid_to_nhash).transpose()?,
        profile_search_root,
        authors_considered: crawl_report.authors_considered,
        authors_processed: crawl_report.authors_processed,
        events_seen: crawl_report.events_seen,
        events_selected: crawl_report.events_selected,
        live_bytes_selected: crawl_report.live_bytes_selected,
        warm_graph_seconds: options.warm_graph_for.as_secs(),
        graph_crawl_depth: options.graph_crawl_depth,
        full_graph_recrawl: options.full_graph_recrawl,
        max_events_seen: options.max_events_seen,
        max_follow_distance: options.max_follow_distance,
        max_authors: options.max_authors,
        max_live_bytes: options.max_live_bytes,
        per_author_live_bytes: options.per_author_live_bytes,
        relay_event_max_bytes: options.relay_event_max_bytes,
        global_relay_scan: options.global_relay_scan,
        full_author_history: options.full_author_history,
        negentropy_only: options.negentropy_only,
        relay_page_size: options.relay_page_size,
        max_relay_pages: options.max_relay_pages,
        relays: relays.to_vec(),
        top_authors: Vec::new(),
        top_kinds: Vec::new(),
        top_hashtags: Vec::new(),
        recent_events: Vec::new(),
    })
}

async fn sync_socialgraph_profile_index_from_root(
    graph_store: &socialgraph::SocialGraphStore,
    event_store: &NostrEventStore<hashtree_cli::storage::StorageRouter>,
    root: Option<&Cid>,
) -> Result<()> {
    let Some(root) = root else {
        return Ok(());
    };

    let events = event_store
        .list_recent(Some(root), ListEventsOptions::default())
        .await
        .context("list crawled events for social graph sync")?;
    if events.is_empty() {
        return Ok(());
    }

    let parsed = events
        .into_iter()
        .map(|event| event.to_nostr_sdk_event())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    graph_store
        .sync_profile_index_for_events(&parsed)
        .context("sync crawled profile search index")?;
    Ok(())
}

async fn warm_social_graph(
    graph_store: Arc<dyn SocialGraphBackend>,
    keys: Keys,
    relays: Vec<String>,
    crawl_depth: u32,
    full_graph_recrawl: bool,
    concurrent_batches: usize,
    duration: Duration,
) -> Result<()> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let crawler = SocialGraphCrawler::new(graph_store, keys, relays, crawl_depth)
        .with_concurrent_batches(concurrent_batches)
        .with_full_recrawl(full_graph_recrawl);
    let mut handle = tokio::spawn(async move {
        crawler.crawl(shutdown_rx).await;
    });

    tokio::time::sleep(duration).await;
    let _ = shutdown_tx.send(true);

    match tokio::time::timeout(Duration::from_secs(5), &mut handle).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(anyhow::anyhow!("social graph warmup task failed: {err}")),
        Err(_) => {
            handle.abort();
            match handle.await {
                Err(err) if err.is_cancelled() => Ok(()),
                Ok(()) => Ok(()),
                Err(err) => Err(anyhow::anyhow!(
                    "social graph warmup task failed after abort: {err}"
                )),
            }
        }
    }
}

async fn build_report(
    event_store: &NostrEventStore<hashtree_cli::storage::StorageRouter>,
    relays: &[String],
    options: &SocialGraphIndexOptions,
    crawl_report: CrawlReport,
) -> Result<IndexedNostrReport> {
    let root = crawl_report.root.as_ref().map(cid_to_nhash).transpose()?;
    let mut events = if let Some(root_cid) = crawl_report.root.as_ref() {
        event_store
            .list_recent(Some(root_cid), ListEventsOptions::default())
            .await?
    } else {
        Vec::new()
    };

    let mut by_author: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_hashtag: BTreeMap<String, usize> = BTreeMap::new();

    for event in &events {
        *by_author.entry(event.pubkey.clone()).or_default() += 1;
        *by_kind.entry(event.kind.to_string()).or_default() += 1;
        for hashtag in hashtags(event) {
            *by_hashtag.entry(hashtag).or_default() += 1;
        }
    }

    events.truncate(TOP_ITEMS_LIMIT);

    Ok(IndexedNostrReport {
        root,
        profile_search_root: None,
        authors_considered: crawl_report.authors_considered,
        authors_processed: crawl_report.authors_processed,
        events_seen: crawl_report.events_seen,
        events_selected: crawl_report.events_selected,
        live_bytes_selected: crawl_report.live_bytes_selected,
        warm_graph_seconds: options.warm_graph_for.as_secs(),
        graph_crawl_depth: options.graph_crawl_depth,
        full_graph_recrawl: options.full_graph_recrawl,
        max_events_seen: options.max_events_seen,
        max_follow_distance: options.max_follow_distance,
        max_authors: options.max_authors,
        max_live_bytes: options.max_live_bytes,
        per_author_live_bytes: options.per_author_live_bytes,
        relay_event_max_bytes: options.relay_event_max_bytes,
        global_relay_scan: options.global_relay_scan,
        full_author_history: options.full_author_history,
        negentropy_only: options.negentropy_only,
        relay_page_size: options.relay_page_size,
        max_relay_pages: options.max_relay_pages,
        relays: relays.to_vec(),
        top_authors: ranked_counts(by_author),
        top_kinds: ranked_counts(by_kind),
        top_hashtags: ranked_counts(by_hashtag),
        recent_events: events
            .into_iter()
            .map(|event| RecentIndexedEvent {
                hashtags: hashtags(&event),
                id: event.id,
                pubkey: event.pubkey,
                created_at: event.created_at,
                kind: event.kind,
            })
            .collect(),
    })
}

fn ranked_counts(counts: BTreeMap<String, usize>) -> Vec<RankedCount> {
    let mut out: Vec<RankedCount> = counts
        .into_iter()
        .map(|(key, count)| RankedCount { key, count })
        .collect();
    out.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.key.cmp(&right.key))
    });
    out.truncate(TOP_ITEMS_LIMIT);
    out
}

async fn load_author_allowlist(
    url: Option<&str>,
    max_authors: usize,
) -> Result<Option<Vec<String>>> {
    let Some(url) = url else {
        return Ok(None);
    };

    let body = fetch_author_allowlist_text(&reqwest::Client::new(), url).await?;
    let authors = parse_author_allowlist(&body, max_authors);
    if authors.is_empty() {
        anyhow::bail!("author allowlist from {url} did not contain any valid pubkeys");
    }
    Ok(Some(authors))
}

async fn fetch_author_allowlist_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let mut last_error = None;
    for attempt in 0..3 {
        match client.get(url).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => {
                    return response
                        .text()
                        .await
                        .with_context(|| format!("decode author allowlist from {url}"));
                }
                Err(err) => {
                    last_error = Some(
                        anyhow::Error::new(err)
                            .context(format!("author allowlist request failed for {url}")),
                    );
                }
            },
            Err(err) => {
                last_error = Some(
                    anyhow::Error::new(err).context(format!("fetch author allowlist from {url}")),
                );
            }
        }

        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(attempt as u64 + 1)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("fetch author allowlist from {url} failed")))
}

fn parse_author_allowlist(body: &str, max_authors: usize) -> Vec<String> {
    let mut authors = Vec::new();
    let mut seen = HashSet::new();
    for line in body.lines().map(str::trim) {
        if line.len() != 64 {
            continue;
        }
        if !line
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            continue;
        }
        if seen.insert(line) {
            authors.push(line.to_owned());
            if authors.len() >= max_authors {
                break;
            }
        }
    }
    authors
}

fn parse_nostr_filters_json(input: &str) -> Result<Vec<NostrFilter>> {
    let value: serde_json::Value =
        serde_json::from_str(input).context("parse Nostr filter JSON")?;
    let filters = match value {
        serde_json::Value::Array(items) if is_req_envelope(&items) => items
            .into_iter()
            .skip(2)
            .map(parse_nostr_filter_value)
            .collect::<Result<Vec<_>>>()?,
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(parse_nostr_filter_value)
            .collect::<Result<Vec<_>>>()?,
        value => vec![parse_nostr_filter_value(value)?],
    };
    if filters.is_empty() {
        anyhow::bail!("Nostr filter JSON did not contain any filters");
    }
    Ok(filters)
}

fn is_req_envelope(items: &[serde_json::Value]) -> bool {
    items.len() >= 3 && items.first().and_then(|item| item.as_str()) == Some("REQ")
}

fn parse_nostr_filter_value(value: serde_json::Value) -> Result<NostrFilter> {
    serde_json::from_value(value).context("decode Nostr filter")
}

fn parse_nostr_events_json(value: &serde_json::Value) -> Result<Vec<NostrSdkEvent>> {
    let events = nostr_events_array(value)?;
    events
        .iter()
        .map(|value| serde_json::from_value(value.clone()).context("decode Nostr event"))
        .collect()
}

fn nostr_events_array(value: &serde_json::Value) -> Result<&[serde_json::Value]> {
    if let Some(events) = value.get("events").and_then(serde_json::Value::as_array) {
        return Ok(events);
    }
    if let Some(data) = value.get("data") {
        if let Some(events) = data.get("events").and_then(serde_json::Value::as_array) {
            return Ok(events);
        }
    }
    value
        .as_array()
        .map(Vec::as_slice)
        .context("expected a Nostr event array or object with an events array")
}

fn write_nostr_index_import_output(
    output: &NostrIndexImportOutput,
    out: Option<&Path>,
) -> Result<()> {
    let json = serde_json::to_string_pretty(output).context("encode Nostr import output")?;
    match out {
        Some(path) if path != Path::new("-") => {
            std::fs::write(path, format!("{json}\n"))
                .with_context(|| format!("write Nostr import output to {}", path.display()))?;
        }
        _ => {
            println!("{json}");
        }
    }
    Ok(())
}

fn write_nostr_index_query_output(
    output: &NostrIndexQueryOutput,
    out: Option<&Path>,
) -> Result<()> {
    let json = serde_json::to_string_pretty(output).context("encode Nostr query output")?;
    match out {
        Some(path) if path != Path::new("-") => {
            std::fs::write(path, format!("{json}\n"))
                .with_context(|| format!("write Nostr query output to {}", path.display()))?;
        }
        _ => {
            println!("{json}");
        }
    }
    Ok(())
}

fn hashtags(event: &StoredNostrEvent) -> Vec<String> {
    let mut out = Vec::new();
    for tag in event.tags.iter() {
        if tag.first().is_some_and(|name| name == "t") {
            if let Some(value) = tag.get(1) {
                if !value.is_empty() {
                    out.push(value.to_lowercase());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn persist_report(data_dir: &Path, report: &IndexedNostrReport) -> Result<()> {
    let output_dir = data_dir.join(INDEX_DIR);
    std::fs::create_dir_all(&output_dir)?;

    let mut saved_report = report.clone();
    if let Some(root) = &saved_report.root {
        saved_report.root = Some(cid_to_nhash(&parse_root_text(root)?)?);
    }

    let report_path = output_dir.join(LATEST_REPORT_FILE);
    std::fs::write(&report_path, serde_json::to_vec_pretty(&saved_report)?)?;

    let root_path = output_dir.join(LATEST_ROOT_FILE);
    if let Some(root) = &saved_report.root {
        std::fs::write(root_path, format!("{root}\n"))?;
    } else if root_path.exists() {
        std::fs::remove_file(root_path)?;
    }

    Ok(())
}

fn persist_latest_root(data_dir: &Path, root: &Cid) -> Result<()> {
    let output_dir = data_dir.join(INDEX_DIR);
    std::fs::create_dir_all(&output_dir)?;
    std::fs::write(
        output_dir.join(LATEST_ROOT_FILE),
        format!("{}\n", cid_to_nhash(root)?),
    )?;
    Ok(())
}

fn load_existing_root(data_dir: &Path) -> Result<Option<Cid>> {
    let index_dir = data_dir.join(INDEX_DIR);
    for path in [
        index_dir.join(LATEST_ROOT_FILE),
        index_dir.join(CHECKPOINT_ROOT_FILE),
        data_dir
            .join(MIRROR_STATE_DIR)
            .join(MIRROR_UPLOADED_EVENT_ROOT_FILE),
    ] {
        if path.exists() {
            if let Some(root) = load_root_from_path(&path)? {
                return Ok(Some(root));
            }
        }
    }

    Ok(None)
}

fn load_root_from_path(path: &Path) -> Result<Option<Cid>> {
    let root = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    parse_root_text(trimmed)
        .map(Some)
        .with_context(|| format!("parse nostr index root from {}", path.display()))
}

fn build_crawl_policy(
    options: &SocialGraphIndexOptions,
    relays: &[String],
    authors: &[String],
    base_root: Option<&Cid>,
) -> Result<IndexedNostrCrawlPolicy> {
    let mut allowlist_hash = Sha256::new();
    for author in authors {
        allowlist_hash.update(author.as_bytes());
        allowlist_hash.update(b"\n");
    }
    Ok(IndexedNostrCrawlPolicy {
        base_root: base_root.map(cid_to_nhash).transpose()?,
        author_allowlist_sha256: hex::encode(allowlist_hash.finalize()),
        author_count: authors.len(),
        relays: relays.to_vec(),
        require_all_relays: false,
        max_events_seen: options.max_events_seen,
        max_authors: options.max_authors,
        max_follow_distance: options.max_follow_distance,
        max_live_bytes: options.max_live_bytes,
        author_batch_size: options.author_batch_size,
        checkpoint_authors: options.checkpoint_authors,
        per_author_event_limit: options.per_author_event_limit,
        per_author_kind_event_limit: options.per_author_kind_event_limit,
        per_author_live_bytes: options.per_author_live_bytes,
        fetch_timeout_millis: options
            .fetch_timeout
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        relay_event_max_bytes: options.relay_event_max_bytes,
        global_relay_scan: options.global_relay_scan,
        full_author_history: options.full_author_history,
        negentropy_only: options.negentropy_only,
        relay_page_size: options.relay_page_size,
        max_relay_pages: options.max_relay_pages,
        kinds: options.kinds.clone(),
    })
}

fn load_crawl_state(data_dir: &Path) -> Result<Option<IndexedNostrCrawlState>> {
    let path = data_dir.join(INDEX_DIR).join(CRAWL_STATE_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", path.display()))
        .map(Some)
}

fn load_stage_state(data_dir: &Path) -> Result<Option<StagedNostrCrawlState>> {
    let path = data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", path.display()))
        .map(Some)
}

fn validate_stage_state(
    state: &StagedNostrCrawlState,
    expected_policy: &IndexedNostrCrawlPolicy,
    author_count: usize,
) -> Result<()> {
    if state.version != STAGE_FORMAT_VERSION {
        anyhow::bail!(
            "unsupported Nostr staging state version {} (expected {})",
            state.version,
            STAGE_FORMAT_VERSION
        );
    }
    let mut resumed_identity = state.policy.clone();
    // Segment claims pin each historical start to its immutable end and body
    // digest, so future fetch cadence can change without making mixed-width
    // staged history ambiguous or requiring a directory scan.
    resumed_identity.author_batch_size = expected_policy.author_batch_size;
    resumed_identity.checkpoint_authors = expected_policy.checkpoint_authors;
    if resumed_identity.require_all_relays && !expected_policy.require_all_relays {
        resumed_identity.require_all_relays = false;
    }
    if &resumed_identity != expected_policy {
        anyhow::bail!(
            "Nostr staging policy or ordered author allowlist changed; refusing to reuse the durable fetch cursor"
        );
    }
    if state.next_author > author_count {
        anyhow::bail!(
            "Nostr staging cursor {} exceeds the validated author count {}",
            state.next_author,
            author_count
        );
    }
    Ok(())
}

fn create_dir_all_durable(path: &Path) -> Result<()> {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir()
            .context("resolve current directory for durable hierarchy")?
            .join(path);
        absolute.as_path()
    };
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        cursor = cursor
            .parent()
            .context("durable directory hierarchy has no existing ancestor")?;
    }
    if !cursor.is_dir() {
        anyhow::bail!(
            "durable directory ancestor is not a directory: {}",
            cursor.display()
        );
    }
    for directory in missing.iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create durable directory {}", directory.display()));
            }
        }
        #[cfg(unix)]
        {
            File::open(directory)
                .with_context(|| format!("open {} for fsync", directory.display()))?
                .sync_all()
                .with_context(|| format!("fsync {}", directory.display()))?;
            let parent = directory
                .parent()
                .context("created durable directory has no parent")?;
            File::open(parent)
                .with_context(|| format!("open {} for fsync", parent.display()))?
                .sync_all()
                .with_context(|| format!("fsync {}", parent.display()))?;
        }
    }
    Ok(())
}

fn stage_segment_path(data_dir: &Path, start_author: usize, end_author: usize) -> PathBuf {
    data_dir
        .join(STAGE_DIR)
        .join(STAGE_SEGMENTS_DIR)
        .join(format!("{start_author:012}-{end_author:012}.json"))
}

fn stage_segment_claim_path(data_dir: &Path, start_author: usize) -> PathBuf {
    data_dir
        .join(STAGE_DIR)
        .join(STAGE_SEGMENT_CLAIMS_DIR)
        .join(format!("{start_author:012}.json"))
}

fn stage_bytes_sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn validate_stage_sha256(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} must be a lowercase 64-character SHA-256");
    }
    Ok(())
}

fn stage_segment_claim(segment: &StagedAuthorSegment, bytes: &[u8]) -> Result<StagedSegmentClaim> {
    if segment.version != STAGE_FORMAT_VERSION || segment.end_author <= segment.start_author {
        anyhow::bail!("cannot claim an invalid staged author segment");
    }
    Ok(StagedSegmentClaim {
        version: STAGE_SEGMENT_CLAIM_VERSION,
        start_author: segment.start_author,
        end_author: segment.end_author,
        body_sha256: stage_bytes_sha256(bytes),
    })
}

fn persist_stage_segment_claim(
    data_dir: &Path,
    segment: &StagedAuthorSegment,
    bytes: &[u8],
) -> Result<()> {
    let claim = stage_segment_claim(segment, bytes)?;
    let mut claim_bytes =
        serde_json::to_vec(&claim).context("encode Nostr staging segment claim")?;
    claim_bytes.push(b'\n');
    persist_immutable_bytes(
        &stage_segment_claim_path(data_dir, segment.start_author),
        &claim_bytes,
        "Nostr staging segment boundary claim",
    )
}

fn load_stage_segment_claim(data_dir: &Path, start_author: usize) -> Result<StagedSegmentClaim> {
    let claim_path = stage_segment_claim_path(data_dir, start_author);
    let claim_bytes = std::fs::read(&claim_path).with_context(|| {
        format!(
            "read immutable staged segment boundary claim at {}",
            claim_path.display()
        )
    })?;
    let claim: StagedSegmentClaim = serde_json::from_slice(&claim_bytes)
        .with_context(|| format!("parse {}", claim_path.display()))?;
    let mut canonical_claim =
        serde_json::to_vec(&claim).context("re-encode staged segment boundary claim")?;
    canonical_claim.push(b'\n');
    if claim_bytes != canonical_claim
        || claim.version != STAGE_SEGMENT_CLAIM_VERSION
        || claim.start_author != start_author
        || claim.end_author <= claim.start_author
    {
        anyhow::bail!(
            "invalid immutable staged segment boundary claim {}",
            claim_path.display()
        );
    }
    validate_stage_sha256("staged segment body SHA-256", &claim.body_sha256)?;
    Ok(claim)
}

fn persist_json_atomic<T: serde::Serialize>(path: &Path, value: &T, label: &str) -> Result<()> {
    let output_dir = path
        .parent()
        .context("atomic JSON path has no parent directory")?;
    create_dir_all_durable(output_dir)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("atomic JSON path has no UTF-8 file name")?;
    let temp_path = output_dir.join(format!(".{file_name}.tmp"));
    let mut bytes = serde_json::to_vec(value).with_context(|| format!("encode {label}"))?;
    bytes.push(b'\n');

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp_path)
        .with_context(|| format!("open {}", temp_path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync {}", temp_path.display()))?;
    drop(file);
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("rename {} to {}", temp_path.display(), path.display()))?;
    #[cfg(unix)]
    File::open(output_dir)
        .with_context(|| format!("open {} for fsync", output_dir.display()))?
        .sync_all()
        .with_context(|| format!("fsync {}", output_dir.display()))?;
    Ok(())
}

fn persist_stage_state(data_dir: &Path, state: &StagedNostrCrawlState) -> Result<()> {
    persist_json_atomic(
        &data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE),
        state,
        "Nostr staging state",
    )
}

fn fsync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("durable path has no parent directory")?;
    #[cfg(unix)]
    File::open(parent)
        .with_context(|| format!("open {} for fsync", parent.display()))?
        .sync_all()
        .with_context(|| format!("fsync {}", parent.display()))?;
    Ok(())
}

/// Publish immutable bytes without replacing an existing destination.
///
/// A deterministic pending name makes a fully written orphan adoptable and a
/// partially written crash orphan repairable. Callers hold the namespace's
/// exclusive writer lock, so repairing the unpublished pending inode cannot
/// race another legitimate writer. The destination hard link remains the
/// no-replace commit point and is never overwritten.
fn persist_immutable_bytes(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("immutable path has no parent directory")?;
    create_dir_all_durable(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("immutable path has no UTF-8 file name")?;
    let pending = parent.join(format!(".{file_name}{IMMUTABLE_PENDING_SUFFIX}"));

    if path.exists() {
        let existing =
            std::fs::read(path).with_context(|| format!("read existing {}", path.display()))?;
        if existing != bytes {
            anyhow::bail!(
                "{label} already exists with different bytes at {}",
                path.display()
            );
        }
        if pending.exists() {
            std::fs::remove_file(&pending)
                .with_context(|| format!("remove obsolete pending {}", pending.display()))?;
        }
        fsync_parent(path)?;
        return Ok(());
    }

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)
    {
        Ok(mut file) => {
            file.write_all(bytes)
                .with_context(|| format!("write {}", pending.display()))?;
            file.sync_all()
                .with_context(|| format!("fsync {}", pending.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let orphan = std::fs::read(&pending)
                .with_context(|| format!("read pending {}", pending.display()))?;
            if orphan != bytes {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&pending)
                    .with_context(|| format!("repair partial pending {}", pending.display()))?;
                file.write_all(bytes)
                    .with_context(|| format!("rewrite {}", pending.display()))?;
                file.sync_all()
                    .with_context(|| format!("fsync repaired {}", pending.display()))?;
            } else {
                File::open(&pending)
                    .with_context(|| format!("open pending {} for fsync", pending.display()))?
                    .sync_all()
                    .with_context(|| format!("fsync pending {}", pending.display()))?;
            }
        }
        Err(error) => {
            return Err(error).with_context(|| format!("create {}", pending.display()));
        }
    }

    match std::fs::hard_link(&pending, path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing =
                std::fs::read(path).with_context(|| format!("read existing {}", path.display()))?;
            if existing != bytes {
                anyhow::bail!("{label} raced with different bytes at {}", path.display());
            }
        }
        Err(error) => {
            return Err(error).with_context(|| format!("commit immutable {}", path.display()));
        }
    }
    fsync_parent(path)?;
    std::fs::remove_file(&pending)
        .with_context(|| format!("remove committed {}", pending.display()))?;
    fsync_parent(path)?;
    Ok(())
}

fn stage_segment_file_parts(path: &Path) -> Result<(usize, usize)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("staged segment path has no UTF-8 file name")?;
    let stem = name
        .strip_suffix(".json")
        .with_context(|| format!("staged segment file `{name}` has the wrong extension"))?;
    let (start, end) = stem
        .split_once('-')
        .with_context(|| format!("staged segment file `{name}` has no boundary separator"))?;
    if start.len() != 12
        || end.len() != 12
        || !start.bytes().all(|byte| byte.is_ascii_digit())
        || !end.bytes().all(|byte| byte.is_ascii_digit())
    {
        anyhow::bail!("staged segment file `{name}` has non-canonical boundaries");
    }
    Ok((
        start
            .parse()
            .with_context(|| format!("parse staged segment start in `{name}`"))?,
        end.parse()
            .with_context(|| format!("parse staged segment end in `{name}`"))?,
    ))
}

fn expected_stage_segment_end(
    policy: &IndexedNostrCrawlPolicy,
    start_author: usize,
) -> Result<usize> {
    if start_author >= policy.author_count {
        anyhow::bail!(
            "staged segment start {start_author} is outside {} policy authors",
            policy.author_count
        );
    }
    let width = policy
        .checkpoint_authors
        .min(policy.author_batch_size)
        .max(1);
    Ok(start_author
        .checked_add(width)
        .context("staged segment boundary overflow")?
        .min(policy.author_count))
}

fn recover_uncheckpointed_stage_segment(
    data_dir: &Path,
    state: &StagedNostrCrawlState,
) -> Result<Option<StagedAuthorSegment>> {
    if state.next_author >= state.policy.author_count {
        return Ok(None);
    }
    let start = state.next_author;
    let claim_path = stage_segment_claim_path(data_dir, start);
    if claim_path.exists() {
        return load_stage_segment(data_dir, start, &state.policy).map(Some);
    }
    // The old persisted cadence remains authoritative until this recovery
    // check completes. It identifies the sole body that can have reached its
    // durable publish boundary before a crash stopped claim/state publication.
    let old_end = expected_stage_segment_end(&state.policy, start)?;
    let body_path = stage_segment_path(data_dir, start, old_end);
    if !body_path.exists() {
        return Ok(None);
    }
    let (_, bytes, segment) = load_stage_segment_path_with_bytes(&body_path, start, &state.policy)?;
    persist_stage_segment_claim(data_dir, &segment, &bytes)?;
    Ok(Some(segment))
}

fn advance_stage_state_from_segment(
    state: &mut StagedNostrCrawlState,
    segment: &StagedAuthorSegment,
) -> Result<()> {
    if segment.start_author != state.next_author
        || segment.end_author <= segment.start_author
        || segment.end_author > state.policy.author_count
    {
        anyhow::bail!(
            "cannot advance staging state at {} from durable segment {}..{}",
            state.next_author,
            segment.start_author,
            segment.end_author
        );
    }
    state.next_author = segment.end_author;
    state.events_seen = state
        .events_seen
        .checked_add(segment.events_seen)
        .context("staging events-seen counter overflow")?;
    state.events_selected = state
        .events_selected
        .checked_add(segment.events_selected)
        .context("staging events-selected counter overflow")?;
    state.live_bytes_selected = state
        .live_bytes_selected
        .checked_add(segment.live_bytes_selected)
        .context("staging live-byte counter overflow")?;
    Ok(())
}

fn persist_stage_segment(
    data_dir: &Path,
    segment: &StagedAuthorSegment,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<StagedAuthorSegment> {
    if segment.version != STAGE_FORMAT_VERSION || segment.end_author <= segment.start_author {
        anyhow::bail!("refusing to persist an invalid staged author segment");
    }
    let claim_path = stage_segment_claim_path(data_dir, segment.start_author);
    if claim_path.exists() {
        let claim = load_stage_segment_claim(data_dir, segment.start_author)?;
        if claim.end_author > policy.author_count {
            anyhow::bail!(
                "durable staged segment claim {}..{} exceeds {} policy authors",
                claim.start_author,
                claim.end_author,
                policy.author_count
            );
        }
        let body_path = stage_segment_path(data_dir, segment.start_author, claim.end_author);
        let (_, body_bytes, durable_segment) =
            load_stage_segment_path_with_bytes(&body_path, segment.start_author, policy)?;
        if stage_bytes_sha256(&body_bytes) != claim.body_sha256 {
            anyhow::bail!(
                "staged author segment body differs from immutable claim {}",
                claim_path.display()
            );
        }
        return Ok(durable_segment);
    }
    let expected_end = expected_stage_segment_end(policy, segment.start_author)?;
    if segment.end_author != expected_end {
        anyhow::bail!(
            "staged segment {}..{} differs from policy boundary {}..{}",
            segment.start_author,
            segment.end_author,
            segment.start_author,
            expected_end
        );
    }
    for cid in &segment.event_cids {
        parse_root_text(cid).context("parse staged event CID before immutable publish")?;
    }
    let mut bytes = serde_json::to_vec(segment).context("encode Nostr staging segment")?;
    bytes.push(b'\n');
    let body_path = stage_segment_path(data_dir, segment.start_author, segment.end_author);
    if body_path.exists() {
        let (_, body_bytes, durable_segment) =
            load_stage_segment_path_with_bytes(&body_path, segment.start_author, policy)?;
        persist_stage_segment_claim(data_dir, &durable_segment, &body_bytes)?;
        return Ok(durable_segment);
    }
    persist_immutable_bytes(&body_path, &bytes, "Nostr staging segment")?;
    persist_stage_segment_claim(data_dir, segment, &bytes)?;
    Ok(segment.clone())
}

fn load_stage_segment(
    data_dir: &Path,
    start_author: usize,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<StagedAuthorSegment> {
    let (_, _, segment) = load_stage_segment_with_bytes(data_dir, start_author, policy)?;
    Ok(segment)
}

fn load_stage_segment_with_bytes(
    data_dir: &Path,
    start_author: usize,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<(PathBuf, Vec<u8>, StagedAuthorSegment)> {
    if start_author >= policy.author_count {
        anyhow::bail!(
            "staged segment start {start_author} is outside {} policy authors",
            policy.author_count
        );
    }
    let claim = load_stage_segment_claim(data_dir, start_author)?;
    if claim.end_author > policy.author_count {
        anyhow::bail!(
            "staged segment claim {}..{} exceeds {} policy authors",
            claim.start_author,
            claim.end_author,
            policy.author_count
        );
    }
    let path = stage_segment_path(data_dir, start_author, claim.end_author);
    let (path, bytes, segment) = load_stage_segment_path_with_bytes(&path, start_author, policy)?;
    if stage_bytes_sha256(&bytes) != claim.body_sha256 {
        anyhow::bail!(
            "staged author segment body differs from immutable claim {}",
            stage_segment_claim_path(data_dir, start_author).display()
        );
    }
    Ok((path, bytes, segment))
}

fn load_stage_segment_path_with_bytes(
    path: &Path,
    start_author: usize,
    policy: &IndexedNostrCrawlPolicy,
) -> Result<(PathBuf, Vec<u8>, StagedAuthorSegment)> {
    let (file_start, file_end) = stage_segment_file_parts(path)?;
    if start_author >= policy.author_count || file_end > policy.author_count {
        anyhow::bail!(
            "staged segment boundary {start_author}..{file_end} exceeds {} policy authors",
            policy.author_count
        );
    }
    note_stage_segment_file_read();
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "read staged author segment {}..{} at {}",
            start_author,
            file_end,
            path.display()
        )
    })?;
    let segment: StagedAuthorSegment =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if segment.version != STAGE_FORMAT_VERSION
        || segment.start_author != start_author
        || segment.start_author != file_start
        || segment.end_author != file_end
        || segment.end_author <= segment.start_author
    {
        anyhow::bail!("invalid staged author segment {}", path.display());
    }
    for cid in &segment.event_cids {
        parse_root_text(cid)
            .with_context(|| format!("parse staged event CID in {}", path.display()))?;
    }
    Ok((path.to_path_buf(), bytes, segment))
}

fn validate_crawl_state(
    state: &IndexedNostrCrawlState,
    expected_policy: &IndexedNostrCrawlPolicy,
    author_count: usize,
) -> Result<()> {
    if state.version != CRAWL_STATE_VERSION {
        anyhow::bail!(
            "unsupported Nostr crawl state version {} (expected {})",
            state.version,
            CRAWL_STATE_VERSION
        );
    }
    let mut resumed_identity = state.policy.clone();
    resumed_identity.author_batch_size = expected_policy.author_batch_size;
    resumed_identity.checkpoint_authors = expected_policy.checkpoint_authors;
    if resumed_identity.require_all_relays && !expected_policy.require_all_relays {
        resumed_identity.require_all_relays = false;
    }
    if &resumed_identity != expected_policy {
        anyhow::bail!(
            "Nostr crawl policy or ordered author allowlist changed; refusing to reuse the durable cursor"
        );
    }
    if state.next_author > author_count {
        anyhow::bail!(
            "Nostr crawl cursor {} exceeds the validated author count {}",
            state.next_author,
            author_count
        );
    }
    if state.next_author == author_count && state.staged_segment_event_offset != 0 {
        anyhow::bail!(
            "completed Nostr crawl has a non-zero staged projection offset {}",
            state.staged_segment_event_offset
        );
    }
    Ok(())
}

fn persist_crawl_state(data_dir: &Path, state: &IndexedNostrCrawlState) -> Result<()> {
    persist_json_atomic(
        &data_dir.join(INDEX_DIR).join(CRAWL_STATE_FILE),
        state,
        "Nostr crawl state",
    )
}

fn persist_checkpoint(
    data_dir: &Path,
    report: &CrawlReport,
    options: &SocialGraphIndexOptions,
    relays: &[String],
) -> Result<()> {
    let output_dir = data_dir.join(INDEX_DIR);
    std::fs::create_dir_all(&output_dir)?;

    let checkpoint = IndexedNostrCheckpointReport {
        root: report.root.as_ref().map(cid_to_nhash).transpose()?,
        authors_considered: report.authors_considered,
        authors_processed: report.authors_processed,
        events_seen: report.events_seen,
        events_selected: report.events_selected,
        live_bytes_selected: report.live_bytes_selected,
        max_live_bytes: options.max_live_bytes,
        negentropy_only: options.negentropy_only,
        relays: relays.to_vec(),
    };
    let report_path = output_dir.join(CHECKPOINT_REPORT_FILE);
    std::fs::write(&report_path, serde_json::to_vec_pretty(&checkpoint)?)?;

    let root_path = output_dir.join(CHECKPOINT_ROOT_FILE);
    if let Some(root) = &checkpoint.root {
        std::fs::write(root_path, format!("{root}\n"))?;
    } else if root_path.exists() {
        std::fs::remove_file(root_path)?;
    }

    Ok(())
}

fn clear_checkpoint(data_dir: &Path) -> Result<()> {
    let output_dir = data_dir.join(INDEX_DIR);
    for path in [
        output_dir.join(CHECKPOINT_ROOT_FILE),
        output_dir.join(CHECKPOINT_REPORT_FILE),
    ] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn parse_root_text(value: &str) -> Result<Cid> {
    if value.starts_with("nhash1") {
        let decoded = nhash_decode(value).context("decode nhash root")?;
        return Ok(Cid {
            hash: decoded.hash,
            key: decoded.decrypt_key,
        });
    }

    Cid::parse(value).context("parse raw cid root")
}

fn cid_to_nhash(cid: &Cid) -> Result<String> {
    nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .context("encode nhash root")
}

fn print_report(report: &IndexedNostrReport, data_dir: &Path) {
    println!(
        "Indexed {} events from {}/{} authors (saw {} relay events, kept {} bytes)",
        report.events_selected,
        report.authors_processed,
        report.authors_considered,
        report.events_seen,
        report.live_bytes_selected
    );
    println!(
        "Graph warm: {}s depth {} ({})",
        report.warm_graph_seconds,
        report.graph_crawl_depth,
        if report.full_graph_recrawl {
            "full recrawl"
        } else {
            "incremental"
        }
    );
    println!(
        "Relay mode: {}",
        if report.negentropy_only {
            "author batches with negentropy-only relays".to_string()
        } else if report.full_author_history {
            format!(
                "full author history (page size {}, max pages {})",
                report.relay_page_size, report.max_relay_pages
            )
        } else if report.global_relay_scan {
            format!(
                "global recent scan (page size {}, max pages {})",
                report.relay_page_size, report.max_relay_pages
            )
        } else {
            "author batches with negentropy".to_string()
        }
    );
    if let Some(max_events_seen) = report.max_events_seen {
        println!("Raw relay event target: {}", max_events_seen);
    }
    if let Some(relay_event_max_bytes) = report.relay_event_max_bytes {
        println!("Relay event max size: {} bytes", relay_event_max_bytes);
    }

    if let Some(root) = &report.root {
        println!("Root: {}", root);
    } else {
        println!("Root: <empty>");
    }
    if let Some(profile_search_root) = &report.profile_search_root {
        println!("Profile search root: {}", profile_search_root);
    }

    println!(
        "Saved: {}",
        data_dir.join(INDEX_DIR).join(LATEST_REPORT_FILE).display()
    );

    if !report.top_hashtags.is_empty() {
        let preview = report
            .top_hashtags
            .iter()
            .take(5)
            .map(|entry| format!("{} ({})", entry.key, entry.count))
            .collect::<Vec<_>>()
            .join(", ");
        println!("Top hashtags: {}", preview);
    }
}

async fn resolve_index_relays(relays: Vec<String>, negentropy_only: bool) -> Result<Vec<String>> {
    if !negentropy_only {
        return Ok(relays);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client for NIP-11 relay checks")?;

    let mut supported = Vec::new();
    for relay in relays {
        match relay_supports_nip(&client, &relay, NEGENTROPY_NIP).await {
            Ok(true) => supported.push(relay),
            Ok(false) => {}
            Err(err) => {
                eprintln!("Skipping relay {relay}: {err}");
            }
        }
    }

    if supported.is_empty() {
        anyhow::bail!("no relays advertise NIP-77 negentropy support");
    }

    Ok(supported)
}

fn relay_info_url(relay: &str) -> Result<String> {
    if let Some(rest) = relay.strip_prefix("wss://") {
        return Ok(format!("https://{rest}"));
    }
    if let Some(rest) = relay.strip_prefix("ws://") {
        return Ok(format!("http://{rest}"));
    }
    anyhow::bail!("unsupported relay scheme: {relay}");
}

async fn relay_supports_nip(client: &reqwest::Client, relay: &str, nip: u16) -> Result<bool> {
    let url = relay_info_url(relay)?;
    let info = client
        .get(url)
        .header(ACCEPT, "application/nostr+json")
        .send()
        .await
        .with_context(|| format!("fetch NIP-11 document for {relay}"))?
        .error_for_status()
        .with_context(|| format!("NIP-11 request failed for {relay}"))?
        .json::<RelayInfoDocument>()
        .await
        .with_context(|| format!("decode NIP-11 document for {relay}"))?;
    Ok(info.supported_nips.contains(&nip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use futures::{SinkExt, StreamExt};
    use hashtree_nostr::NostrEventStore;
    use nostr::prelude::{EventBuilder, Kind, Tag, Timestamp};
    use nostr_sdk::Client;
    use serde_json::Value;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    macro_rules! event_builder {
        ($kind:expr, $content:expr $(,)?) => {
            EventBuilder::new($kind, $content)
        };
        ($kind:expr, $content:expr, $tags:expr $(,)?) => {
            EventBuilder::new($kind, $content).tags($tags)
        };
    }

    #[test]
    fn durable_profile_repair_rejects_unsafe_or_ambiguous_lmdb_sync_values() {
        let variable = "HTREE_LMDB_NO_SYNC";
        assert!(require_durable_profile_repair_lmdb_value(variable, None).is_ok());
        for value in ["0", "false", "FALSE"] {
            assert!(require_durable_profile_repair_lmdb_value(
                variable,
                Some(std::ffi::OsStr::new(value))
            )
            .is_ok());
        }
        for value in ["", "1", "true", "False", "yes"] {
            let error = require_durable_profile_repair_lmdb_value(
                variable,
                Some(std::ffi::OsStr::new(value)),
            )
            .expect_err("unsafe or ambiguous LMDB durability value must fail closed");
            assert!(error.to_string().contains(variable));
        }

        let external = "HTREE_LMDB_EXTERNAL_BLOB_SYNC";
        assert!(require_durable_external_blob_sync_value(external, None).is_ok());
        for value in ["1", "true", "TRUE", "yes", " YES "] {
            assert!(require_durable_external_blob_sync_value(
                external,
                Some(std::ffi::OsStr::new(value))
            )
            .is_ok());
        }
        for value in ["", "0", "false", "no", "maybe"] {
            let error = require_durable_external_blob_sync_value(
                external,
                Some(std::ffi::OsStr::new(value)),
            )
            .expect_err("disabled or ambiguous external-blob sync must fail closed");
            assert!(error.to_string().contains(external));
        }

        let audit_read_only = hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV;
        assert!(require_writable_profile_repair_pool_value(audit_read_only, None).is_ok());
        assert!(require_writable_profile_repair_pool_value(
            audit_read_only,
            Some(std::ffi::OsStr::new("0"))
        )
        .is_ok());
        for value in ["", "1", "false", "true", "yes", " 0 "] {
            let error = require_writable_profile_repair_pool_value(
                audit_read_only,
                Some(std::ffi::OsStr::new(value)),
            )
            .expect_err("read-only or ambiguous Pool mode must fail closed");
            assert!(error.to_string().contains(audit_read_only));
        }
    }

    #[test]
    fn durable_profile_repair_command_rejects_unsafe_env_before_mutation() {
        const CHILD_ENV: &str = "HTREE_TEST_UNSAFE_PROFILE_REPAIR_CHILD";
        const EXPECTED_VARIABLE_ENV: &str = "HTREE_TEST_UNSAFE_PROFILE_REPAIR_EXPECTED_VARIABLE";
        const TEST_NAME: &str =
            "app::nostr_index::tests::durable_profile_repair_command_rejects_unsafe_env_before_mutation";
        if let Some(base) = std::env::var_os(CHILD_ENV) {
            let base = PathBuf::from(base);
            let data_dir = base.join("projection-must-not-exist");
            let staging_data_dir = base.join("staging-must-not-exist");
            let out = base.join("receipt-must-not-exist.json");
            let error = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run_nostr_bulk_profile_repair(
                    data_dir.clone(),
                    BulkProfileRepairOptions {
                        staging_data_dir: staging_data_dir.clone(),
                        expected_state_sha256: "a".repeat(64),
                        expected_stage_state_sha256: "b".repeat(64),
                        expected_policy_sha256: "c".repeat(64),
                        expected_spool_data_sha256: "d".repeat(64),
                        event_blob_repair_receipt: base.join("event-repair-must-not-exist.json"),
                        expected_event_blob_repair_receipt_sha256: "0".repeat(64),
                        profile_rank_decisions_file: base.join("ranks-must-not-exist.jsonl"),
                        expected_profile_rank_decisions_file_sha256: "e".repeat(64),
                        profile_rank_decisions_report: base.join("rank-report-must-not-exist.json"),
                        expected_profile_rank_decisions_report_sha256: "f".repeat(64),
                        expected_replayed_author_count: 1,
                        expected_full_author_count: 1,
                        expected_profiles_by_pubkey_root_file_sha256: "1".repeat(64),
                        expected_profile_search_root_file_sha256: "2".repeat(64),
                        required_profile_pubkeys: vec!["3".repeat(64)],
                        btree_order: 64,
                        out: Some(out.clone()),
                    },
                ))
                .expect_err("unsafe LMDB environment must reject the production repair wrapper");
            let expected_variable = std::env::var(EXPECTED_VARIABLE_ENV).unwrap();
            assert!(format!("{error:#}").contains(&expected_variable));
            assert!(!data_dir.exists());
            assert!(!staging_data_dir.exists());
            assert!(!out.exists());
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        for (case, variable, value) in [
            ("no-sync", "HTREE_LMDB_NO_SYNC", "1"),
            ("no-meta-sync", "HTREE_LMDB_NO_META_SYNC", "true"),
            ("external-sync", "HTREE_LMDB_EXTERNAL_BLOB_SYNC", "0"),
            (
                "pool-read-only",
                hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV,
                "1",
            ),
            (
                "pool-read-only-invalid",
                hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV,
                "invalid",
            ),
        ] {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD_ENV, temp.path().join(case))
                .env(EXPECTED_VARIABLE_ENV, variable)
                .env_remove("HTREE_LMDB_NO_SYNC")
                .env_remove("HTREE_LMDB_NO_META_SYNC")
                .env_remove("HTREE_LMDB_EXTERNAL_BLOB_SYNC")
                .env_remove(hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV)
                .env(variable, value)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "unsafe-environment repair child failed for {variable}={value}"
            );
        }
    }

    fn checkpoint_test_options(
        allowlist_url: String,
        relays: Vec<String>,
    ) -> SocialGraphIndexOptions {
        SocialGraphIndexOptions {
            warm_graph_for: Duration::ZERO,
            graph_crawl_depth: 0,
            full_graph_recrawl: false,
            relays: Some(relays),
            author_allowlist_url: Some(allowlist_url),
            max_events_seen: None,
            max_authors: 16,
            max_authors_per_run: None,
            max_follow_distance: Some(0),
            max_live_bytes: 32 * 1024 * 1024,
            author_batch_size: 1,
            checkpoint_authors: 1,
            index_commit_batch_size: 8,
            stage_only: false,
            project_staged: false,
            bulk_project_staged: false,
            staging_data_dir: None,
            projection_authors: 8,
            projection_event_limit: 65_536,
            projection_follow: false,
            btree_order: 8,
            btree_update_concurrency: 4,
            concurrent_batches: 1,
            per_author_event_limit: 16,
            per_author_kind_event_limit: None,
            per_author_live_bytes: Some(1024 * 1024),
            fetch_timeout: Duration::from_millis(100),
            relay_event_max_bytes: None,
            global_relay_scan: false,
            full_author_history: true,
            negentropy_only: false,
            relay_page_size: 32,
            max_relay_pages: 1,
            kinds: Some(vec![1]),
        }
    }

    fn stage_test_policy(author_count: usize, segment_width: usize) -> IndexedNostrCrawlPolicy {
        let mut options = checkpoint_test_options(
            "http://127.0.0.1/authors".to_string(),
            vec!["wss://relay.example".to_string()],
        );
        options.max_authors = author_count;
        options.author_batch_size = segment_width;
        options.checkpoint_authors = segment_width;
        let authors = (0..author_count)
            .map(|index| format!("{index:064x}"))
            .collect::<Vec<_>>();
        build_crawl_policy(
            &options,
            &["wss://relay.example".to_string()],
            &authors,
            None,
        )
        .expect("build stage test policy")
    }

    async fn publish_test_events(relay: &str, events: &[nostr::Event]) {
        let publisher = Client::new(Keys::generate());
        publisher.add_relay(relay).await.expect("add relay");
        publisher.connect().await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        for event in events {
            publisher
                .send_event(event)
                .await
                .expect("publish test event");
        }
    }

    #[test]
    fn parse_nostr_filters_json_accepts_scope_tag_filter() {
        let filters =
            parse_nostr_filters_json(r##"{"kinds":[7368],"#i":["fips.peer"],"limit":10}"##)
                .expect("parse filter");

        assert_eq!(filters.len(), 1);
        let json = serde_json::to_value(&filters[0]).expect("serialize filter");
        assert_eq!(json["kinds"], serde_json::json!([7368]));
        assert_eq!(json["#i"], serde_json::json!(["fips.peer"]));
        assert_eq!(json["limit"], serde_json::json!(10));
    }

    #[test]
    fn parse_nostr_filters_json_accepts_req_envelope() {
        let filters = parse_nostr_filters_json(
            r##"["REQ","ratings",{"kinds":[7368],"#i":["fips.peer"]},{"kinds":[1]}]"##,
        )
        .expect("parse req envelope");

        assert_eq!(filters.len(), 2);
        let first = serde_json::to_value(&filters[0]).expect("serialize first filter");
        let second = serde_json::to_value(&filters[1]).expect("serialize second filter");
        assert_eq!(first["#i"], serde_json::json!(["fips.peer"]));
        assert_eq!(second["kinds"], serde_json::json!([1]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nostr_index_import_stores_queryable_rating_fact_events() {
        let temp_dir = TempDir::new().expect("tempdir");
        let rater = Keys::generate();
        let subject = Keys::generate().public_key().to_hex();
        let event = signed_rating_fact_event(&rater, &subject, "fips.peer", 80, 70);
        let events_path = temp_dir.path().join("ratings.json");
        std::fs::write(
            &events_path,
            serde_json::to_vec_pretty(&serde_json::json!({ "events": [event] }))
                .expect("encode rating event"),
        )
        .expect("write events file");

        let import_output = run_nostr_index_import(
            temp_dir.path().to_path_buf(),
            NostrIndexImportOptions {
                root: None,
                events_file: events_path,
                out: Some(temp_dir.path().join("import-report.json")),
            },
        )
        .await
        .expect("import rating event");

        assert_eq!(import_output.imported, 1);
        assert!(import_output.root.starts_with("nhash1"));

        let query_output = run_nostr_index_query(
            temp_dir.path().to_path_buf(),
            NostrIndexQueryOptions {
                root: None,
                filter_json: r##"{"kinds":[7368],"#i":["fips.peer"]}"##.to_string(),
                limit: 10,
                out: Some(temp_dir.path().join("query-report.json")),
            },
        )
        .await
        .expect("query imported rating event");

        assert_eq!(query_output.count, 1);
        assert_eq!(query_output.events[0].id, event.id.to_hex());
        assert!(stored_event_has_tag(
            &query_output.events[0],
            &["schema", "1"]
        ));
        assert!(stored_event_has_tag(
            &query_output.events[0],
            &["created_at", "70"]
        ));
        assert!(stored_event_has_tag(
            &query_output.events[0],
            &["rater", &rater.public_key().to_hex()]
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nostr_index_query_uses_daemon_mirror_root_by_default() {
        let temp_dir = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(temp_dir.path()).expect("open store"));
        let author = Keys::generate();
        let event = event_builder!(Kind::TextNote, "daemon mirrored note")
            .custom_created_at(Timestamp::from(42))
            .sign_with_keys(&author)
            .expect("sign mirrored note");
        let event_store = NostrEventStore::new(store.store_arc());
        let root = event_store
            .build(
                None,
                vec![hashtree_nostr::stored_event_from_nostr_sdk_event(&event)],
            )
            .await
            .expect("build mirrored root")
            .expect("mirrored root");
        let mirror_state_dir = temp_dir.path().join("nostr-mirror");
        std::fs::create_dir_all(&mirror_state_dir).expect("create mirror state dir");
        std::fs::write(
            mirror_state_dir.join("nostr-event-index.uploaded-root"),
            format!("{root}\n"),
        )
        .expect("write daemon mirror root");
        drop(event_store);
        drop(store);

        let output = run_nostr_index_query(
            temp_dir.path().to_path_buf(),
            NostrIndexQueryOptions {
                root: None,
                filter_json: r#"{"kinds":[1]}"#.to_string(),
                limit: 10,
                out: Some(temp_dir.path().join("daemon-query.json")),
            },
        )
        .await
        .expect("query daemon mirror root");

        assert_eq!(output.count, 1);
        assert_eq!(output.events[0].id, event.id.to_hex());
    }

    fn signed_rating_fact_event(
        keys: &Keys,
        subject: &str,
        scope: &str,
        rating: i64,
        created_at: u64,
    ) -> nostr::Event {
        let scope_index = scope.to_lowercase();
        let rating = rating.to_string();
        let created_at_tag = created_at.to_string();
        let rater = keys.public_key().to_hex();
        EventBuilder::new(Kind::from(7368_u16), "")
            .tags(vec![
                Tag::parse(["i", scope_index.as_str()]).expect("scope index tag"),
                Tag::parse(["i", subject]).expect("subject index tag"),
                Tag::parse(["type", "rating"]).expect("type fact tag"),
                Tag::parse(["schema", "1"]).expect("schema fact tag"),
                Tag::parse(["created_at", created_at_tag.as_str()]).expect("created at fact tag"),
                Tag::parse(["rater", rater.as_str()]).expect("rater fact tag"),
                Tag::parse(["subject", subject]).expect("subject fact tag"),
                Tag::parse(["scope", scope]).expect("scope fact tag"),
                Tag::parse(["rating", rating.as_str()]).expect("rating fact tag"),
                Tag::parse(["min_rating", "0"]).expect("min rating fact tag"),
                Tag::parse(["max_rating", "100"]).expect("max rating fact tag"),
            ])
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("sign rating fact event")
    }

    fn stored_event_has_tag(event: &StoredNostrEvent, parts: &[&str]) -> bool {
        let expected = parts
            .iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>();
        event.tags.iter().any(|tag| tag == &expected)
    }

    struct TestRelay {
        port: u16,
        shutdown: broadcast::Sender<()>,
        requested_authors: Arc<Mutex<Vec<String>>>,
    }

    impl TestRelay {
        fn new() -> Self {
            Self::bind("127.0.0.1:0")
        }

        fn bind(address: &str) -> Self {
            let events = Arc::new(Mutex::new(Vec::new()));
            let requested_authors = Arc::new(Mutex::new(Vec::new()));
            let (shutdown, _) = broadcast::channel(1);

            let std_listener = TcpListener::bind(address).expect("bind relay listener");
            let port = std_listener.local_addr().expect("relay local addr").port();
            std_listener.set_nonblocking(true).expect("set nonblocking");

            let events_for_thread = Arc::clone(&events);
            let requested_authors_for_thread = Arc::clone(&requested_authors);
            let shutdown_for_thread = shutdown.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build tokio runtime");

                rt.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut shutdown_rx = shutdown_for_thread.subscribe();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            accept = listener.accept() => {
                                if let Ok((stream, _)) = accept {
                                    let events = Arc::clone(&events_for_thread);
                                    let requested_authors = Arc::clone(&requested_authors_for_thread);
                                    tokio::spawn(async move {
                                        handle_connection(stream, events, requested_authors).await;
                                    });
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(Duration::from_millis(100));

            Self {
                port,
                shutdown,
                requested_authors,
            }
        }

        fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        fn requested_authors(&self) -> Vec<String> {
            self.requested_authors
                .lock()
                .expect("requested authors lock")
                .clone()
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    struct TestNip11Server {
        port: u16,
        shutdown: broadcast::Sender<()>,
    }

    impl TestNip11Server {
        fn new(status_line: &'static str, body: String) -> Self {
            let (shutdown, _) = broadcast::channel(1);

            let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind nip11 listener");
            let port = std_listener.local_addr().expect("nip11 local addr").port();
            std_listener
                .set_nonblocking(true)
                .expect("set nip11 nonblocking");

            let shutdown_for_thread = shutdown.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build tokio runtime");

                rt.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut shutdown_rx = shutdown_for_thread.subscribe();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            accept = listener.accept() => {
                                if let Ok((mut stream, _)) = accept {
                                    let body = body.clone();
                                    tokio::spawn(async move {
                                        let mut buf = [0u8; 1024];
                                        let _ = stream.read(&mut buf).await;
                                        let response = format!(
                                            "HTTP/1.1 {status_line}\r\ncontent-type: application/nostr+json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                            body.len()
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                        let _ = stream.shutdown().await;
                                    });
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(Duration::from_millis(50));
            Self { port, shutdown }
        }

        fn relay_url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for TestNip11Server {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    struct TestTextServer {
        port: u16,
        shutdown: broadcast::Sender<()>,
    }

    impl TestTextServer {
        fn new(body: String) -> Self {
            let (shutdown, _) = broadcast::channel(1);

            let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind text listener");
            let port = std_listener.local_addr().expect("text local addr").port();
            std_listener
                .set_nonblocking(true)
                .expect("set text server nonblocking");

            let shutdown_for_thread = shutdown.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build tokio runtime");

                rt.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut shutdown_rx = shutdown_for_thread.subscribe();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            accept = listener.accept() => {
                                if let Ok((mut stream, _)) = accept {
                                    let body = body.clone();
                                    tokio::spawn(async move {
                                        let mut buf = [0u8; 1024];
                                        let _ = stream.read(&mut buf).await;
                                        let response = format!(
                                            "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                            body.len()
                                        );
                                        let _ = stream.write_all(response.as_bytes()).await;
                                        let _ = stream.shutdown().await;
                                    });
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(Duration::from_millis(50));
            Self { port, shutdown }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for TestTextServer {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn event_matches_filter(event: &Value, filter: &Value) -> bool {
        let Some(filter_obj) = filter.as_object() else {
            return true;
        };

        if let Some(authors) = filter_obj.get("authors").and_then(Value::as_array) {
            let accepted: Vec<&str> = authors.iter().filter_map(Value::as_str).collect();
            let author = event
                .get("pubkey")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !accepted.is_empty() && !accepted.contains(&author) {
                return false;
            }
        }

        if let Some(kinds) = filter_obj.get("kinds").and_then(Value::as_array) {
            let event_kind = event
                .get("kind")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            if !kinds
                .iter()
                .any(|kind| kind.as_i64().is_some_and(|value| value == event_kind))
            {
                return false;
            }
        }

        true
    }

    async fn handle_connection(
        stream: TcpStream,
        events: Arc<Mutex<Vec<Value>>>,
        requested_authors: Arc<Mutex<Vec<String>>>,
    ) {
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };

        let (mut write, mut read) = ws_stream.split();

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(text)) => text,
                Ok(Message::Ping(data)) => {
                    let _ = write.send(Message::Pong(data)).await;
                    continue;
                }
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

            let parsed: Vec<Value> = match serde_json::from_str(&msg) {
                Ok(value) => value,
                Err(_) => continue,
            };

            match parsed.first().and_then(Value::as_str) {
                Some("EVENT") => {
                    let Some(event) = parsed.get(1).cloned() else {
                        continue;
                    };
                    let Some(id) = event.get("id").and_then(Value::as_str).map(str::to_owned)
                    else {
                        continue;
                    };
                    events.lock().expect("relay events lock").push(event);
                    let ok = serde_json::json!(["OK", id, true, ""]);
                    let _ = write.send(Message::Text(ok.to_string())).await;
                }
                Some("REQ") => {
                    let Some(sub_id) = parsed.get(1).and_then(Value::as_str) else {
                        continue;
                    };
                    let filters: Vec<Value> = parsed.iter().skip(2).cloned().collect();
                    for author in filters
                        .iter()
                        .filter_map(|filter| filter.get("authors"))
                        .filter_map(Value::as_array)
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        requested_authors
                            .lock()
                            .expect("requested authors lock")
                            .push(author.to_string());
                    }
                    let snapshot = events.lock().expect("relay events lock").clone();
                    for event in snapshot {
                        let matched = if filters.is_empty() {
                            true
                        } else {
                            filters
                                .iter()
                                .any(|filter| event_matches_filter(&event, filter))
                        };
                        if matched {
                            let msg = serde_json::json!(["EVENT", sub_id, event]);
                            let _ = write.send(Message::Text(msg.to_string())).await;
                        }
                    }
                    let eose = serde_json::json!(["EOSE", sub_id]);
                    let _ = write.send(Message::Text(eose.to_string())).await;
                }
                _ => {}
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warms_social_graph_and_persists_index_report() -> io::Result<()> {
        let relay = TestRelay::new();
        let relay_url = relay.url();

        let tmp = TempDir::new().expect("tempdir");
        let root_keys = Keys::generate();
        let alice_keys = Keys::generate();

        let contact_list = event_builder!(
            Kind::ContactList,
            "",
            [
                Tag::parse(vec!["p".to_string(), alice_keys.public_key().to_hex(),])
                    .expect("p tag")
            ],
        )
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");

        let alice_note = event_builder!(
            Kind::TextNote,
            "alice nostr note",
            [Tag::parse(["t", "nostr"]).expect("t tag")],
        )
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("alice note");

        let alice_profile = event_builder!(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "Alice Relay",
                "nip05": "alice@example.com",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(30))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");

        let publisher = Client::new(Keys::generate());
        publisher.add_relay(&relay_url).await.expect("add relay");
        publisher.connect().await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        for event in [&contact_list, &alice_note, &alice_profile] {
            publisher
                .send_event(event)
                .await
                .expect("publish test event");
        }

        let mut config = Config::default();
        config.nostr.relays = vec![relay_url];
        config.nostr.social_graph_crawl_depth = 1;
        config.storage.max_size_gb = 1;

        let report = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &config,
            root_keys.clone(),
            SocialGraphIndexOptions {
                warm_graph_for: Duration::from_secs(1),
                graph_crawl_depth: 1,
                full_graph_recrawl: false,
                relays: None,
                author_allowlist_url: None,
                max_events_seen: None,
                max_authors: 8,
                max_authors_per_run: None,
                max_follow_distance: Some(1),
                max_live_bytes: 8 * 1024 * 1024,
                author_batch_size: 32,
                checkpoint_authors: 8,
                index_commit_batch_size: 32,
                stage_only: false,
                project_staged: false,
                bulk_project_staged: false,
                staging_data_dir: None,
                projection_authors: 8,
                projection_event_limit: 65_536,
                projection_follow: false,
                btree_order: 16,
                btree_update_concurrency: 4,
                concurrent_batches: 4,
                per_author_event_limit: 8,
                per_author_kind_event_limit: None,
                per_author_live_bytes: None,
                fetch_timeout: Duration::from_secs(5),
                relay_event_max_bytes: None,
                global_relay_scan: false,
                full_author_history: false,
                negentropy_only: false,
                relay_page_size: 1_000,
                max_relay_pages: 10,
                kinds: None,
            },
        )
        .await
        .expect("run index");

        assert_eq!(report.authors_considered, 2);
        assert_eq!(report.authors_processed, 2);
        assert!(report.events_selected >= 3);
        assert!(report.profile_search_root.is_some());
        assert_eq!(
            report.top_hashtags.first(),
            Some(&RankedCount {
                key: "nostr".to_string(),
                count: 1
            })
        );

        let report_path = tmp.path().join(INDEX_DIR).join(LATEST_REPORT_FILE);
        let root_path = tmp.path().join(INDEX_DIR).join(LATEST_ROOT_FILE);
        assert!(report_path.exists());
        assert!(root_path.exists());

        let saved_report: IndexedNostrReport =
            serde_json::from_slice(&std::fs::read(&report_path).expect("read report"))
                .expect("parse report");
        assert_eq!(saved_report.root, report.root);
        assert_eq!(saved_report.profile_search_root, report.profile_search_root);

        let store = HashtreeStore::with_options(tmp.path(), None, 1024 * 1024 * 1024)
            .expect("reopen store");
        let event_store = NostrEventStore::new(store.store_arc());
        let root =
            parse_root_text(report.root.as_deref().expect("root string")).expect("parse cid");
        let hashtagged = event_store
            .list_by_tag(
                Some(&root),
                "t",
                "nostr",
                ListEventsOptions {
                    limit: Some(10),
                    ..Default::default()
                },
            )
            .await
            .expect("query hashtag");

        assert_eq!(hashtagged.len(), 1);
        assert_eq!(hashtagged[0].id, alice_note.id.to_hex());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_profile_index_can_use_external_author_allowlist() -> io::Result<()> {
        let relay = TestRelay::new();
        let relay_url = relay.url();

        let tmp = TempDir::new().expect("tempdir");
        let root_keys = Keys::generate();
        let alice_keys = Keys::generate();
        let alice_pubkey = alice_keys.public_key().to_hex();
        let allowlist = TestTextServer::new(format!("{alice_pubkey}\nnot-a-pubkey\n"));

        let alice_profile = event_builder!(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "Alice Allowlist",
                "name": "alice",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(30))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");

        let publisher = Client::new(Keys::generate());
        publisher.add_relay(&relay_url).await.expect("add relay");
        publisher.connect().await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        publisher
            .send_event(&alice_profile)
            .await
            .expect("publish test event");

        let mut config = Config::default();
        config.nostr.relays = vec![relay_url];
        config.nostr.social_graph_crawl_depth = 1;
        config.storage.max_size_gb = 1;

        let db_max_size_bytes = config.nostr.db_max_size_gb * 1024 * 1024 * 1024;

        let report = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &config,
            root_keys,
            SocialGraphIndexOptions {
                warm_graph_for: Duration::from_secs(0),
                graph_crawl_depth: 1,
                full_graph_recrawl: false,
                relays: None,
                author_allowlist_url: Some(format!("{}/allowlist", allowlist.url())),
                max_events_seen: None,
                max_authors: 8,
                max_authors_per_run: None,
                max_follow_distance: Some(0),
                max_live_bytes: 8 * 1024 * 1024,
                author_batch_size: 32,
                checkpoint_authors: 8,
                index_commit_batch_size: 32,
                stage_only: false,
                project_staged: false,
                bulk_project_staged: false,
                staging_data_dir: None,
                projection_authors: 8,
                projection_event_limit: 65_536,
                projection_follow: false,
                btree_order: 16,
                btree_update_concurrency: 4,
                concurrent_batches: 1,
                per_author_event_limit: 8,
                per_author_kind_event_limit: None,
                per_author_live_bytes: None,
                fetch_timeout: Duration::from_secs(5),
                relay_event_max_bytes: None,
                global_relay_scan: true,
                full_author_history: false,
                negentropy_only: false,
                relay_page_size: 128,
                max_relay_pages: 1,
                kinds: Some(vec![0]),
            },
        )
        .await
        .expect("run index");

        assert_eq!(report.authors_considered, 1);
        assert_eq!(report.authors_processed, 1);
        assert_eq!(report.events_selected, 1);
        assert!(report.profile_search_root.is_some());

        let store = HashtreeStore::with_options(tmp.path(), None, 1024 * 1024 * 1024)
            .expect("reopen store");
        let graph_store = socialgraph::open_social_graph_store_with_storage(
            tmp.path(),
            store.store_arc(),
            Some(db_max_size_bytes),
        )
        .expect("reopen graph store");
        let results = graph_store
            .profile_search_entries_for_prefix("p:alice")
            .expect("query profile search root");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, format!("p:alice:{alice_pubkey}"));
        assert_eq!(results[0].1.name, "Alice Allowlist");

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpointed_allowlist_recycles_after_a_durable_process_tranche() -> io::Result<()> {
        let relay = TestRelay::new();
        let alice = Keys::generate();
        let bob = Keys::generate();
        let events = vec![
            event_builder!(Kind::TextNote, "alice tranche")
                .custom_created_at(Timestamp::from_secs(30))
                .sign_with_keys(&alice)
                .expect("alice note"),
            event_builder!(Kind::TextNote, "bob tranche")
                .custom_created_at(Timestamp::from_secs(31))
                .sign_with_keys(&bob)
                .expect("bob note"),
        ];
        publish_test_events(&relay.url(), &events).await;

        let allowlist = TestTextServer::new(format!(
            "{}\n{}\n",
            alice.public_key().to_hex(),
            bob.public_key().to_hex()
        ));
        let mut options =
            checkpoint_test_options(format!("{}/authors", allowlist.url()), vec![relay.url()]);
        options.max_authors_per_run = Some(1);
        let tmp = TempDir::new().expect("tempdir");
        let mut config = Config::default();
        config.storage.max_size_gb = 1;
        config.nostr.db_max_size_gb = 1;

        let first = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &config,
            Keys::generate(),
            options.clone(),
        )
        .await
        .expect("first bounded process tranche");
        assert_eq!(first.authors_considered, 2);
        assert_eq!(first.authors_processed, 1);
        assert_eq!(
            load_crawl_state(tmp.path())
                .expect("load first tranche state")
                .expect("first tranche state")
                .next_author,
            1
        );
        assert!(!tmp.path().join(INDEX_DIR).join(LATEST_REPORT_FILE).exists());

        let second =
            run_socialgraph_index(tmp.path().to_path_buf(), &config, Keys::generate(), options)
                .await
                .expect("resumed bounded process tranche");
        assert_eq!(second.authors_considered, 2);
        assert_eq!(second.authors_processed, 2);
        assert_eq!(second.events_selected, 2);
        assert!(tmp.path().join(INDEX_DIR).join(LATEST_REPORT_FILE).exists());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpointed_allowlist_advances_with_one_healthy_relay_and_resumes_without_refetch(
    ) -> io::Result<()> {
        let primary = TestRelay::new();
        let unavailable = TcpListener::bind("127.0.0.1:0").expect("reserve relay port");
        let secondary_port = unavailable
            .local_addr()
            .expect("reserved relay addr")
            .port();
        drop(unavailable);
        let secondary_url = format!("ws://127.0.0.1:{secondary_port}");

        let alice = Keys::generate();
        let bob = Keys::generate();
        let events = vec![
            event_builder!(
                Kind::Metadata,
                serde_json::json!({ "name": "alice-checkpoint" }).to_string()
            )
            .custom_created_at(Timestamp::from_secs(29))
            .sign_with_keys(&alice)
            .expect("alice profile"),
            event_builder!(Kind::TextNote, "alice checkpoint")
                .custom_created_at(Timestamp::from_secs(30))
                .sign_with_keys(&alice)
                .expect("alice note"),
            event_builder!(Kind::TextNote, "bob checkpoint")
                .custom_created_at(Timestamp::from_secs(31))
                .sign_with_keys(&bob)
                .expect("bob note"),
        ];
        publish_test_events(&primary.url(), &events).await;

        let allowlist = TestTextServer::new(format!(
            "{}\n{}\n",
            alice.public_key().to_hex(),
            bob.public_key().to_hex()
        ));
        let mut options = checkpoint_test_options(
            format!("{}/authors", allowlist.url()),
            vec![primary.url(), secondary_url.clone()],
        );
        options.kinds = Some(vec![0, 1]);
        let tmp = TempDir::new().expect("tempdir");
        let mut config = Config::default();
        config.storage.max_size_gb = 1;
        config.nostr.db_max_size_gb = 1;

        let report = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &config,
            Keys::generate(),
            options.clone(),
        )
        .await
        .expect("unavailable optional relay must not stop the crawl");
        assert_eq!(report.authors_processed, 2);
        assert_eq!(report.events_selected, 3);
        assert!(report.profile_search_root.is_some());
        assert!(report.top_authors.is_empty());
        assert!(report.recent_events.is_empty());

        let completed_state = load_crawl_state(tmp.path())
            .expect("load completed state")
            .expect("completed state retained");
        assert_eq!(completed_state.next_author, 2);
        assert_eq!(completed_state.root, report.root);
        assert_eq!(completed_state.policy.author_count, 2);
        assert!(!completed_state.policy.require_all_relays);

        let requests_before_resume = primary.requested_authors().len();
        let resumed = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &config,
            Keys::generate(),
            options.clone(),
        )
        .await
        .expect("resume completed crawl");
        assert_eq!(resumed.root, report.root);
        assert_eq!(primary.requested_authors().len(), requests_before_resume);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_phase_crawl_bulk_projects_staged_blobs_without_refetching() -> io::Result<()> {
        let relay = TestRelay::new();
        let author = Keys::generate();
        let first_note = event_builder!(Kind::TextNote, "first staged note")
            .custom_created_at(Timestamp::from_secs(42))
            .sign_with_keys(&author)
            .expect("signed note");
        let second_note = event_builder!(Kind::TextNote, "second staged note")
            .custom_created_at(Timestamp::from_secs(43))
            .sign_with_keys(&author)
            .expect("signed note");
        publish_test_events(&relay.url(), &[first_note, second_note]).await;
        let allowlist = TestTextServer::new(format!("{}\n", author.public_key().to_hex()));
        let mut options =
            checkpoint_test_options(format!("{}/authors", allowlist.url()), vec![relay.url()]);
        options.stage_only = true;
        let tmp = TempDir::new().expect("tempdir");
        let staging_data_dir = tmp.path().join("staging-data");
        options.staging_data_dir = Some(staging_data_dir.clone());
        let mut config = Config::default();
        config.storage.max_size_gb = 1;
        config.nostr.db_max_size_gb = 1;

        let staged = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &config,
            Keys::generate(),
            options.clone(),
        )
        .await
        .expect("stage relay events");
        assert_eq!(staged.authors_processed, 1);
        assert_eq!(staged.events_selected, 2);
        assert!(staged.root.is_none());
        assert!(load_crawl_state(tmp.path()).unwrap().is_none());
        let stage_state = load_stage_state(&staging_data_dir).unwrap().unwrap();
        assert_eq!(stage_state.next_author, 1);
        let segment =
            load_stage_segment(&staging_data_dir, 0, &stage_state.policy).expect("staged segment");
        assert_eq!(segment.event_cids.len(), 2);

        let relay_requests_after_stage = relay.requested_authors().len();
        options.stage_only = false;
        options.project_staged = true;
        options.bulk_project_staged = true;
        options.projection_follow = true;
        options.projection_authors = 16;
        options.index_commit_batch_size = 1;
        let projected =
            run_socialgraph_index(tmp.path().to_path_buf(), &config, Keys::generate(), options)
                .await
                .expect("project staged events");

        assert_eq!(projected.authors_processed, 1);
        assert_eq!(projected.events_selected, 2);
        assert!(projected.root.is_some());
        assert_eq!(relay.requested_authors().len(), relay_requests_after_stage);
        let projected_state = load_crawl_state(tmp.path()).unwrap().unwrap();
        assert_eq!(projected_state.next_author, 1);
        assert_eq!(projected_state.staged_segment_event_offset, 0);
        assert_eq!(projected_state.root, projected.root);
        Ok(())
    }

    #[test]
    fn partial_staged_projection_checkpoint_resumes_inside_segment() {
        let authors = vec!["a".repeat(64), "b".repeat(64)];
        let options = checkpoint_test_options(
            "http://127.0.0.1:10001/authors".to_string(),
            vec!["wss://relay.example".to_string()],
        );
        let policy = build_crawl_policy(
            &options,
            &["wss://relay.example".to_string()],
            &authors,
            None,
        )
        .expect("build policy");
        let mut state = IndexedNostrCrawlState {
            version: CRAWL_STATE_VERSION,
            author_allowlist_source: options.author_allowlist_url.clone(),
            policy,
            next_author: 0,
            staged_segment_event_offset: 0,
            root: None,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
        };
        let segment = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 0,
            end_author: 2,
            events_seen: 5,
            events_selected: 3,
            live_bytes_selected: 123,
            event_cids: vec!["one".into(), "two".into(), "three".into()],
        };

        assert!(!apply_projected_segment_checkpoint(
            &mut state,
            &segment,
            1,
            Some("partial-root".into())
        )
        .expect("partial checkpoint"));
        assert_eq!(state.next_author, 0);
        assert_eq!(state.staged_segment_event_offset, 1);
        assert_eq!(state.events_selected, 0);

        let resumed: IndexedNostrCrawlState =
            serde_json::from_slice(&serde_json::to_vec(&state).expect("serialize state"))
                .expect("resume state");
        state = resumed;
        assert!(apply_projected_segment_checkpoint(
            &mut state,
            &segment,
            3,
            Some("complete-root".into())
        )
        .expect("complete checkpoint"));
        assert_eq!(state.next_author, 2);
        assert_eq!(state.staged_segment_event_offset, 0);
        assert_eq!(state.events_seen, 5);
        assert_eq!(state.events_selected, 3);
        assert_eq!(state.live_bytes_selected, 123);
    }

    #[test]
    fn legacy_projection_state_defaults_partial_offset_to_zero() {
        let authors = vec!["a".repeat(64)];
        let options = checkpoint_test_options(
            "http://127.0.0.1:10001/authors".to_string(),
            vec!["wss://relay.example".to_string()],
        );
        let policy = build_crawl_policy(
            &options,
            &["wss://relay.example".to_string()],
            &authors,
            None,
        )
        .expect("build policy");
        let legacy = serde_json::json!({
            "version": CRAWL_STATE_VERSION,
            "author_allowlist_source": options.author_allowlist_url,
            "policy": policy,
            "next_author": 0,
            "root": null,
            "events_seen": 0,
            "events_selected": 0,
            "live_bytes_selected": 0
        });
        let state: IndexedNostrCrawlState =
            serde_json::from_value(legacy).expect("load legacy state");
        assert_eq!(state.staged_segment_event_offset, 0);
    }

    #[test]
    fn crawl_state_policy_uses_ordered_content_not_ephemeral_source_url() {
        let tmp = TempDir::new().expect("tempdir");
        let authors = vec!["a".repeat(64), "b".repeat(64)];
        let options = checkpoint_test_options(
            "http://127.0.0.1:10001/authors".to_string(),
            vec!["wss://relay.example".to_string()],
        );
        let policy = build_crawl_policy(
            &options,
            &["wss://relay.example".to_string()],
            &authors,
            None,
        )
        .expect("build policy");
        let state = IndexedNostrCrawlState {
            version: CRAWL_STATE_VERSION,
            author_allowlist_source: options.author_allowlist_url.clone(),
            policy: policy.clone(),
            next_author: 0,
            staged_segment_event_offset: 0,
            root: None,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
        };
        persist_crawl_state(tmp.path(), &state).expect("persist state");
        let loaded = load_crawl_state(tmp.path())
            .expect("load state")
            .expect("state exists");
        assert_eq!(loaded, state);
        assert!(!tmp
            .path()
            .join(INDEX_DIR)
            .join(format!(".{CRAWL_STATE_FILE}.tmp"))
            .exists());

        let mut changed_source = options.clone();
        changed_source.author_allowlist_url = Some("http://127.0.0.1:20002/authors".to_string());
        let same_policy = build_crawl_policy(
            &changed_source,
            &["wss://relay.example".to_string()],
            &authors,
            None,
        )
        .expect("build same policy");
        validate_crawl_state(&loaded, &same_policy, authors.len())
            .expect("source URL is diagnostic only");

        let mut changed_cadence = changed_source.clone();
        changed_cadence.author_batch_size = 64;
        changed_cadence.checkpoint_authors = 64;
        let same_content_policy = build_crawl_policy(
            &changed_cadence,
            &["wss://relay.example".to_string()],
            &authors,
            None,
        )
        .expect("build policy with new cadence");
        validate_crawl_state(&loaded, &same_content_policy, authors.len())
            .expect("execution cadence must not invalidate durable content progress");

        assert!(!same_content_policy.require_all_relays);
        let mut legacy_required_state = loaded.clone();
        legacy_required_state.policy.require_all_relays = true;
        validate_crawl_state(&legacy_required_state, &same_content_policy, authors.len())
            .expect("one-way relaxation from required to optional relays must preserve progress");

        let reordered = vec![authors[1].clone(), authors[0].clone()];
        let changed_policy = build_crawl_policy(
            &changed_source,
            &["wss://relay.example".to_string()],
            &reordered,
            None,
        )
        .expect("build changed policy");
        assert!(validate_crawl_state(&loaded, &changed_policy, authors.len()).is_err());
    }

    #[test]
    fn staged_segments_and_fetch_watermark_are_durable_and_independent() {
        let tmp = TempDir::new().expect("tempdir");
        let authors = vec!["a".repeat(64), "b".repeat(64)];
        let options = checkpoint_test_options(
            "http://127.0.0.1:10001/authors".to_string(),
            vec!["wss://relay.example".to_string()],
        );
        let policy = build_crawl_policy(
            &options,
            &["wss://relay.example".to_string()],
            &authors,
            None,
        )
        .expect("build policy");
        let state = StagedNostrCrawlState {
            version: STAGE_FORMAT_VERSION,
            author_allowlist_source: options.author_allowlist_url.clone(),
            policy: policy.clone(),
            next_author: 1,
            events_seen: 4,
            events_selected: 2,
            live_bytes_selected: 512,
        };
        let segment = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 0,
            end_author: 1,
            events_seen: 4,
            events_selected: 2,
            live_bytes_selected: 512,
            event_cids: vec![cid_to_nhash(&Cid {
                hash: [0; 32],
                key: None,
            })
            .expect("encode test event CID")],
        };

        persist_stage_segment(tmp.path(), &segment, &policy).expect("persist segment");
        persist_stage_state(tmp.path(), &state).expect("persist stage state");

        assert_eq!(load_stage_state(tmp.path()).unwrap(), Some(state));
        assert_eq!(load_stage_segment(tmp.path(), 0, &policy).unwrap(), segment);
        validate_stage_state(
            &load_stage_state(tmp.path()).unwrap().unwrap(),
            &policy,
            authors.len(),
        )
        .expect("validate stage state");
        assert!(!tmp.path().join(INDEX_DIR).join(CRAWL_STATE_FILE).exists());
    }

    #[test]
    fn staging_state_allows_new_cadence_because_segment_claims_pin_old_boundaries() {
        let policy = stage_test_policy(8, 4);
        let state = StagedNostrCrawlState {
            version: STAGE_FORMAT_VERSION,
            author_allowlist_source: Some("http://127.0.0.1/authors".to_string()),
            policy: policy.clone(),
            next_author: 4,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
        };

        let mut changed_batch_width = policy.clone();
        changed_batch_width.author_batch_size = 2;
        validate_stage_state(&state, &changed_batch_width, 8)
            .expect("per-start claims make a new author batch width safe");

        let mut changed_checkpoint_width = policy;
        changed_checkpoint_width.checkpoint_authors = 2;
        validate_stage_state(&state, &changed_checkpoint_width, 8)
            .expect("per-start claims make a new checkpoint width safe");
    }

    #[test]
    fn staged_segment_publish_is_immutable_and_retryable() {
        let tmp = TempDir::new().expect("tempdir");
        let policy = stage_test_policy(8, 4);
        let segment = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 0,
            end_author: 4,
            events_seen: 2,
            events_selected: 1,
            live_bytes_selected: 128,
            event_cids: vec![cid_to_nhash(&Cid {
                hash: [1; 32],
                key: None,
            })
            .expect("encode test event CID")],
        };
        persist_stage_segment(tmp.path(), &segment, &policy)
            .expect("first immutable segment publish");
        let path = stage_segment_path(tmp.path(), 0, 4);
        let first_bytes = std::fs::read(&path).expect("read first segment bytes");

        persist_stage_segment(tmp.path(), &segment, &policy).expect("exact segment retry");
        assert_eq!(
            std::fs::read(&path).expect("read retried segment bytes"),
            first_bytes
        );

        let mut conflicting = segment.clone();
        conflicting.events_seen = 3;
        let adopted = persist_stage_segment(tmp.path(), &conflicting, &policy)
            .expect("durable claimed bytes win over changed relay results");
        assert_eq!(adopted, segment);

        let mut different_boundary = segment.clone();
        different_boundary.end_author = 5;
        let adopted = persist_stage_segment(tmp.path(), &different_boundary, &policy)
            .expect("durable claimed boundary wins over a later cadence");
        assert_eq!(adopted, segment);
    }

    #[test]
    fn staged_segment_publish_adopts_exact_fsynced_orphan() {
        let tmp = TempDir::new().expect("tempdir");
        let policy = stage_test_policy(9, 2);
        let segment = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 7,
            end_author: 9,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
            event_cids: Vec::new(),
        };
        let path = stage_segment_path(tmp.path(), 7, 9);
        let parent = path.parent().expect("segment parent");
        std::fs::create_dir_all(parent).expect("create segment parent");
        let mut bytes = serde_json::to_vec(&segment).expect("encode segment");
        bytes.push(b'\n');
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("segment file name");
        let pending = parent.join(format!(".{file_name}{IMMUTABLE_PENDING_SUFFIX}"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)
            .expect("create pending orphan");
        file.write_all(&bytes).expect("write pending orphan");
        file.sync_all().expect("fsync pending orphan");
        drop(file);

        persist_stage_segment(tmp.path(), &segment, &policy).expect("adopt exact pending orphan");

        assert_eq!(std::fs::read(&path).expect("read adopted segment"), bytes);
        assert!(!pending.exists());
        assert_eq!(load_stage_segment(tmp.path(), 7, &policy).unwrap(), segment);
    }

    #[test]
    fn staged_segment_publish_repairs_partial_pending_crash_orphan() {
        let tmp = TempDir::new().expect("tempdir");
        let policy = stage_test_policy(2, 2);
        let segment = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 0,
            end_author: 2,
            events_seen: 1,
            events_selected: 0,
            live_bytes_selected: 0,
            event_cids: Vec::new(),
        };
        let path = stage_segment_path(tmp.path(), 0, 2);
        let parent = path.parent().expect("segment parent");
        std::fs::create_dir_all(parent).expect("create segment parent");
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("segment file name");
        let pending = parent.join(format!(".{file_name}{IMMUTABLE_PENDING_SUFFIX}"));
        std::fs::write(&pending, b"{\"version\":1")
            .expect("simulate a truncated unpublished pending file");

        persist_stage_segment(tmp.path(), &segment, &policy)
            .expect("repair partial orphan and commit exact immutable segment");

        let mut expected = serde_json::to_vec(&segment).expect("encode expected segment");
        expected.push(b'\n');
        assert_eq!(
            std::fs::read(&path).expect("read committed segment"),
            expected
        );
        assert!(!pending.exists());
    }

    #[test]
    fn restart_claims_old_cadence_body_and_advances_from_durable_counters() {
        let tmp = TempDir::new().expect("tempdir");
        let old_policy = stage_test_policy(4, 1);
        let durable = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 0,
            end_author: 1,
            events_seen: 7,
            events_selected: 3,
            live_bytes_selected: 512,
            event_cids: Vec::new(),
        };
        let body_path = stage_segment_path(tmp.path(), 0, 1);
        let mut body_bytes = serde_json::to_vec(&durable).expect("encode durable segment");
        body_bytes.push(b'\n');
        persist_immutable_bytes(&body_path, &body_bytes, "simulated pre-claim segment")
            .expect("publish body before simulated crash");
        assert!(!stage_segment_claim_path(tmp.path(), 0).exists());

        let mut state = StagedNostrCrawlState {
            version: STAGE_FORMAT_VERSION,
            author_allowlist_source: None,
            policy: old_policy,
            next_author: 0,
            events_seen: 10,
            events_selected: 4,
            live_bytes_selected: 1_024,
        };
        let recovered = recover_uncheckpointed_stage_segment(tmp.path(), &state)
            .expect("recover with old persisted cadence")
            .expect("durable uncheckpointed body");
        assert_eq!(recovered, durable);
        assert!(stage_segment_claim_path(tmp.path(), 0).exists());
        advance_stage_state_from_segment(&mut state, &recovered)
            .expect("advance only from adopted durable body");
        assert_eq!(state.next_author, 1);
        assert_eq!(state.events_seen, 17);
        assert_eq!(state.events_selected, 7);
        assert_eq!(state.live_bytes_selected, 1_536);

        let new_policy = stage_test_policy(4, 2);
        validate_stage_state(&state, &new_policy, 4).expect("new cadence is safe after recovery");
        state.policy = new_policy.clone();
        let changed_fetch = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 0,
            end_author: 2,
            events_seen: 999,
            events_selected: 999,
            live_bytes_selected: 999,
            event_cids: Vec::new(),
        };
        let adopted = persist_stage_segment(tmp.path(), &changed_fetch, &new_policy)
            .expect("claimed old body must beat changed refetch results");
        assert_eq!(adopted, durable);
        assert_eq!(
            std::fs::read_dir(tmp.path().join(STAGE_DIR).join(STAGE_SEGMENTS_DIR))
                .expect("read segment directory")
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
                .count(),
            1,
            "restart must not publish a duplicate start under the new cadence"
        );
    }

    #[test]
    fn targeted_stage_load_is_constant_io_and_defers_catalog_duplicate_detection() {
        let tmp = TempDir::new().expect("tempdir");
        let policy = stage_test_policy(20, 1);
        let first = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: 11,
            end_author: 12,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
            event_cids: Vec::new(),
        };
        let mut second = first.clone();
        second.end_author = 13;
        persist_stage_segment(tmp.path(), &first, &policy)
            .expect("publish canonical claimed segment");
        let second_path = stage_segment_path(tmp.path(), second.start_author, second.end_author);
        let mut second_bytes = serde_json::to_vec(&second).expect("encode duplicate segment");
        second_bytes.push(b'\n');
        std::fs::write(second_path, second_bytes).expect("install duplicate legacy body");

        reset_stage_segment_io_counts();
        let loaded = load_stage_segment(tmp.path(), 11, &policy)
            .expect("targeted hot-path load must use the deterministic exact boundary");
        assert_eq!(loaded, first);
        assert_eq!(
            stage_segment_io_counts(),
            (0, 1),
            "targeted loading must perform no directory scan and exactly one segment read"
        );
    }

    #[test]
    fn stage_publish_and_load_do_not_scan_thousands_of_historical_files() {
        const HISTORICAL_SEGMENTS: usize = 2_048;

        let tmp = TempDir::new().expect("tempdir");
        let policy = stage_test_policy(HISTORICAL_SEGMENTS + 1, 1);
        let directory = tmp.path().join(STAGE_DIR).join(STAGE_SEGMENTS_DIR);
        std::fs::create_dir_all(&directory).expect("create real segment directory");
        for start in 0..HISTORICAL_SEGMENTS {
            std::fs::write(
                stage_segment_path(tmp.path(), start, start + 1),
                b"deliberately unread historical body\n",
            )
            .expect("write historical real filesystem entry");
        }
        let tail = StagedAuthorSegment {
            version: STAGE_FORMAT_VERSION,
            start_author: HISTORICAL_SEGMENTS,
            end_author: HISTORICAL_SEGMENTS + 1,
            events_seen: 0,
            events_selected: 0,
            live_bytes_selected: 0,
            event_cids: Vec::new(),
        };

        reset_stage_segment_io_counts();
        persist_stage_segment(tmp.path(), &tail, &policy)
            .expect("publish deterministic tail without historical reads");
        assert_eq!(
            stage_segment_io_counts().0,
            0,
            "immutable tail publication must not enumerate the segment directory"
        );

        reset_stage_segment_io_counts();
        assert_eq!(
            load_stage_segment(tmp.path(), HISTORICAL_SEGMENTS, &policy)
                .expect("load deterministic tail"),
            tail
        );
        assert_eq!(
            stage_segment_io_counts(),
            (0, 1),
            "tail lookup must stay one exact file read regardless of historical catalog size"
        );
    }

    #[tokio::test]
    async fn durable_checkpoint_mode_validates_bounds_before_network_work() {
        let tmp = TempDir::new().expect("tempdir");
        let mut options = checkpoint_test_options(
            "http://127.0.0.1:1/authors".to_string(),
            vec!["ws://127.0.0.1:1".to_string()],
        );
        assert!(uses_durable_author_checkpoints(&options));
        let mut ordinary = options.clone();
        ordinary.full_author_history = false;
        ordinary.negentropy_only = false;
        assert!(!uses_durable_author_checkpoints(&ordinary));

        options.author_batch_size = 0;
        let error = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &Config::default(),
            Keys::generate(),
            options.clone(),
        )
        .await
        .expect_err("zero author batch must fail");
        assert!(error.to_string().contains("--author-batch-size"));

        options.author_batch_size = 1;
        options.max_events_seen = Some(0);
        let error = run_socialgraph_index(
            tmp.path().to_path_buf(),
            &Config::default(),
            Keys::generate(),
            options,
        )
        .await
        .expect_err("zero event budget must fail");
        assert!(error.to_string().contains("--max-events-seen"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpointed_eventless_authors_advance_with_empty_root() -> io::Result<()> {
        let relay = TestRelay::new();
        let author = Keys::generate().public_key().to_hex();
        let allowlist = TestTextServer::new(format!("{author}\n"));
        let options =
            checkpoint_test_options(format!("{}/authors", allowlist.url()), vec![relay.url()]);
        let tmp = TempDir::new().expect("tempdir");
        let mut config = Config::default();
        config.storage.max_size_gb = 1;
        config.nostr.db_max_size_gb = 1;

        let report =
            run_socialgraph_index(tmp.path().to_path_buf(), &config, Keys::generate(), options)
                .await
                .expect("eventless checkpoint crawl");
        assert_eq!(report.authors_processed, 1);
        assert_eq!(report.events_selected, 0);
        assert!(report.root.is_none());
        let state = load_crawl_state(tmp.path())
            .expect("load state")
            .expect("state exists");
        assert_eq!(state.next_author, 1);
        assert!(state.root.is_none());

        Ok(())
    }

    #[test]
    fn crawl_lock_is_exclusive_and_reacquirable() {
        let tmp = TempDir::new().expect("tempdir");
        let first = CrawlStateLock::acquire(tmp.path()).expect("first lock");
        assert!(CrawlStateLock::acquire(tmp.path()).is_err());
        drop(first);
        CrawlStateLock::acquire(tmp.path()).expect("lock after release");
    }

    #[test]
    fn crawl_audit_lock_is_shared_noncreating_and_rejects_writers() {
        let tmp = TempDir::new().expect("tempdir");
        assert!(CrawlStateLock::acquire_shared(tmp.path()).is_err());
        assert!(!tmp.path().join(INDEX_DIR).exists());

        let writer = CrawlStateLock::acquire(tmp.path()).expect("writer lock");
        assert!(CrawlStateLock::acquire_shared(tmp.path()).is_err());
        drop(writer);

        let first_reader = CrawlStateLock::acquire_shared(tmp.path()).expect("first reader");
        let second_reader = CrawlStateLock::acquire_shared(tmp.path()).expect("second reader");
        assert!(CrawlStateLock::acquire(tmp.path()).is_err());
        drop(first_reader);
        drop(second_reader);
        CrawlStateLock::acquire(tmp.path()).expect("writer after readers");
    }

    #[test]
    fn parse_author_allowlist_filters_invalid_and_deduplicates() {
        let parsed = parse_author_allowlist(
            &format!("{}\nnot-hex\n{}\n", "a".repeat(64), "a".repeat(64)),
            16,
        );

        assert_eq!(parsed, vec!["a".repeat(64)]);
    }

    #[test]
    fn parse_author_allowlist_preserves_input_order_before_limit() {
        let parsed = parse_author_allowlist(
            &format!(
                "{}\n{}\n{}\n",
                "b".repeat(64),
                "a".repeat(64),
                "b".repeat(64)
            ),
            2,
        );

        assert_eq!(parsed, vec!["b".repeat(64), "a".repeat(64)]);
    }

    #[test]
    fn loads_existing_root_from_latest_root_file() {
        let tmp = TempDir::new().expect("tempdir");
        let index_dir = tmp.path().join(INDEX_DIR);
        std::fs::create_dir_all(&index_dir).expect("create index dir");
        let cid =
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let nhash = cid_to_nhash(&parse_root_text(cid).expect("parse raw cid")).expect("nhash");
        std::fs::write(index_dir.join(LATEST_ROOT_FILE), format!("{nhash}\n"))
            .expect("write latest root");

        let loaded = load_existing_root(tmp.path()).expect("load root");
        assert_eq!(loaded.expect("existing root").to_string(), cid);
    }

    #[test]
    fn loads_existing_root_from_checkpoint_when_latest_root_is_missing() {
        let tmp = TempDir::new().expect("tempdir");
        let index_dir = tmp.path().join(INDEX_DIR);
        std::fs::create_dir_all(&index_dir).expect("create index dir");
        let cid =
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let nhash = cid_to_nhash(&parse_root_text(cid).expect("parse raw cid")).expect("nhash");
        std::fs::write(index_dir.join(CHECKPOINT_ROOT_FILE), format!("{nhash}\n"))
            .expect("write checkpoint root");

        let loaded = load_existing_root(tmp.path()).expect("load root");
        assert_eq!(loaded.expect("checkpoint root").to_string(), cid);
    }

    #[test]
    fn persist_report_clears_checkpoint_files() {
        let tmp = TempDir::new().expect("tempdir");
        let index_dir = tmp.path().join(INDEX_DIR);
        std::fs::create_dir_all(&index_dir).expect("create index dir");
        std::fs::write(
            index_dir.join(CHECKPOINT_ROOT_FILE),
            "nhash1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\n",
        )
            .expect("write checkpoint root");
        std::fs::write(index_dir.join(CHECKPOINT_REPORT_FILE), "{}")
            .expect("write checkpoint report");

        let report = IndexedNostrReport {
            root: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                    .to_string(),
            ),
            profile_search_root: Some("nhash1profileexample".to_string()),
            authors_considered: 10,
            authors_processed: 10,
            events_seen: 11,
            events_selected: 12,
            live_bytes_selected: 13,
            warm_graph_seconds: 0,
            graph_crawl_depth: 1,
            full_graph_recrawl: false,
            max_events_seen: None,
            max_follow_distance: Some(1),
            max_authors: 10,
            max_live_bytes: 14,
            per_author_live_bytes: None,
            relay_event_max_bytes: None,
            global_relay_scan: false,
            negentropy_only: false,
            full_author_history: false,
            relay_page_size: 1000,
            max_relay_pages: 10,
            relays: vec!["wss://example.com".to_string()],
            top_authors: Vec::new(),
            top_kinds: Vec::new(),
            top_hashtags: Vec::new(),
            recent_events: Vec::new(),
        };

        persist_report(tmp.path(), &report).expect("persist report");
        clear_checkpoint(tmp.path()).expect("clear checkpoint");

        let saved_root =
            std::fs::read_to_string(index_dir.join(LATEST_ROOT_FILE)).expect("read latest root");
        assert!(saved_root.trim().starts_with("nhash1"));

        assert!(!index_dir.join(CHECKPOINT_ROOT_FILE).exists());
        assert!(!index_dir.join(CHECKPOINT_REPORT_FILE).exists());
        assert!(index_dir.join(LATEST_ROOT_FILE).exists());
        assert!(index_dir.join(LATEST_REPORT_FILE).exists());
    }

    #[test]
    fn relay_info_url_maps_websocket_urls_to_http() {
        assert_eq!(
            relay_info_url("ws://127.0.0.1:1234").expect("ws url"),
            "http://127.0.0.1:1234"
        );
        assert_eq!(
            relay_info_url("wss://relay.example").expect("wss url"),
            "https://relay.example"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_index_relays_keeps_only_relays_advertising_nip77() {
        let supported = TestNip11Server::new("200 OK", r#"{"supported_nips":[11,77]}"#.to_string());
        let unsupported =
            TestNip11Server::new("200 OK", r#"{"supported_nips":[11,12]}"#.to_string());
        let broken = TestNip11Server::new("500 Internal Server Error", "{}".to_string());

        let relays = vec![
            supported.relay_url(),
            unsupported.relay_url(),
            broken.relay_url(),
        ];
        let resolved = resolve_index_relays(relays, true)
            .await
            .expect("resolve relays");

        assert_eq!(resolved, vec![supported.relay_url()]);
    }
}
