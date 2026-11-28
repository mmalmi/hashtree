use anyhow::{ensure, Context, Result};
use hashtree_nostr::{catchup::CatchupState, StoredNostrEvent, VerifiedStoredNostrEvent};
use serde::Deserialize;
use std::collections::BTreeSet;

use super::super::coverage;

pub(super) const MAX_BATCH: usize = 4 * 1024 * 1024;
pub(super) const MAX_GRAPH_AUTHORS: usize = 16 * 1024 * 1024;
pub(super) const KINDS: [u32; 11] = [
    5, 21, 22, 7368, 30402, 30405, 30406, 31555, 34235, 34236, 38383,
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Batch {
    format: String,
    config_sha256: String,
    since: u64,
    events: Vec<serde_json::Value>,
    graph_authors_sha256: Option<String>,
}

pub(super) struct ValidatedBatch {
    pub(super) events: Vec<StoredNostrEvent>,
    pub(super) graph_authors_sha256: Option<String>,
}

pub(super) fn validate(
    bytes: &[u8],
    batch_sha: &str,
    config_sha: &str,
    since: u64,
) -> Result<ValidatedBatch> {
    ensure!(
        bytes.len() <= MAX_BATCH
            && since > 0
            && coverage::is_sha(batch_sha)
            && coverage::is_sha(config_sha),
        "invalid forward batch bounds or digest"
    );
    ensure!(
        coverage::sha(bytes) == batch_sha,
        "forward batch byte digest mismatch"
    );
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    ensure!(
        coverage::canonical(&value)?.as_bytes() == bytes,
        "forward batch must be canonical compact JSON without newline"
    );
    let batch: Batch = serde_json::from_value(value)?;
    ensure!(
        batch.config_sha256 == config_sha && batch.since == since,
        "forward batch configuration differs"
    );
    let graph = match (batch.format.as_str(), batch.graph_authors_sha256.as_deref()) {
        ("iris-social/archive-forward-batch@1", None) => false,
        ("iris-social/archive-forward-batch@2", Some(sha)) if coverage::is_sha(sha) => true,
        _ => anyhow::bail!("invalid forward batch format or graph pin"),
    };
    ensure!(
        !batch.events.is_empty() && batch.events.len() <= 32,
        "forward batch needs 1..32 events"
    );
    let latest = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        .saturating_add(60);
    let mut ids = BTreeSet::new();
    let events = batch
        .events
        .into_iter()
        .map(|value| {
            ensure!(
                coverage::canonical(&value)?.len() <= 128 * 1024,
                "forward event exceeds 128 KiB"
            );
            let event: StoredNostrEvent = serde_json::from_value(value.clone())?;
            ensure!(
                serde_json::to_value(&event)? == value,
                "forward event has unknown or noncanonical fields"
            );
            ensure!(
                (if graph {
                    durable_public(&event)
                } else {
                    KINDS.contains(&event.kind)
                }) && event.created_at >= since
                    && event.created_at <= latest,
                "forward event outside kind or time policy"
            );
            ensure!(
                ids.insert(event.id.clone()),
                "duplicate forward batch event ID"
            );
            let verified = VerifiedStoredNostrEvent::try_from(event.clone())
                .context("verify forward event ID and signature")?;
            ensure!(
                verified.as_stored() == &event,
                "noncanonical forward signed event"
            );
            Ok(event)
        })
        .collect::<Result<_>>()?;
    Ok(ValidatedBatch {
        events,
        graph_authors_sha256: batch.graph_authors_sha256,
    })
}

// Keep the structural privacy policy consistent with the forward collector.
fn durable_public(event: &StoredNostrEvent) -> bool {
    event.kind <= 65_535
        && !(20_000..30_000).contains(&event.kind)
        && !matches!(
            event.kind,
            4 | 13
                | 14
                | 15
                | 44
                | 1059
                | 1060
                | 1234
                | 7375
                | 7376
                | 10013
                | 17375
                | 30024
                | 30064
                | 30078
                | 31234
                | 37368
        )
        && !event.tags.iter().any(|tag| {
            matches!(
                tag.first().map(String::as_str),
                Some("h" | "private" | "encrypted" | "-")
            ) || (event.kind == 9734 && tag.first().map(String::as_str) == Some("anon"))
        })
        && (event.content.is_empty()
            || !matches!(
                event.kind,
                10000
                    | 10001
                    | 10003
                    | 10004
                    | 10005
                    | 10006
                    | 10007
                    | 10009
                    | 10012
                    | 10015
                    | 10017
                    | 10018
                    | 10020
                    | 10021
                    | 10030
                    | 10050
                    | 10054
                    | 10063
                    | 10064
                    | 10101
                    | 10102
                    | 10992
                    | 30000
                    | 30002
                    | 30003
                    | 30004
                    | 30005
                    | 30006
                    | 30007
                    | 30008
                    | 30015
                    | 30030
                    | 30063
            ))
}

pub(super) fn verify_graph_authors(
    batch: &ValidatedBatch,
    bytes: &[u8],
    pinned: &str,
) -> Result<()> {
    ensure!(
        batch.graph_authors_sha256.as_deref() == Some(pinned) && coverage::sha(bytes) == pinned,
        "forward graph authors digest differs"
    );
    ensure!(
        bytes.ends_with(b"\n") && !bytes.contains(&b'\r'),
        "forward graph rows must use LF"
    );
    let mut previous = None;
    let mut authors = BTreeSet::new();
    for row in std::str::from_utf8(bytes)?.lines() {
        let (author, raw_distance) = row
            .split_once('\t')
            .context("invalid forward graph author row")?;
        let distance = raw_distance.parse::<u16>()?;
        ensure!(
            coverage::is_sha(author)
                && distance < 1_000
                && distance.to_string() == raw_distance
                && previous.is_none_or(|before| before < (distance, author))
                && authors.insert(author),
            "invalid or duplicate forward graph author"
        );
        previous = Some((distance, author));
    }
    ensure!(!authors.is_empty(), "empty forward author graph");
    ensure!(
        batch
            .events
            .iter()
            .all(|event| KINDS.contains(&event.kind) || authors.contains(event.pubkey.as_str())),
        "forward event author is outside its admitted graph"
    );
    Ok(())
}

pub(super) fn authors(bytes: &[u8], state: &CatchupState) -> Result<Vec<String>> {
    ensure!(
        bytes.len() <= 64 * 1024 * 1024 && coverage::sha(bytes) == state.policy.authors_sha256,
        "forward historical authors digest differs"
    );
    let authors = std::str::from_utf8(bytes)?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut unique = BTreeSet::new();
    ensure!(
        authors.len() == state.policy.author_count
            && authors
                .iter()
                .all(|author| coverage::is_sha(author) && unique.insert(author)),
        "forward historical authors differ"
    );
    // Validate the saved state without preparing a new pass or changing policy.
    let checked = CatchupState::prepare(
        Some(state.clone()),
        state.policy.clone(),
        Some(state.pass_until),
        state.pass_until,
    )?;
    ensure!(checked == *state, "invalid forward historical checkpoint");
    Ok(authors)
}
