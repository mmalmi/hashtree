//! Explicit, SHA-pinned bridge for a repaired checkpoint whose old coverage is immutable.
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::super::parse_root_text;
use anyhow::{ensure, Context, Result};
use hashtree_core::Store;
use hashtree_index::{BTree, BTreeOptions, LinkComparisonLimits, LinkTreeVerifier};
use hashtree_nostr::catchup::{CatchupSourceMode, CatchupState};
use hashtree_nostr::{
    nostr_event_index_entries, NostrEventIndex, NostrEventStore, StoredNostrEvent,
    VerifiedStoredNostrEvent,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RepairWitness {
    format: String,
    completed: bool,
    exit_code: i32,
    native_authority_sha256: String,
    native_run_receipt_sha256: String,
    native_repair_receipt_file: PathBuf,
    native_repair_receipt_sha256: String,
    tool: Tool,
    before: Before,
    after: After,
    original_and_pool_pins_unchanged: bool,
    event_file: PathBuf,
    event_file_sha256: String,
    event: StoredNostrEvent,
    event_cid: String,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Tool {
    source_commit: String,
    binary_file: PathBuf,
    binary_sha256: String,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Before {
    state: CatchupState,
    checkpoint_sha256: String,
    checkpoint_file: PathBuf,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct After {
    state: CatchupState,
    checkpoint_sha256: String,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeRepair {
    format: String,
    status: String,
    before_root: String,
    after_root: String,
    checkpoint_sha256: String,
    event_file_sha256: String,
    event: NativeEvent,
    event_cid: String,
    absent_by_id_before: bool,
    verified_after: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeEvent {
    id: String,
    pubkey: String,
    kind: u32,
    created_at: u64,
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn read_pinned(path: &Path, sha: &str) -> Result<Vec<u8>> {
    ensure!(is_hex(sha, 64), "invalid repair witness file digest");
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 2 * 1024 * 1024,
        "invalid repair witness input file"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 2 * 1024 * 1024 && digest(&bytes) == sha,
        "repair witness file digest mismatch"
    );
    Ok(bytes)
}

impl RepairWitness {
    pub(super) fn load(path: &Path, sha: &str) -> Result<Self> {
        let witness: Self = serde_json::from_slice(&read_pinned(path, sha)?)?;
        ensure!(
            witness.format == "iris-social/archive-projection-repair@1"
                && witness.completed
                && witness.exit_code == 0
                && witness.original_and_pool_pins_unchanged,
            "invalid completed repair witness"
        );
        ensure!(
            is_hex(&witness.tool.source_commit, 40)
                && [
                    &witness.native_authority_sha256,
                    &witness.native_run_receipt_sha256,
                    &witness.native_repair_receipt_sha256,
                    &witness.before.checkpoint_sha256,
                    &witness.after.checkpoint_sha256,
                    &witness.event_file_sha256,
                    &witness.tool.binary_sha256,
                ]
                .into_iter()
                .all(|value| is_hex(value, 64)),
            "invalid repair witness pins"
        );
        let mut expected = witness.before.state.clone();
        ensure!(
            expected.root != witness.after.state.root
                && expected.coverage_head.is_some()
                && expected.policy.source_mode == CatchupSourceMode::BestEffort,
            "repair witness needs one best-effort root transition"
        );
        expected.root.clone_from(&witness.after.state.root);
        ensure!(
            expected == witness.after.state,
            "repair witness may change only checkpoint root"
        );
        for root in [
            &witness.before.state.root,
            &witness.after.state.root,
            &witness.event_cid,
        ] {
            parse_root_text(root)?;
        }
        VerifiedStoredNostrEvent::try_from(witness.event.clone())?;
        ensure!(
            witness.event.kind == 1,
            "repair witness must retain an ordinary note"
        );
        Ok(witness)
    }

    /// Called only when ordinary coverage validation failed. A historical flag
    /// can never substitute its state for a different frontier or account policy.
    pub(super) fn original_state(
        &self,
        saved: &CatchupState,
        raw_saved: &[u8],
    ) -> Result<&CatchupState> {
        ensure!(
            *saved == self.after.state && digest(raw_saved) == self.after.checkpoint_sha256,
            "repair witness does not bind the exact repaired checkpoint"
        );
        let before = read_pinned(&self.before.checkpoint_file, &self.before.checkpoint_sha256)?;
        ensure!(
            serde_json::from_slice::<CatchupState>(&before)? == self.before.state,
            "repair witness original checkpoint differs"
        );
        let event = read_pinned(&self.event_file, &self.event_file_sha256)?;
        ensure!(
            serde_json::from_slice::<StoredNostrEvent>(&event)? == self.event,
            "repair witness signed event file differs"
        );
        let native: NativeRepair = serde_json::from_slice(&read_pinned(
            &self.native_repair_receipt_file,
            &self.native_repair_receipt_sha256,
        )?)?;
        ensure!(
            native.format == "nostr-index/id-repair@1"
                && native.status == "complete"
                && native.before_root == self.before.state.root
                && native.after_root == self.after.state.root
                && native.checkpoint_sha256 == self.before.checkpoint_sha256
                && native.event_file_sha256 == self.event_file_sha256
                && native.event_cid == self.event_cid
                && native.absent_by_id_before
                && native.verified_after
                && native.event.id == self.event.id
                && native.event.pubkey == self.event.pubkey
                && native.event.kind == self.event.kind
                && native.event.created_at == self.event.created_at,
            "native repair receipt does not bind witness"
        );
        // The protected repair executable is independent of the running catchup
        // executable. Hash it only when the bridge is actually needed.
        let metadata = std::fs::symlink_metadata(&self.tool.binary_file)?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= 256 * 1024 * 1024,
            "invalid protected repair executable"
        );
        let mut file = File::open(&self.tool.binary_file)?.take(256 * 1024 * 1024 + 1);
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut size = 0usize;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size += read;
            ensure!(
                size <= 256 * 1024 * 1024,
                "protected repair executable exceeds bound"
            );
            hash.update(&buffer[..read]);
        }
        ensure!(
            hex::encode(hash.finalize()) == self.tool.binary_sha256,
            "protected repair executable digest mismatch"
        );
        Ok(&self.before.state)
    }

    pub(super) async fn validate_archive<S: Store>(&self, backing: Arc<S>) -> Result<()> {
        let events = NostrEventStore::new(backing.clone());
        let before_root = parse_root_text(&self.before.state.root)?;
        let after_root = parse_root_text(&self.after.state.root)?;
        let before = events.get_canonical_manifest(&before_root).await?;
        let after = events.get_canonical_manifest(&after_root).await?;
        ensure!(
            before.metadata_cid == after.metadata_cid
                && before.manifest.by_id != after.manifest.by_id,
            "repair witness changed collection metadata or did not repair by-ID"
        );
        let cid = parse_root_text(&self.event_cid)?;
        let stored =
            VerifiedStoredNostrEvent::try_from(events.load_event_blob(&cid).await?)?.into_stored();
        ensure!(stored == self.event, "repair witness stored event differs");
        // The old repair command re-upserts every event projection. A tree
        // built at order64 can therefore be rechunked at its default order32
        // without changing any mapping. Prove the complete logical delta,
        // retaining exact metadata and strict canonical manifest validation.
        let mut verifier = LinkTreeVerifier::new(backing.clone(), LinkComparisonLimits::default());
        for (name, old, new) in [
            ("by-id", &before.manifest.by_id, &after.manifest.by_id),
            (
                "by-author-time",
                &before.manifest.by_author_time,
                &after.manifest.by_author_time,
            ),
            (
                "by-author-kind-time",
                &before.manifest.by_author_kind_time,
                &after.manifest.by_author_kind_time,
            ),
            (
                "by-kind-time",
                &before.manifest.by_kind_time,
                &after.manifest.by_kind_time,
            ),
            (
                "by-kind-time-author",
                &before.manifest.by_kind_time_author,
                &after.manifest.by_kind_time_author,
            ),
            ("by-time", &before.manifest.by_time, &after.manifest.by_time),
            ("by-tag", &before.manifest.by_tag, &after.manifest.by_tag),
            (
                "replaceable",
                &before.manifest.replaceable,
                &after.manifest.replaceable,
            ),
            (
                "parameterized-replaceable",
                &before.manifest.parameterized_replaceable,
                &after.manifest.parameterized_replaceable,
            ),
        ] {
            // An identical full projection CID preserves the accepted baseline
            // verbatim; do not turn this delta proof into an archive count scan.
            if name != "by-id" && old == new {
                continue;
            }
            verifier
                .compare(
                    old.as_ref().context("missing original projection")?,
                    new.as_ref().context("missing repaired projection")?,
                    (name == "by-id").then_some((stored.id.as_str(), &cid)),
                )
                .await
                .with_context(|| format!("repair witness logical {name} comparison"))?;
        }
        let index = BTree::new(backing, BTreeOptions::default());
        let author_key = nostr_event_index_entries(&stored, &cid)
            .into_iter()
            .find(|entry| entry.index == NostrEventIndex::ByAuthorKindTime)
            .context("missing author key")?
            .key;
        for manifest in [&before.manifest, &after.manifest] {
            let links = index
                .get_links_required(
                    manifest
                        .by_author_kind_time
                        .as_ref()
                        .context("missing author root")?,
                    [author_key.clone()],
                )
                .await?;
            ensure!(
                links.get(&author_key) == Some(&cid),
                "repair witness original author CID differs"
            );
        }
        let old = index
            .get_links_required(
                before
                    .manifest
                    .by_id
                    .as_ref()
                    .context("missing old ID root")?,
                [stored.id.clone()],
            )
            .await?;
        let new = index
            .get_links_required(
                after
                    .manifest
                    .by_id
                    .as_ref()
                    .context("missing repaired ID root")?,
                [stored.id.clone()],
            )
            .await?;
        ensure!(
            !old.contains_key(&stored.id) && new.get(&stored.id) == Some(&cid),
            "repair witness did not restore the exact absent by-ID entry"
        );
        Ok(())
    }
}
