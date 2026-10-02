//! A closed-writer, single-event projection repair. This never advances a crawl.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use hashtree_core::Cid;
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::catchup::CatchupState;
use hashtree_nostr::{
    nostr_event_index_entries, NostrEventIndex, NostrEventManifest, NostrEventStore,
    NostrEventStoreOptions, VerifiedStoredNostrEvent,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use hashtree_cli::{Config, HashtreeStore};

use super::{cid_to_nhash, parse_root_text, CrawlStateLock, CRAWL_LOCK_FILE, INDEX_DIR};

const MAX_INPUT_BYTES: u64 = 1024 * 1024;

#[derive(clap::Args, Debug)]
pub(crate) struct RepairIdArgs {
    /// Exact retained catch-up checkpoint root.
    #[arg(long)]
    root: String,
    /// One original signed kind-1 event JSON object (at most 1 MiB).
    #[arg(long)]
    event: PathBuf,
    #[arg(long)]
    expected_id: String,
    /// SHA-256 of the exact event file bytes, including whitespace.
    #[arg(long)]
    expected_event_sha256: String,
    /// SHA-256 of the exact saved catchup-state.json bytes.
    #[arg(long)]
    expected_checkpoint_sha256: String,
    /// New immutable receipt file; an existing destination is never replaced.
    #[arg(long)]
    receipt: PathBuf,
    /// Physical free-space floor checked at each local write.
    #[arg(long, default_value_t = 10 * 1024 * 1024 * 1024u64)]
    min_free_bytes: u64,
}

#[derive(Serialize)]
struct ReceiptEvent {
    id: String,
    pubkey: String,
    kind: u32,
    created_at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepairReceipt {
    format: &'static str,
    status: &'static str,
    before_root: String,
    after_root: String,
    checkpoint_sha256: String,
    event_file_sha256: String,
    event: ReceiptEvent,
    event_cid: String,
    absent_by_id_before: bool,
    verified_after: bool,
}

fn read_pinned(path: &Path, expected: &str, label: &str) -> Result<Vec<u8>> {
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{label} SHA-256 must be lowercase hexadecimal"
    );
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("open {label}"))?
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label}"))?;
    ensure!(
        bytes.len() as u64 <= MAX_INPUT_BYTES,
        "{label} exceeds 1 MiB"
    );
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == expected,
        "{label} SHA-256 mismatch"
    );
    Ok(bytes)
}

fn index_root(manifest: &NostrEventManifest, index: NostrEventIndex) -> Option<&Cid> {
    match index {
        NostrEventIndex::ById => manifest.by_id.as_ref(),
        NostrEventIndex::ByAuthorTime => manifest.by_author_time.as_ref(),
        NostrEventIndex::ByAuthorKindTime => manifest.by_author_kind_time.as_ref(),
        NostrEventIndex::ByKindTime => manifest.by_kind_time.as_ref(),
        NostrEventIndex::ByKindTimeAuthor => manifest.by_kind_time_author.as_ref(),
        NostrEventIndex::ByTime => manifest.by_time.as_ref(),
        NostrEventIndex::ByTag => manifest.by_tag.as_ref(),
        NostrEventIndex::Replaceable => manifest.replaceable.as_ref(),
        NostrEventIndex::ParameterizedReplaceable => manifest.parameterized_replaceable.as_ref(),
    }
}

fn persist_receipt(store: &HashtreeStore, path: &Path, receipt: &RepairReceipt) -> Result<()> {
    let parent = path.parent().context("receipt has no parent directory")?;
    ensure!(
        parent.is_dir(),
        "receipt parent directory must already exist"
    );
    let mut bytes = serde_json::to_vec(receipt)?;
    bytes.push(b'\n');
    store.admit_checkpoint_write(path, bytes.len())?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .context("publish new immutable repair receipt")?;
    if let Err(error) = File::open(parent).and_then(|directory| directory.sync_all()) {
        // This process created the destination with no-replace semantics.
        // Do not leave a completed claim when its directory fsync failed.
        let _ = std::fs::remove_file(path);
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        return Err(error).context("fsync repair receipt directory");
    }
    Ok(())
}

pub(crate) async fn run(data_dir: PathBuf, args: RepairIdArgs) -> Result<()> {
    ensure!(
        std::fs::symlink_metadata(&args.receipt)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "repair receipt destination already exists or cannot be inspected"
    );
    ensure!(
        args.receipt.parent().is_some_and(Path::is_dir),
        "receipt parent directory must already exist"
    );
    let event_bytes = read_pinned(&args.event, &args.expected_event_sha256, "event file")?;
    let event = VerifiedStoredNostrEvent::try_from(
        serde_json::from_slice::<hashtree_nostr::StoredNostrEvent>(&event_bytes)
            .context("parse one signed event object")?,
    )
    .context("verify original signed event")?
    .into_stored();
    ensure!(
        event.kind == 1,
        "ID repair accepts only an ordinary kind-1 event"
    );
    ensure!(
        event.id == args.expected_id,
        "signed event ID differs from expected ID"
    );
    let root = parse_root_text(&args.root).context("parse exact repair root")?;
    let index_dir = data_dir.join(INDEX_DIR);
    ensure!(
        index_dir.is_dir(),
        "existing catch-up index directory is required"
    );
    #[cfg(feature = "lmdb")]
    if !index_dir.join(CRAWL_LOCK_FILE).exists() {
        let guard = hashtree_lmdb::PhysicalSpaceGuard::new(args.min_free_bytes)?;
        guard
            .admit_file(&File::open(&index_dir)?, 0, 1)
            .with_context(|| guard.status())?;
    }
    let _lock = CrawlStateLock::acquire(&data_dir)?;
    let checkpoint_path = index_dir.join("catchup-state.json");
    let checkpoint_bytes = read_pinned(
        &checkpoint_path,
        &args.expected_checkpoint_sha256,
        "catch-up checkpoint",
    )?;
    let checkpoint: CatchupState = serde_json::from_slice(&checkpoint_bytes)?;
    ensure!(
        parse_root_text(&checkpoint.root)? == root,
        "checkpoint root differs from exact repair root"
    );
    let config = Config::load()?;
    let store = Arc::new(HashtreeStore::with_catchup_physical_space(
        &data_dir, config.storage.s3.as_ref(), config.storage.max_size_gb.saturating_mul(1024 * 1024 * 1024), args.min_free_bytes,
    ).with_context(|| format!("open repair storage: physical-space floor={} metadata_margin=16777216 max_write_quantum=67108864", args.min_free_bytes))?);
    let events = NostrEventStore::with_options(
        store.store_arc(),
        NostrEventStoreOptions {
            index_commit_batch_size: Some(1),
            ..Default::default()
        },
    );
    events.validate_index_root(Some(&root)).await?;
    let before = events.get_manifest(Some(&root)).await?;
    let index = BTree::new(store.store_arc(), BTreeOptions::default());
    ensure!(
        index
            .get_link(before.by_id.as_ref(), &event.id)
            .await?
            .is_none(),
        "event already has a by-ID entry"
    );
    let author_key = nostr_event_index_entries(&event, &root)
        .into_iter()
        .find(|entry| entry.index == NostrEventIndex::ByAuthorKindTime)
        .unwrap()
        .key;
    let original_cid = index
        .get_link(before.by_author_kind_time.as_ref(), &author_key)
        .await?
        .context("original event is not reachable through exact author-kind-time key")?;
    let original =
        VerifiedStoredNostrEvent::try_from(events.load_event_blob(&original_cid).await?)?
            .into_stored();
    ensure!(
        original == event,
        "stored author event differs from original signed input"
    );
    let report = events
        .build_with_superseded_nodes(Some(&root), [event.clone()])
        .await
        .with_context(|| format!("repair event ID; {}", store.physical_space_status()))?;
    let repaired = report.root.context("repair discarded retained root")?;
    ensure!(
        repaired != root,
        "repair did not change missing by-ID projection"
    );
    let after = events.get_manifest(Some(&repaired)).await?;
    for entry in nostr_event_index_entries(&event, &original_cid) {
        ensure!(
            index
                .get_link(index_root(&after, entry.index), &entry.key)
                .await?
                .as_ref()
                == Some(&original_cid),
            "repaired {} entry does not retain original event CID",
            entry.index.name()
        );
    }
    ensure!(
        events
            .get_verified_by_id(Some(&repaired), &event.id)
            .await?
            .map(|verified| verified.into_stored())
            == Some(event.clone()),
        "repaired by-ID event does not match verified original"
    );
    ensure!(
        std::fs::read(&checkpoint_path)? == checkpoint_bytes,
        "catch-up checkpoint changed during repair"
    );
    // Retain every superseded node and every existing crawl/latest-root file.
    store
        .force_sync()
        .context("force-sync repaired archive blocks")?;
    let receipt = RepairReceipt {
        format: "nostr-index/id-repair@1",
        status: "complete",
        before_root: cid_to_nhash(&root)?,
        after_root: cid_to_nhash(&repaired)?,
        checkpoint_sha256: args.expected_checkpoint_sha256,
        event_file_sha256: args.expected_event_sha256,
        event: ReceiptEvent {
            id: event.id,
            pubkey: event.pubkey,
            kind: event.kind,
            created_at: event.created_at,
        },
        event_cid: cid_to_nhash(&original_cid)?,
        absent_by_id_before: true,
        verified_after: true,
    };
    persist_receipt(&store, &args.receipt, &receipt)?;
    println!("{}", serde_json::to_string(&receipt)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::app::args::Cli;
    use crate::app::run::should_spawn_background_update;

    #[test]
    fn nostr_id_repair_cli_requires_pins_and_disables_background_updates() {
        let args = vec![
            "htree",
            "nostr-index",
            "repair-id",
            "--root",
            "retained-root",
            "--event",
            "/fixture/event.json",
            "--expected-id",
            "event-id",
            "--expected-event-sha256",
            "event-sha",
            "--expected-checkpoint-sha256",
            "checkpoint-sha",
            "--receipt",
            "/fixture/new-receipt.json",
        ];
        for flag in [
            "--root",
            "--event",
            "--expected-id",
            "--expected-event-sha256",
            "--expected-checkpoint-sha256",
            "--receipt",
        ] {
            let mut missing = args.clone();
            let at = missing.iter().position(|value| *value == flag).unwrap();
            missing.drain(at..=at + 1);
            assert!(
                Cli::try_parse_from(missing).is_err(),
                "{flag} must be required"
            );
        }
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(!should_spawn_background_update(&cli));
    }
}
