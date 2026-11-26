//! Blocks and bridge are synced before the pending commit. Once pending is
//! durable, either entry point finishes the same state/receipt commit locally.
use anyhow::{ensure, Context, Result};
use hashtree_cli::HashtreeStore;
use hashtree_nostr::{catchup::CatchupState, NostrEventStore};
use std::path::{Path, PathBuf};

use super::super::super::{fsync_parent, parse_root_text, persist_json_atomic};
use super::super::coverage;
use super::{
    input,
    receipt::{self, Receipt},
};

pub(super) const PENDING: &str = "catchup-forward-pending.json";
pub(super) fn present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
pub(super) fn batch_path(directory: &Path, digest: &str) -> Result<PathBuf> {
    ensure!(coverage::is_sha(digest), "invalid forward batch digest");
    Ok(directory
        .join("catchup-forward")
        .join(format!("{digest}.batch.json")))
}
pub(super) fn receipt_path(directory: &Path, digest: &str) -> Result<PathBuf> {
    ensure!(coverage::is_sha(digest), "invalid forward batch digest");
    Ok(directory
        .join("catchup-forward")
        .join(format!("{digest}.json")))
}
pub(super) fn state(directory: &Path) -> Result<CatchupState> {
    serde_json::from_slice(&coverage::read(&directory.join("catchup-state.json"))?)
        .context("read forward catchup checkpoint")
}

pub(super) async fn verify(
    store: &HashtreeStore,
    directory: &Path,
    receipt: &Receipt,
    authors: &[u8],
) -> Result<()> {
    let batch = coverage::read_bounded(
        &batch_path(directory, &receipt.batch_sha256)?,
        input::MAX_BATCH,
    )?;
    let coverage_dir = directory.join("catchup-coverage");
    let events = receipt.validate(&coverage_dir, &batch, authors)?;
    let head = receipt
        .after
        .coverage_head
        .as_deref()
        .context("forward head missing")?;
    ensure!(
        coverage::read(&coverage::receipt_path(&coverage_dir, head)?)?
            == receipt.coverage_bridge.bytes()?,
        "saved forward bridge differs"
    );
    let root = parse_root_text(&receipt.after.root)?;
    let writer = NostrEventStore::new(store.store_arc());
    writer.validate_index_root(Some(&root)).await?;
    ensure!(
        receipt::dispositions(&writer, &root, &events).await? == receipt.events,
        "forward final-root readback differs"
    );
    Ok(())
}

pub(super) async fn finish(
    store: &HashtreeStore,
    directory: &Path,
    receipt: &Receipt,
    authors: &[u8],
) -> Result<Vec<u8>> {
    ensure!(
        !present(&directory.join("catchup-append.json"))?,
        "historical append is pending; forward commit refused"
    );
    let current = state(directory)?;
    ensure!(
        current == receipt.before || current == receipt.after,
        "forward pending checkpoint lineage differs"
    );
    verify(store, directory, receipt, authors).await?;
    // Re-sync on recovery too. A prior sync error is not proof of durability.
    store.force_sync().context("sync forward blocks")?;
    let checkpoint = directory.join("catchup-state.json");
    store.admit_checkpoint_write(&checkpoint, serde_json::to_vec(&receipt.after)?.len() + 1)?;
    persist_json_atomic(&checkpoint, &receipt.after, "forward catchup checkpoint")?;
    let bytes = receipt.bytes()?;
    let path = receipt_path(directory, &receipt.batch_sha256)?;
    coverage::persist_bytes(store, path.parent().unwrap(), &path, &bytes)?;
    // No acknowledgement is returned until the canonical checkpoint, immutable
    // receipt and retirement of this transaction have all reached stable storage.
    std::fs::remove_file(directory.join(PENDING)).context("retire forward transaction")?;
    fsync_parent(&directory.join(PENDING))?;
    Ok(bytes)
}

pub(super) async fn recover(store: &HashtreeStore, directory: &Path, authors: &[u8]) -> Result<()> {
    let pending = directory.join(PENDING);
    if present(&pending)? {
        let receipt: Receipt = serde_json::from_slice(&coverage::read(&pending)?)?;
        finish(store, directory, &receipt, authors).await?;
    }
    Ok(())
}
