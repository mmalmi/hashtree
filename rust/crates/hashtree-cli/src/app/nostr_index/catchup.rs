use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use hashtree_cli::{Config, HashtreeStore};
use hashtree_nostr::catchup::{
    fetch_catchup_author_with_coverage, CatchupPolicy, CatchupRunSources, CatchupSourceMode,
    CatchupState, DEFAULT_CATCHUP_OVERLAP_SECS,
};
use hashtree_nostr::{NostrEventStore, NostrEventStoreOptions};
use sha2::{Digest, Sha256};

use super::{cid_to_nhash, parse_root_text, persist_json_atomic, CrawlStateLock, INDEX_DIR};

mod coverage;
mod pipeline;
mod read_cache;
mod relay;

#[derive(clap::Args, Debug)]
pub(crate) struct CatchupArgs {
    /// Exact original archive root. Never inferred from a mutable pointer.
    #[arg(long)]
    root: String,
    /// Ordered newline-delimited lowercase author pubkeys.
    #[arg(long)]
    authors_file: PathBuf,
    /// Conservative original catchup floor, retained in the resume identity.
    #[arg(long)]
    since: u64,
    /// Fixed pass end. Defaults to saved unfinished end, or now for a new pass.
    #[arg(long)]
    until: Option<u64>,
    /// Revisit this many seconds before the previous pass end for late arrivals.
    #[arg(long, default_value_t = DEFAULT_CATCHUP_OVERLAP_SECS)]
    overlap_secs: u64,
    /// Source completion policy. Best-effort advances with at least one complete source.
    #[arg(long, default_value = "strict", value_parser = ["strict", "best-effort"])]
    source_mode: String,
    /// Source relay (repeatable); strict requires every source to complete.
    #[arg(long = "relay", required = true)]
    relays: Vec<String>,
    /// Kind to retain (repeatable). Include kind 5 for deletion events.
    #[arg(long = "kind", required = true)]
    kinds: Vec<u16>,
    /// Stop successfully after this many durable author checkpoints.
    #[arg(long)]
    max_authors_per_run: Option<usize>,
    #[arg(long, default_value_t = 1000)]
    page_size: usize,
    #[arg(long, default_value_t = 10000)]
    max_pages_per_author: usize,
    #[arg(long, default_value_t = 65536)]
    max_events_per_author: usize,
    #[arg(long, default_value_t = 67108864)]
    max_bytes_per_author: usize,
    #[arg(long, default_value_t = 30)]
    fetch_timeout_secs: u64,
    #[arg(long, default_value_t = 256)]
    index_commit_batch_size: usize,
    /// Immutable read-cache payload budget in MiB (0 disables; not resume policy).
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(0..=256))]
    read_cache_mib: u16,
    /// Physical free-space floor checked at each local write (not resume policy).
    #[arg(long, default_value_t = 10 * 1024 * 1024 * 1024u64)]
    min_free_bytes: u64,
}

pub(crate) async fn run(data_dir: PathBuf, args: CatchupArgs) -> Result<()> {
    if args.max_authors_per_run == Some(0) {
        anyhow::bail!("--max-authors-per-run must be greater than zero");
    }
    let author_bytes = std::fs::read(&args.authors_file).context("read catchup authors")?;
    if author_bytes.len() > 64 * 1024 * 1024 {
        anyhow::bail!("catchup author file exceeds 64 MiB");
    }
    let text = std::str::from_utf8(&author_bytes).context("catchup authors must be UTF-8")?;
    let authors = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut unique = BTreeSet::new();
    for author in &authors {
        if author.len() != 64
            || author
                .bytes()
                .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
            || !unique.insert(author)
        {
            anyhow::bail!("catchup authors must be unique lowercase hexadecimal public keys");
        }
    }
    let mut relays = args.relays;
    relays.sort();
    relays.dedup();
    for relay in &relays {
        let url = reqwest::Url::parse(relay).context("invalid catchup relay URL")?;
        if !matches!(url.scheme(), "ws" | "wss") || url.host_str().is_none() {
            anyhow::bail!("catchup relays must be ws:// or wss:// URLs");
        }
    }
    let mut kinds = args.kinds;
    kinds.sort();
    kinds.dedup();
    if !kinds.contains(&5) || kinds.iter().any(|kind| (20000..30000).contains(kind)) {
        anyhow::bail!("catchup must retain kind 5 and cannot retain ephemeral kinds");
    }
    let base_root = parse_root_text(&args.root).context("parse exact catchup base root")?;
    let policy = CatchupPolicy {
        base_root: cid_to_nhash(&base_root)?,
        source_mode: if args.source_mode == "best-effort" {
            CatchupSourceMode::BestEffort
        } else {
            CatchupSourceMode::Strict
        },
        authors_sha256: hex::encode(Sha256::digest(&author_bytes)),
        author_count: authors.len(),
        initial_since: args.since,
        overlap_secs: args.overlap_secs,
        relays,
        kinds,
        page_size: args.page_size,
        max_pages_per_author: args.max_pages_per_author,
        max_events_per_author: args.max_events_per_author,
        max_bytes_per_author: args.max_bytes_per_author,
        fetch_timeout_secs: args.fetch_timeout_secs,
        index_commit_batch_size: args.index_commit_batch_size,
    };
    // The advisory lock has no payload; only first-time namespace creation can
    // allocate. Admit that small operation before opening any writable store.
    #[cfg(feature = "lmdb")]
    {
        let guard = hashtree_lmdb::PhysicalSpaceGuard::new(args.min_free_bytes)?;
        let directory = data_dir.join(INDEX_DIR);
        guard
            .create_dir_all(&directory)
            .with_context(|| guard.status())?;
        if !directory.join(super::CRAWL_LOCK_FILE).exists() {
            guard
                .admit_file(&std::fs::File::open(&directory)?, 0, 1)
                .with_context(|| guard.status())?;
        }
    }
    // Share the existing writer lock while preserving the original crawl state.
    let _lock = CrawlStateLock::acquire(&data_dir)?;
    let state_file = data_dir.join(INDEX_DIR).join("catchup-state.json");
    let saved = match std::fs::read(&state_file) {
        Ok(bytes) => Some(
            serde_json::from_slice::<CatchupState>(&bytes).context("read catchup checkpoint")?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read catchup checkpoint"),
    };
    let coverage_directory = data_dir.join(INDEX_DIR).join("catchup-coverage");
    if let Some(saved) = &saved {
        coverage::validate_head(&coverage_directory, saved, &authors)?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let state = CatchupState::prepare(saved, policy, args.until, now)?;
    let config = Config::load()?;
    let store = Arc::new(HashtreeStore::with_catchup_physical_space(
        &data_dir,
        config.storage.s3.as_ref(),
        config.storage.max_size_gb * 1024 * 1024 * 1024,
        args.min_free_bytes,
    ).with_context(|| format!("open catch-up storage: physical-space floor={} metadata_margin=16777216 max_write_quantum=67108864", args.min_free_bytes))?);
    let read_cache = Arc::new(read_cache::CatchupReadCache::new(
        store.store_arc(),
        usize::from(args.read_cache_mib) * 1024 * 1024,
    ));
    let event_store = NostrEventStore::with_options(
        read_cache.clone(),
        NostrEventStoreOptions {
            index_commit_batch_size: Some(state.policy.index_commit_batch_size),
            ..Default::default()
        },
    );
    // An unreadable supplied root is fatal. Never silently start an empty index.
    event_store
        .validate_index_root(Some(&base_root))
        .await
        .context("validate original catchup base root")?;
    let root = parse_root_text(&state.root).context("parse catchup checkpoint root")?;
    event_store
        .validate_index_root(Some(&root))
        .await
        .context("validate catchup checkpoint root")?;
    store.admit_checkpoint_write(&state_file, serde_json::to_vec(&state)?.len() + 1)?;
    persist_json_atomic(&state_file, &state, "Nostr catchup checkpoint")?;
    let end = args
        .max_authors_per_run
        .map(|count| state.next_author.saturating_add(count).min(authors.len()))
        .unwrap_or(authors.len());
    let authors = Arc::new(authors);
    let policy = Arc::new(state.policy.clone());
    let pass_since = state.pass_since;
    let pass_until = state.pass_until;
    let run_sources = CatchupRunSources::default();
    let sources = std::array::from_fn(|_| {
        relay::RelaySource::new(policy.fetch_timeout_secs, policy.max_bytes_per_author)
    });
    let (_, state) = pipeline::run(
        sources,
        state.next_author..end,
        |mut source, ordinal| {
            let authors = authors.clone();
            let policy = policy.clone();
            let run_sources = run_sources.clone();
            async move {
                let started = Instant::now();
                let events = fetch_catchup_author_with_coverage(
                    &mut source,
                    &policy,
                    &run_sources,
                    &authors[ordinal],
                    pass_since,
                    pass_until,
                )
                .await
                .map_err(anyhow::Error::from);
                let fetch_ms = started.elapsed().as_millis();
                (source, events.map(|events| (events, fetch_ms, started)))
            }
        },
        (root, state),
        |(root, state), ordinal, (fetched, fetch_ms, author_started)| {
            let author = &authors[ordinal];
            let store = &store;
            let event_store = &event_store;
            let read_cache = &read_cache;
            let state_file = &state_file;
            let coverage_directory = &coverage_directory;
            let author_count = authors.len();
            async move {
                // Fetches may finish out of order, but there is exactly one
                // writer and its next author must match the saved frontier.
                anyhow::ensure!(ordinal == state.next_author, "catchup author order changed");
                let received = fetched.events.len() as u64;
                let append_started = Instant::now();
                let report = event_store
                    .build_with_superseded_nodes(Some(&root), fetched.events)
                    .await
                    .with_context(|| {
                        format!(
                            "append catchup author {} ({author}); {}",
                            state.next_author,
                            store.physical_space_status()
                        )
                    })?;
                let append_ms = append_started.elapsed().as_millis();
                let next_root = report
                    .root
                    .context("catchup writer discarded its nonempty base root")?;
                let validate_started = Instant::now();
                event_store.validate_index_root(Some(&next_root)).await?;
                let validate_ms = validate_started.elapsed().as_millis();
                let sync_started = Instant::now();
                store.force_sync().context("force-sync catchup blocks")?;
                let sync_ms = sync_started.elapsed().as_millis();
                let checkpoint_started = Instant::now();
                let mut next = state.clone();
                next.root = cid_to_nhash(&next_root)?;
                next.next_author += 1;
                next.events_received = next.events_received.saturating_add(received);
                if state.policy.source_mode == CatchupSourceMode::BestEffort {
                    next = coverage::commit(store,coverage_directory,state_file,&state,next,author,fetched.sources)?;
                } else {
                    store.admit_checkpoint_write(&state_file, serde_json::to_vec(&next)?.len() + 1)?;
                    persist_json_atomic(&state_file, &next, "Nostr catchup checkpoint")?;
                }
                let state = next;
                // Do not delete superseded nodes: published roots and rollback readers
                // may still depend on them. Publication owns eventual root-aware GC.
                let (read_hits, read_misses, cache_bytes, cache_entries) = read_cache.read_stats();
                eprintln!(
                    "Nostr catchup checkpoint: authors={}/{} interval={}..{} events_received={} author_events={} fetch_ms={} append_ms={} validate_ms={} sync_ms={} checkpoint_ms={} elapsed_ms={} read_cache_hits={} read_cache_misses={} read_cache_bytes={} read_cache_entries={}",
                    state.next_author,
                    author_count,
                    state.pass_since,
                    state.pass_until,
                    state.events_received,
                    received,
                    fetch_ms,
                    append_ms,
                    validate_ms,
                    sync_ms,
                    checkpoint_started.elapsed().as_millis(),
                    author_started.elapsed().as_millis(),
                    read_hits,
                    read_misses,
                    cache_bytes,
                    cache_entries
                );
                Ok((next_root, state))
            }
        },
    )
    .await?;
    let mut output = serde_json::json!({
        "format": "hashtree/nostr-index-catchup@2",
        "root": state.root,
        "pass_since": state.pass_since,
        "pass_until": state.pass_until,
        "next_author": state.next_author,
        "author_count": authors.len(),
        "events_received": state.events_received,
        "complete": state.complete(),
    });
    if state.policy.source_mode == CatchupSourceMode::BestEffort {
        output["coverage_head"] = serde_json::json!(state.coverage_head);
        output["source_mode"] = serde_json::json!(state.policy.source_mode);
    }
    println!("{output}");
    Ok(())
}
