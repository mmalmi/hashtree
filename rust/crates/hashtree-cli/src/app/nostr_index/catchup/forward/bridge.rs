//! One bounded bridge to the original historical coverage receipt. Consecutive
//! forward batches carry the same anchor, rather than growing a resume chain.
use anyhow::{ensure, Result};
use hashtree_nostr::catchup::CatchupState;
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::super::coverage;

pub(super) const FORMAT: &str = "hashtree/nostr-index-catchup-forward@1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in super::super) struct Bridge {
    pub format: String,
    pub batch_sha256: String,
    pub before: CatchupState,
    pub historical_state: CatchupState,
    pub after_root: String,
}

pub(in super::super) fn load(directory: &Path, state: &CatchupState) -> Result<Option<Bridge>> {
    let Some(head) = &state.coverage_head else {
        return Ok(None);
    };
    let bytes = coverage::read(&coverage::receipt_path(directory, head)?)?;
    ensure!(
        coverage::sha(&bytes) == *head,
        "catchup coverage receipt hash mismatch"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value["format"] != FORMAT {
        return Ok(None);
    }
    let bridge: Bridge = serde_json::from_value(value)?;
    ensure!(bridge.bytes()? == bytes, "noncanonical forward bridge");
    Ok(Some(bridge))
}

impl Bridge {
    pub fn new(
        directory: &Path,
        before: &CatchupState,
        after_root: String,
        batch: String,
        authors: &[String],
    ) -> Result<Self> {
        coverage::validate_head(directory, before, authors)?;
        let mut historical_state = before.clone();
        if let Some(previous) = load(directory, before)? {
            historical_state.root = previous.historical_state.root;
            historical_state.coverage_head = previous.historical_state.coverage_head;
        }
        Ok(Self {
            format: FORMAT.into(),
            batch_sha256: batch,
            before: before.clone(),
            historical_state,
            after_root,
        })
    }

    pub fn bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = coverage::canonical(&serde_json::to_value(self)?)?.into_bytes();
        bytes.push(b'\n');
        ensure!(bytes.len() <= 256 * 1024, "forward bridge exceeds limit");
        Ok(bytes)
    }

    pub fn after(&self) -> Result<CatchupState> {
        let mut after = self.before.clone();
        after.root = self.after_root.clone();
        after.coverage_head = Some(coverage::sha(&self.bytes()?));
        Ok(after)
    }

    pub(in super::super) fn validate(
        &self,
        directory: &Path,
        state: &CatchupState,
        authors: &[String],
    ) -> Result<()> {
        ensure!(
            self.format == FORMAT && coverage::is_sha(&self.batch_sha256),
            "invalid forward bridge"
        );
        let mut frontier = self.historical_state.clone();
        frontier.root = self.before.root.clone();
        frontier.coverage_head = self.before.coverage_head.clone();
        ensure!(
            frontier == self.before,
            "forward bridge changes historical coverage"
        );
        let after = self.after()?;
        // Catch-up can save a newly rolled pass before its first fetch fails.
        // Accept only that existing state transition, never a cursor jump.
        let expected = hashtree_nostr::catchup::CatchupState::prepare(
            Some(after),
            self.before.policy.clone(),
            Some(state.pass_until),
            state.pass_until,
        )?;
        ensure!(
            *state == expected,
            "forward bridge does not bind checkpoint frontier"
        );
        let mut historical = state.clone();
        historical.root = self.historical_state.root.clone();
        historical.coverage_head = self.historical_state.coverage_head.clone();
        // Deliberately use the ordinary parser: a bridge cannot anchor another
        // bridge, so resume has a fixed read and recursion bound.
        coverage::validate_historical_head(directory, &historical, authors)
    }
}
