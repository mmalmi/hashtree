use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use hashtree_cli::{Config, HashtreeStore};
use hashtree_nostr::catchup::{
    fetch_catchup_author, CatchupPolicy, CatchupState, DEFAULT_CATCHUP_OVERLAP_SECS,
};
use hashtree_nostr::{NostrEventStore, NostrEventStoreOptions};
use sha2::{Digest, Sha256};

use super::{cid_to_nhash, parse_root_text, persist_json_atomic, CrawlStateLock, INDEX_DIR};

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
    /// Required source relay (repeatable); every source must complete.
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let mut state = CatchupState::prepare(saved, policy, args.until, now)?;
    let config = Config::load()?;
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        config.storage.max_size_gb * 1024 * 1024 * 1024,
    )?);
    let event_store = NostrEventStore::with_options(
        store.store_arc(),
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
    let mut root = parse_root_text(&state.root).context("parse catchup checkpoint root")?;
    event_store
        .validate_index_root(Some(&root))
        .await
        .context("validate catchup checkpoint root")?;
    persist_json_atomic(&state_file, &state, "Nostr catchup checkpoint")?;
    let end = args
        .max_authors_per_run
        .map(|count| state.next_author.saturating_add(count).min(authors.len()))
        .unwrap_or(authors.len());
    let mut source = relay::RelaySource::new(
        state.policy.fetch_timeout_secs,
        state.policy.max_bytes_per_author,
    );
    while state.next_author < end {
        let author = &authors[state.next_author];
        let events = fetch_catchup_author(
            &mut source,
            &state.policy,
            author,
            state.pass_since,
            state.pass_until,
        )
        .await?;
        let received = events.len() as u64;
        let report = event_store
            .build_with_superseded_nodes(Some(&root), events)
            .await
            .with_context(|| format!("append catchup author {} ({author})", state.next_author))?;
        let next_root = report
            .root
            .context("catchup writer discarded its nonempty base root")?;
        event_store.validate_index_root(Some(&next_root)).await?;
        store.force_sync().context("force-sync catchup blocks")?;
        let mut next = state.clone();
        next.root = cid_to_nhash(&next_root)?;
        next.next_author += 1;
        next.events_received = next.events_received.saturating_add(received);
        persist_json_atomic(&state_file, &next, "Nostr catchup checkpoint")?;
        state = next;
        root = next_root;
        // Do not delete superseded nodes: published roots and rollback readers
        // may still depend on them. Publication owns eventual root-aware GC.
        eprintln!(
            "Nostr catchup checkpoint: authors={}/{} interval={}..{} events_received={}",
            state.next_author,
            authors.len(),
            state.pass_since,
            state.pass_until,
            state.events_received
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "format": "hashtree/nostr-index-catchup@2",
            "root": state.root,
            "pass_since": state.pass_since,
            "pass_until": state.pass_until,
            "next_author": state.next_author,
            "author_count": authors.len(),
            "events_received": state.events_received,
            "complete": state.complete(),
        })
    );
    Ok(())
}
