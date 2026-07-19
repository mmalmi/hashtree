use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{Context, Result};
use hashtree_core::{nhash_decode, nhash_encode_full, Cid, NHashData};
use hashtree_nostr::{
    CrawlConfig, CrawlReport, ListEventsOptions, NostrBridge, NostrEventStore, RelayFetchMode,
    StoredNostrEvent, VerifiedEvent,
};
use nostr::Keys;
use nostr_sdk::{Event as NostrSdkEvent, Filter as NostrFilter};
use reqwest::header::ACCEPT;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use hashtree_cli::config::{ensure_keys, parse_npub};
use hashtree_cli::socialgraph::{self, SocialGraphBackend, SocialGraphCrawler};
use hashtree_cli::{Config, HashtreeStore};

const INDEX_DIR: &str = "nostr-index";
const LATEST_ROOT_FILE: &str = "latest-root.txt";
const LATEST_REPORT_FILE: &str = "latest-report.json";
const CHECKPOINT_ROOT_FILE: &str = "checkpoint-root.txt";
const CHECKPOINT_REPORT_FILE: &str = "checkpoint-report.json";
const CRAWL_STATE_FILE: &str = "crawl-state.json";
const CRAWL_LOCK_FILE: &str = "crawl.lock";
const CRAWL_STATE_VERSION: u32 = 1;
const MIRROR_STATE_DIR: &str = "nostr-mirror";
const MIRROR_UPLOADED_EVENT_ROOT_FILE: &str = "nostr-event-index.uploaded-root";
const TOP_ITEMS_LIMIT: usize = 20;
const NEGENTROPY_NIP: u16 = 77;

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
    pub(crate) max_follow_distance: Option<u32>,
    pub(crate) max_live_bytes: u64,
    pub(crate) author_batch_size: usize,
    pub(crate) checkpoint_authors: usize,
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
    root: Option<String>,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
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
        let output_dir = data_dir.join(INDEX_DIR);
        std::fs::create_dir_all(&output_dir)
            .with_context(|| format!("create {}", output_dir.display()))?;
        let path = output_dir.join(CRAWL_LOCK_FILE);
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
    if checkpointed_allowlist && options.checkpoint_authors == 0 {
        anyhow::bail!("--checkpoint-authors must be greater than zero");
    }
    if checkpointed_allowlist && options.author_batch_size == 0 {
        anyhow::bail!("--author-batch-size must be greater than zero");
    }
    if checkpointed_allowlist && options.max_events_seen == Some(0) {
        anyhow::bail!("--max-events-seen must be greater than zero");
    }
    if options.per_author_kind_event_limit == Some(0) {
        anyhow::bail!("--per-author-kind-event-limit must be greater than zero");
    }
    let _crawl_lock = checkpointed_allowlist
        .then(|| CrawlStateLock::acquire(&data_dir))
        .transpose()?;

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
    let event_store = NostrEventStore::new(store.store_arc());

    if checkpointed_allowlist {
        let authors = author_allowlist
            .as_ref()
            .context("checkpointed crawl requires an explicit author allowlist")?;
        validate_reachable_root(&event_store, existing_root.as_ref(), "crawl base root").await?;
        let policy = build_crawl_policy(&options, &relays, authors, existing_root.as_ref())?;
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
                root: existing_root.as_ref().map(cid_to_nhash).transpose()?,
                events_seen: 0,
                events_selected: 0,
                live_bytes_selected: 0,
            },
        };
        state.author_allowlist_source = options.author_allowlist_url.clone();
        state.policy = policy;
        persist_crawl_state(&data_dir, &state)?;
        let report = crawl_allowlist_in_checkpoints(
            store.store_arc(),
            graph_store.as_ref(),
            &data_dir,
            &options,
            &relays,
            authors,
            &mut state,
        )
        .await?;
        let profile_search_root = graph_store
            .profile_search_root()?
            .as_ref()
            .map(cid_to_nhash)
            .transpose()?;
        let index_report = build_bounded_report(&relays, &options, report, profile_search_root)?;
        persist_report(&data_dir, &index_report)?;
        clear_checkpoint(&data_dir)?;
        print_report(&index_report, &data_dir);
        return Ok(index_report);
    }

    let bridge = NostrBridge::new(
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

async fn crawl_allowlist_in_checkpoints(
    store: Arc<hashtree_cli::storage::StorageRouter>,
    graph_store: &socialgraph::SocialGraphStore,
    data_dir: &Path,
    options: &SocialGraphIndexOptions,
    relays: &[String],
    authors: &[String],
    state: &mut IndexedNostrCrawlState,
) -> Result<CrawlReport> {
    let checkpoint_authors = options
        .checkpoint_authors
        .min(options.author_batch_size)
        .max(1);
    let event_store = NostrEventStore::new(Arc::clone(&store));

    while state.next_author < authors.len() {
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
        let bridge = NostrBridge::new(
            Arc::clone(&store),
            build_crawl_config(
                options,
                relays,
                Some(author_batch),
                remaining_live_bytes,
                remaining_events_seen,
                end - state.next_author,
            ),
        );
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
        persist_crawl_state(data_dir, state)?;
        eprintln!(
            "Nostr index checkpoint: authors={}/{} events_seen={} events_selected={} live_bytes={} relay_fetch_select_ms={} index_build_ms={} batch_elapsed_ms={}",
            state.next_author,
            authors.len(),
            state.events_seen,
            state.events_selected,
            state.live_bytes_selected,
            report.relay_fetch_select_ms,
            report.index_build_ms,
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
    })
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
    Ok(())
}

fn persist_crawl_state(data_dir: &Path, state: &IndexedNostrCrawlState) -> Result<()> {
    let output_dir = data_dir.join(INDEX_DIR);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("create {}", output_dir.display()))?;
    let path = output_dir.join(CRAWL_STATE_FILE);
    let temp_path = output_dir.join(format!(".{CRAWL_STATE_FILE}.tmp"));
    let mut bytes = serde_json::to_vec_pretty(state).context("encode Nostr crawl state")?;
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
    std::fs::rename(&temp_path, &path)
        .with_context(|| format!("rename {} to {}", temp_path.display(), path.display()))?;
    #[cfg(unix)]
    File::open(&output_dir)
        .with_context(|| format!("open {} for fsync", output_dir.display()))?
        .sync_all()
        .with_context(|| format!("fsync {}", output_dir.display()))?;
    Ok(())
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
            max_follow_distance: Some(0),
            max_live_bytes: 32 * 1024 * 1024,
            author_batch_size: 1,
            checkpoint_authors: 1,
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
                max_follow_distance: Some(1),
                max_live_bytes: 8 * 1024 * 1024,
                author_batch_size: 32,
                checkpoint_authors: 8,
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
                max_follow_distance: Some(0),
                max_live_bytes: 8 * 1024 * 1024,
                author_batch_size: 32,
                checkpoint_authors: 8,
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
