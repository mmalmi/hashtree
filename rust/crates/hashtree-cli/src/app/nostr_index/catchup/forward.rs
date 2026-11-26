//! Bounded forward ingestion into the same archive, between historical author
//! transactions. Neither event intake nor a retry advances historical coverage.
use anyhow::{ensure, Context, Result};
use hashtree_cli::{Config, HashtreeStore};
use hashtree_nostr::{NostrEventStore, NostrEventStoreOptions};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

use super::super::{cid_to_nhash, parse_root_text, persist_json_atomic, CrawlStateLock, INDEX_DIR};
use super::coverage;

pub(super) mod bridge;
mod input;
mod journal;
mod receipt;

#[derive(clap::Args, Debug)]
pub(crate) struct AppendForwardArgs {
    /// Immutable canonical batch exported by the forward collector.
    #[arg(long)]
    batch: PathBuf,
    /// SHA256 of the exact batch bytes, including its encoding.
    #[arg(long)]
    batch_sha256: String,
    /// Pinned collector configuration digest.
    #[arg(long)]
    config_sha256: String,
    #[arg(long)]
    since: u64,
    /// Required for graph-based forward batches.
    #[arg(long, requires = "graph_authors_sha256")]
    graph_authors_file: Option<PathBuf>,
    #[arg(long, requires = "graph_authors_file")]
    graph_authors_sha256: Option<String>,
    /// Existing historical author census, checked against the saved policy.
    /// Forward event authors are not restricted to this census.
    #[arg(long)]
    authors_file: PathBuf,
    #[arg(long, default_value_t = 10 * 1024 * 1024 * 1024u64)]
    min_free_bytes: u64,
}

fn open_store(data: &Path, min_free_bytes: u64) -> Result<HashtreeStore> {
    let config = Config::load()?;
    HashtreeStore::with_catchup_physical_space(
        data,
        config.storage.s3.as_ref(),
        config.storage.max_size_gb * 1024 * 1024 * 1024,
        min_free_bytes,
    )
    .context("open forward archive storage")
}

// Caller already holds CrawlStateLock. This performs no network fetch and no
// store open on the usual no-pending path.
pub(super) async fn recover_if_pending(
    data: &Path,
    authors: &[u8],
    min_free_bytes: u64,
) -> Result<()> {
    let directory = data.join(INDEX_DIR);
    if journal::present(&directory.join(journal::PENDING))? {
        let store = open_store(data, min_free_bytes)?;
        journal::recover(&store, &directory, authors).await?;
    }
    Ok(())
}

pub(crate) async fn run(data_dir: PathBuf, args: AppendForwardArgs) -> Result<()> {
    // Fully authenticate and bound input before opening any writable namespace.
    let batch = coverage::read_bounded(&args.batch, input::MAX_BATCH)?;
    let validated = input::validate(&batch, &args.batch_sha256, &args.config_sha256, args.since)?;
    match (
        &args.graph_authors_file,
        &args.graph_authors_sha256,
        &validated.graph_authors_sha256,
    ) {
        (Some(path), Some(sha), Some(_)) => {
            let bytes = coverage::read_bounded(path, input::MAX_GRAPH_AUTHORS)?;
            input::verify_graph_authors(&validated, &bytes, sha)?;
        }
        (None, None, None) => {}
        _ => anyhow::bail!("forward batch and graph authority differ"),
    }
    let author_bytes = coverage::read_bounded(&args.authors_file, 64 * 1024 * 1024)?;
    let directory = data_dir.join(INDEX_DIR);
    // This command only appends to an established catch-up archive.
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "invalid forward archive directory"
    );
    #[cfg(feature = "lmdb")]
    if !directory.join(super::super::CRAWL_LOCK_FILE).exists() {
        let guard = hashtree_lmdb::PhysicalSpaceGuard::new(args.min_free_bytes)?;
        guard
            .admit_file(&std::fs::File::open(&directory)?, 0, 1)
            .with_context(|| guard.status())?;
    }
    let _lock = CrawlStateLock::acquire(&data_dir)?;
    let store = open_store(&data_dir, args.min_free_bytes)?;
    journal::recover(&store, &directory, &author_bytes).await?;
    let path = journal::receipt_path(&directory, &args.batch_sha256)?;
    let output = if journal::present(&path)? {
        let bytes = coverage::read(&path)?;
        let receipt: receipt::Receipt = serde_json::from_slice(&bytes)?;
        ensure!(
            receipt.batch_sha256 == args.batch_sha256
                && receipt.config_sha256 == args.config_sha256
                && receipt.since == args.since,
            "forward replay identity differs"
        );
        journal::verify(&store, &directory, &receipt, &author_bytes).await?;
        // A completed batch can be acknowledged after later historical work;
        // never restore its old checkpoint or append those events again.
        bytes
    } else {
        append(
            &store,
            &directory,
            &batch,
            &validated.events,
            &author_bytes,
            &args,
        )
        .await?
    };
    std::io::stdout()
        .lock()
        .write_all(&output)
        .context("write durable forward receipt")
}

async fn append(
    store: &HashtreeStore,
    directory: &Path,
    batch: &[u8],
    events: &[hashtree_nostr::StoredNostrEvent],
    authors: &[u8],
    args: &AppendForwardArgs,
) -> Result<Vec<u8>> {
    ensure!(
        !journal::present(&directory.join("catchup-append.json"))?,
        "historical append is pending; forward append refused"
    );
    let before = journal::state(directory)?;
    let author_keys = input::authors(authors, &before)?;
    let coverage_directory = directory.join("catchup-coverage");
    coverage::validate_head(&coverage_directory, &before, &author_keys)?;
    let root = parse_root_text(&before.root)?;
    let writer = NostrEventStore::with_options(
        store.store_arc(),
        NostrEventStoreOptions {
            index_commit_batch_size: Some(32),
            ..Default::default()
        },
    )
    .with_index_write_buffer_bytes(8 * 1024 * 1024);
    writer.validate_index_root(Some(&root)).await?;
    let batch_path = journal::batch_path(directory, &args.batch_sha256)?;
    coverage::persist_bytes(store, batch_path.parent().unwrap(), &batch_path, batch)?;
    // Copy-on-write build returns obsolete blocks, but none are reclaimed:
    // published/rollback roots and historical coverage remain readable.
    let built = writer
        .build_with_superseded_nodes(Some(&root), events.to_vec())
        .await?;
    let after_root = built.root.context("forward append returned no root")?;
    writer.validate_index_root(Some(&after_root)).await?;
    let dispositions = receipt::dispositions(&writer, &after_root, events).await?;
    let bridge = bridge::Bridge::new(
        &coverage_directory,
        &before,
        cid_to_nhash(&after_root)?,
        args.batch_sha256.clone(),
        &author_keys,
    )?;
    let receipt =
        receipt::Receipt::new(bridge, args.config_sha256.clone(), args.since, dispositions)?;
    store
        .force_sync()
        .context("sync forward event and index blocks")?;
    let head = receipt
        .after
        .coverage_head
        .as_deref()
        .context("forward coverage head missing")?;
    coverage::persist_bytes(
        store,
        &coverage_directory,
        &coverage::receipt_path(&coverage_directory, head)?,
        &receipt.coverage_bridge.bytes()?,
    )?;
    let pending = directory.join(journal::PENDING);
    store.admit_checkpoint_write(&pending, receipt.bytes()?.len())?;
    persist_json_atomic(&pending, &receipt, "forward pending transaction")?;
    journal::finish(store, directory, &receipt, authors).await
}

#[cfg(test)]
mod tests;
