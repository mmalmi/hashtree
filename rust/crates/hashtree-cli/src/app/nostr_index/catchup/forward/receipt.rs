use anyhow::{ensure, Context, Result};
use hashtree_core::{Cid, Store};
use hashtree_nostr::{
    catchup::CatchupState, compare_nostr_replaceable_events, nostr_replaceable_slot,
    NostrEventStore, StoredNostrEvent, VerifiedStoredNostrEvent,
};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, path::Path};

use super::{bridge::Bridge, input};

const FORMAT: &str = "hashtree/nostr-index-forward-receipt@1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Disposition {
    pub id: String,
    pub status: String,
    pub retained_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Counts {
    pub input: usize,
    pub indexed: usize,
    pub superseded: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Receipt {
    pub format: String,
    pub batch_sha256: String,
    pub config_sha256: String,
    pub since: u64,
    pub before: CatchupState,
    pub after: CatchupState,
    pub counts: Counts,
    pub events: Vec<Disposition>,
    pub coverage_bridge: Bridge,
}

impl Receipt {
    pub fn new(
        bridge: Bridge,
        config_sha256: String,
        since: u64,
        events: Vec<Disposition>,
    ) -> Result<Self> {
        let indexed = events
            .iter()
            .filter(|event| event.status == "indexed")
            .count();
        Ok(Self {
            format: FORMAT.into(),
            batch_sha256: bridge.batch_sha256.clone(),
            config_sha256,
            since,
            before: bridge.before.clone(),
            after: bridge.after()?,
            counts: Counts {
                input: events.len(),
                indexed,
                superseded: events.len() - indexed,
            },
            events,
            coverage_bridge: bridge,
        })
    }

    pub fn bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        ensure!(bytes.len() <= 256 * 1024, "forward receipt exceeds limit");
        Ok(bytes)
    }

    pub fn validate(
        &self,
        coverage: &Path,
        batch: &[u8],
        authors: &[u8],
    ) -> Result<Vec<StoredNostrEvent>> {
        let events =
            input::validate(batch, &self.batch_sha256, &self.config_sha256, self.since)?.events;
        ensure!(
            self.format == FORMAT
                && self.before == self.coverage_bridge.before
                && self.batch_sha256 == self.coverage_bridge.batch_sha256
                && self.after == self.coverage_bridge.after()?,
            "forward receipt transition differs"
        );
        let authors = input::authors(authors, &self.before)?;
        self.coverage_bridge
            .validate(coverage, &self.after, &authors)?;
        ensure!(
            self.events.len() == events.len(),
            "forward receipt event count differs"
        );
        for (disposition, event) in self.events.iter().zip(&events) {
            ensure!(
                disposition.id == event.id
                    && match disposition.status.as_str() {
                        "indexed" => disposition.retained_id == event.id,
                        "superseded" =>
                            super::super::coverage::is_sha(&disposition.retained_id)
                                && disposition.retained_id != event.id
                                && nostr_replaceable_slot(event).is_some(),
                        _ => false,
                    },
                "invalid forward event disposition"
            );
        }
        let indexed = self
            .events
            .iter()
            .filter(|row| row.status == "indexed")
            .count();
        ensure!(
            self.counts
                == Counts {
                    input: events.len(),
                    indexed,
                    superseded: events.len() - indexed
                },
            "forward receipt counts differ"
        );
        Ok(events)
    }
}

/// An absent ID is never an acknowledgement. The only alternative is an
/// authenticated winner of the exact same replaceable coordinate, also
/// present in the final by-ID projection. Missing/corrupt storage fails closed.
pub(super) async fn dispositions<S: Store>(
    store: &NostrEventStore<S>,
    root: &Cid,
    events: &[StoredNostrEvent],
) -> Result<Vec<Disposition>> {
    let mut rows = Vec::with_capacity(events.len());
    for event in events {
        let (status, retained_id) = match store.get_verified_by_id(Some(root), &event.id).await? {
            Some(retained) => {
                ensure!(
                    retained.as_stored() == event,
                    "forward by-ID event bytes differ"
                );
                ("indexed", event.id.clone())
            }
            None => {
                ensure!(
                    nostr_replaceable_slot(event).is_some(),
                    "forward event missing from final root"
                );
                // This is the existing index's d-tag selection, including its
                // empty-tag fallback; slot equality below is checked as well.
                let d_tag = event
                    .tags
                    .iter()
                    .find_map(|tag| match tag.as_slice() {
                        [name, value, ..] if name == "d" && !value.is_empty() => {
                            Some(value.as_str())
                        }
                        _ => None,
                    })
                    .unwrap_or("");
                let winner = if (30_000..40_000).contains(&event.kind) {
                    store
                        .get_parameterized_replaceable(Some(root), &event.pubkey, event.kind, d_tag)
                        .await?
                } else {
                    store
                        .get_replaceable(Some(root), &event.pubkey, event.kind)
                        .await?
                }
                .context("forward event has no retained replacement")?;
                let winner = VerifiedStoredNostrEvent::try_from(winner)?.into_stored();
                ensure!(
                    nostr_replaceable_slot(&winner) == nostr_replaceable_slot(event)
                        && compare_nostr_replaceable_events(&winner, event) == Ordering::Greater,
                    "forward replacement is not a newer same-coordinate event"
                );
                let by_id = store
                    .get_verified_by_id(Some(root), &winner.id)
                    .await?
                    .context("forward replacement missing from by-ID index")?;
                ensure!(
                    by_id.as_stored() == &winner,
                    "forward replacement projections differ"
                );
                ("superseded", winner.id)
            }
        };
        rows.push(Disposition {
            id: event.id.clone(),
            status: status.into(),
            retained_id,
        });
    }
    Ok(rows)
}
