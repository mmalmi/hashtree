//! An unpublished batch cursor. Author coverage and publication still advance
//! only after the entire freshly fetched author has been durably appended.
use super::super::{fsync_parent, persist_json_atomic};
use anyhow::{ensure, Context, Result};
use hashtree_cli::HashtreeStore;
use hashtree_nostr::{catchup::CatchupState, StoredNostrEvent};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

const FORMAT: &str = "hashtree/nostr-catchup-append@1";
const MAX_BYTES: u64 = 128 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AppendCheckpoint {
    format: String,
    before: CatchupState,
    events_sha256: String,
    event_count: usize,
    received: u64,
    pub root: String,
    pub next_event: usize,
}

fn commitment(events: &[StoredNostrEvent], received: u64) -> Result<String> {
    // The normalized full input (including signed bytes), not just its length
    // or a fresh relay's claimed coverage, must match before skipping work.
    let mut hasher = Sha256::new();
    serde_json::to_writer(&mut hasher, &(received, events))?;
    Ok(hex::encode(hasher.finalize()))
}

impl AppendCheckpoint {
    pub fn new(
        before: &CatchupState,
        events: &[StoredNostrEvent],
        received: u64,
        root: String,
    ) -> Result<Self> {
        Ok(Self {
            format: FORMAT.into(),
            before: before.clone(),
            events_sha256: commitment(events, received)?,
            event_count: events.len(),
            received,
            root,
            next_event: 0,
        })
    }

    pub fn load(path: &Path, saved: Option<&CatchupState>) -> Result<Option<Self>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= MAX_BYTES,
            "invalid append checkpoint file"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_BYTES as usize,
            "append checkpoint exceeds bound"
        );
        let pending: Self = serde_json::from_slice(&bytes).context("read append checkpoint")?;
        ensure!(
            pending.format == FORMAT
                && pending.before.policy.index_commit_batch_size > 0
                && pending.next_event <= pending.event_count
                && (pending.next_event == pending.event_count
                    || pending.next_event % pending.before.policy.index_commit_batch_size == 0)
                && pending.events_sha256.len() == 64
                && pending
                    .events_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid append checkpoint cursor"
        );
        super::super::parse_root_text(&pending.root).context("invalid append checkpoint root")?;
        let saved = saved.context("append checkpoint without author checkpoint")?;
        if &pending.before == saved {
            return Ok(Some(pending));
        }
        // Crash after the author checkpoint but before cursor retirement. Retire
        // before preparing a new pass, whose relay fetch may then fail without
        // ever replacing this old cursor.
        ensure!(
            pending.next_event == pending.event_count
                && pending.root == saved.root
                && pending.before.policy == saved.policy
                && pending.before.pass_since == saved.pass_since
                && pending.before.pass_until == saved.pass_until
                && pending.before.next_author.checked_add(1) == Some(saved.next_author)
                && pending.before.events_received.checked_add(pending.received)
                    == Some(saved.events_received),
            "append checkpoint lineage differs from saved author checkpoint"
        );
        Self::retire(path)?;
        Ok(None)
    }

    pub fn retire(path: &Path) -> Result<()> {
        std::fs::remove_file(path).context("retire completed append checkpoint")?;
        fsync_parent(path)
    }

    pub fn matches(
        &self,
        state: &CatchupState,
        events: &[StoredNostrEvent],
        received: u64,
    ) -> Result<bool> {
        Ok(&self.before == state
            && self.event_count == events.len()
            && self.received == received
            && self.events_sha256 == commitment(events, received)?)
    }

    pub fn save(&self, store: &HashtreeStore, path: &Path) -> Result<()> {
        let size = serde_json::to_vec(self)?.len() + 1;
        ensure!(
            size <= MAX_BYTES as usize,
            "append checkpoint exceeds bound"
        );
        store.admit_checkpoint_write(path, size)?;
        // Block durability precedes the cursor, just like the author checkpoint.
        store
            .force_sync()
            .context("force-sync append checkpoint blocks")?;
        persist_json_atomic(path, self, "unpublished append checkpoint")
    }
}

#[cfg(test)]
mod tests;
