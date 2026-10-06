//! Bounded temporal catchup into an existing event index.
//!
//! Coverage is source-relative. Strict mode requires every configured relay;
//! optional best-effort mode records missing sources and requires at least one
//! completed relay per author. EOSE never claims global completeness.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::StoredNostrEvent;

mod best_effort;
pub use best_effort::{
    fetch_catchup_author_with_coverage, CatchupAuthorResult, CatchupRunSources,
    CatchupSourceCoverage, CatchupSourceStatus,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatchupSourceMode {
    #[default]
    Strict,
    BestEffort,
}
impl CatchupSourceMode {
    fn is_strict(&self) -> bool {
        *self == Self::Strict
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CatchupError(pub String);

pub type Result<T> = std::result::Result<T, CatchupError>;

pub const DEFAULT_CATCHUP_OVERLAP_SECS: u64 = 86_400;

fn default_overlap_secs() -> u64 {
    DEFAULT_CATCHUP_OVERLAP_SECS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatchupPolicy {
    pub base_root: String,
    pub authors_sha256: String,
    pub author_count: usize,
    pub initial_since: u64,
    #[serde(default = "default_overlap_secs")]
    pub overlap_secs: u64,
    pub relays: Vec<String>,
    #[serde(default, skip_serializing_if = "CatchupSourceMode::is_strict")]
    pub source_mode: CatchupSourceMode,
    pub kinds: Vec<u16>,
    pub page_size: usize,
    pub max_pages_per_author: usize,
    pub max_events_per_author: usize,
    pub max_bytes_per_author: usize,
    pub fetch_timeout_secs: u64,
    pub index_commit_batch_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatchupState {
    pub version: u32,
    pub policy: CatchupPolicy,
    pub root: String,
    pub pass_since: u64,
    pub pass_until: u64,
    pub next_author: usize,
    pub events_received: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_head: Option<String>,
}

impl CatchupState {
    /// Resume the exact captured window, or start the next window only after
    /// every author satisfied the configured source policy in the previous one.
    pub fn prepare(
        saved: Option<Self>,
        policy: CatchupPolicy,
        requested_until: Option<u64>,
        now: u64,
    ) -> Result<Self> {
        if policy.source_mode == CatchupSourceMode::BestEffort && policy.relays.len() > 64 {
            return Err(CatchupError(
                "best-effort catchup supports at most 64 sources".into(),
            ));
        }
        if policy.author_count == 0
            || policy.relays.is_empty()
            || policy.kinds.is_empty()
            || policy.page_size == 0
            || policy.max_pages_per_author == 0
            || policy.max_events_per_author == 0
            || policy.max_bytes_per_author == 0
            || policy.fetch_timeout_secs == 0
            || policy.index_commit_batch_size == 0
        {
            return Err(CatchupError(
                "catchup inputs and limits must be nonempty".into(),
            ));
        }
        if let Some(mut state) = saved {
            let previous = &state.policy;
            if previous.source_mode == CatchupSourceMode::BestEffort && *previous != policy {
                return Err(CatchupError(
                    "best-effort coverage requires exact policy; explicit migration required"
                        .into(),
                ));
            }
            if policy.overlap_secs < previous.overlap_secs
                || policy.page_size < previous.page_size
                || policy.max_pages_per_author < previous.max_pages_per_author
                || policy.max_events_per_author < previous.max_events_per_author
                || policy.max_bytes_per_author < previous.max_bytes_per_author
                || policy.fetch_timeout_secs < previous.fetch_timeout_secs
            {
                return Err(CatchupError(
                    "catchup resume may increase resource bounds, not decrease them".into(),
                ));
            }
            let mut identity = previous.clone();
            identity.overlap_secs = policy.overlap_secs;
            identity.page_size = policy.page_size;
            identity.max_pages_per_author = policy.max_pages_per_author;
            identity.max_events_per_author = policy.max_events_per_author;
            identity.max_bytes_per_author = policy.max_bytes_per_author;
            identity.fetch_timeout_secs = policy.fetch_timeout_secs;
            identity.index_commit_batch_size = policy.index_commit_batch_size;
            if state.version != 2 || identity != policy {
                return Err(CatchupError(
                    "catchup policy changed; refusing to reuse coverage".into(),
                ));
            }
            if state.coverage_head.as_ref().is_some_and(|head| {
                head.len() != 64
                    || head
                        .bytes()
                        .any(|b| !b.is_ascii_digit() && !(b'a'..=b'f').contains(&b))
            })
            {
                return Err(CatchupError("invalid catchup coverage head".into()));
            }
            state.policy = policy.clone();
            if state.next_author > policy.author_count
                || state.pass_since < policy.initial_since
                || state.pass_since > state.pass_until
                || state.pass_until > now
            {
                return Err(CatchupError("invalid catchup checkpoint boundaries".into()));
            }
            if !state.complete() {
                if requested_until.is_some_and(|until| until != state.pass_until) {
                    return Err(CatchupError(
                        "unfinished catchup must retain its fixed until".into(),
                    ));
                }
                return Ok(state);
            }
            let until = requested_until.unwrap_or(now);
            if until < state.pass_until || until > now {
                return Err(CatchupError(
                    "next catchup until must be monotonic and not in the future".into(),
                ));
            }
            if until > state.pass_until {
                // Revisit a bounded interval for late relay arrivals, retaining
                // the complete accumulated index and original migration floor.
                state.pass_since = state
                    .pass_until
                    .saturating_sub(policy.overlap_secs)
                    .max(policy.initial_since);
                state.pass_until = until;
                state.next_author = 0;
            }
            Ok(state)
        } else {
            let until = requested_until.unwrap_or(now);
            if policy.initial_since > until || until > now {
                return Err(CatchupError(
                    "catchup requires since <= until <= now".into(),
                ));
            }
            Ok(Self {
                version: 2,
                root: policy.base_root.clone(),
                pass_since: policy.initial_since,
                pass_until: until,
                policy,
                next_author: 0,
                events_received: 0,
                coverage_head: None,
            })
        }
    }

    pub fn complete(&self) -> bool {
        self.next_author == self.policy.author_count
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchupQuery {
    pub author: String,
    pub kinds: Vec<u16>,
    pub since: u64,
    pub until: u64,
    pub limit: usize,
}

/// Queries must reject missing EOSE, closed sockets, invalid events and size
/// limits. Returning an empty vector means a completed empty subscription.
#[allow(async_fn_in_trait)]
pub trait CatchupSource {
    async fn query(&mut self, relay: &str, query: &CatchupQuery) -> Result<Vec<StoredNostrEvent>>;

    /// Largest unique, verified EOSE page observed from this relay under the
    /// supplied pass interval, kinds and limit. May omit the author in at most
    /// one bounded probe; probe events must not enter the author's results.
    /// Zero means unavailable. Failures must remain errors, not capacity zero.
    /// This is an observed uniform-cap heuristic, not proof against a source
    /// that applies different caps to different filters or hides events.
    async fn observed_capacity(&mut self, _relay: &str, _query: &CatchupQuery) -> Result<usize> {
        Ok(0)
    }

    /// An unbounded-by-limit count for the exact filter, when the source can
    /// supply one without marking it approximate. None means unavailable.
    /// Like EOSE, this is evidence from that source, not global completeness.
    async fn exact_count(&mut self, _relay: &str, _query: &CatchupQuery) -> Result<Option<usize>> {
        Ok(None)
    }
}

/// Fetch one author's full missing interval from all required relays. No
/// historical index scan is needed; the event writer deduplicates existing IDs.
pub async fn fetch_catchup_author(
    source: &mut impl CatchupSource,
    policy: &CatchupPolicy,
    author: &str,
    since: u64,
    until: u64,
) -> Result<Vec<StoredNostrEvent>> {
    let mut accepted = BTreeMap::new();
    let mut bytes = 0usize;
    let mut pages = 0usize;
    for relay in &policy.relays {
        // Probe the oldest returned second independently before moving below
        // it. Never subtract one from an unexamined same-second boundary.
        let mut pending = vec![(since, until)];
        let mut observed_broad_capacity = 0;
        while let Some((lower, upper)) = pending.pop() {
            if pages >= policy.max_pages_per_author {
                return Err(CatchupError(format!(
                    "author {author}, relay {relay}: page budget exhausted; coverage incomplete"
                )));
            }
            pages += 1;
            let query = CatchupQuery {
                author: author.to_owned(),
                kinds: policy.kinds.clone(),
                since: lower,
                until: upper,
                limit: policy.page_size,
            };
            let events = source.query(relay, &query).await.map_err(|err| {
                CatchupError(format!(
                    "author {author}, relay {relay}, interval {lower}..{upper}: {err}"
                ))
            })?;
            if events.len() > policy.page_size {
                return Err(CatchupError(format!(
                    "author {author}, relay {relay}: source exceeded requested page limit"
                )));
            }
            if lower != upper {
                // A short final page does not lower capacity already observed
                // from this same relay and author during this fetch.
                observed_broad_capacity = observed_broad_capacity.max(events.len());
            }
            let saturated = events.len() >= policy.page_size;
            if lower == upper
                && (saturated || (events.len() > 1 && events.len() >= observed_broad_capacity))
            {
                let unique = events
                    .iter()
                    .map(|event| &event.id)
                    .collect::<BTreeSet<_>>()
                    .len();
                if !saturated {
                    // An author's complete history can be a small tie. Its
                    // size alone is not an observed relay cap. A larger page
                    // from this same source disproves a uniform cap that low.
                    if pages >= policy.max_pages_per_author {
                        return Err(CatchupError(format!(
                            "author {author}, relay {relay}: page budget exhausted before capacity probe; coverage incomplete"
                        )));
                    }
                    pages += 1;
                    let capacity_query = CatchupQuery {
                        since,
                        until,
                        ..query.clone()
                    };
                    let capacity = source
                        .observed_capacity(relay, &capacity_query)
                        .await
                        .map_err(|err| {
                            CatchupError(format!(
                                "author {author}, relay {relay}, capacity probe: {err}"
                            ))
                        })?;
                    if capacity > policy.page_size {
                        return Err(CatchupError(format!(
                            "author {author}, relay {relay}: capacity exceeded requested limit"
                        )));
                    }
                    observed_broad_capacity = observed_broad_capacity.max(capacity);
                }
                if saturated || unique >= observed_broad_capacity {
                    // Full requested-page ties always need an unbounded count.
                    // Both probes and counts share the author's page budget,
                    // including calls served from a source's capacity cache.
                    if pages >= policy.max_pages_per_author {
                        return Err(CatchupError(format!(
                            "author {author}, relay {relay}: page budget exhausted before timestamp count; coverage incomplete"
                        )));
                    }
                    pages += 1;
                    let count = source.exact_count(relay, &query).await.map_err(|err| {
                        CatchupError(format!(
                            "author {author}, relay {relay}, timestamp {lower} count: {err}"
                        ))
                    })?;
                    if count != Some(unique) {
                        let reason = if saturated {
                            "saturated"
                        } else {
                            "ambiguous capped"
                        };
                        return Err(CatchupError(format!("author {author}, relay {relay}: {reason} timestamp {lower}; coverage incomplete")));
                    }
                }
            }
            let oldest = events.iter().map(|event| event.created_at).min();
            for event in events {
                if event.pubkey != author
                    || event.created_at < lower
                    || event.created_at > upper
                    || !policy
                        .kinds
                        .iter()
                        .any(|kind| u32::from(*kind) == event.kind)
                {
                    return Err(CatchupError(format!(
                        "author {author}, relay {relay}: event outside requested filter"
                    )));
                }
                if !accepted.contains_key(&event.id) {
                    bytes = bytes.saturating_add(
                        serde_json::to_vec(&event)
                            .map_err(|err| CatchupError(err.to_string()))?
                            .len(),
                    );
                    if accepted.len() >= policy.max_events_per_author
                        || bytes > policy.max_bytes_per_author
                    {
                        return Err(CatchupError(format!("author {author}, relay {relay}: event/byte budget exhausted; coverage incomplete")));
                    }
                    accepted.insert(event.id.clone(), event);
                }
            }
            if lower != upper {
                if let Some(oldest) = oldest {
                    // Even short pages can reflect a relay-side cap. Continue
                    // into the older interval instead of assuming exhaustion.
                    if oldest > lower {
                        pending.push((lower, oldest - 1));
                    }
                    pending.push((oldest, oldest));
                }
            }
        }
    }
    Ok(accepted.into_values().collect())
}
