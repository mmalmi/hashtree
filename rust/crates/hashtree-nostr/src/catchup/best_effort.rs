//! Optional source-relative availability policy. Failed partial sources never
//! contribute events, and a run quarantines failed sources across both workers.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::{
    fetch_catchup_author, CatchupError, CatchupPolicy, CatchupSource, CatchupSourceMode, Result,
};
use crate::StoredNostrEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatchupSourceStatus {
    Complete,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatchupSourceCoverage {
    pub relay: String,
    pub status: CatchupSourceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct CatchupAuthorResult {
    pub events: Vec<StoredNostrEvent>,
    pub sources: Vec<CatchupSourceCoverage>,
}

/// No clocks or persistent blacklist: each native invocation retries sources.
/// At most the two already-running author fetches can observe the first error
/// concurrently. Later authors skip that source without another network wait.
#[derive(Clone, Default)]
pub struct CatchupRunSources(Arc<Mutex<BTreeMap<String, String>>>);

impl CatchupRunSources {
    fn reason(&self, relay: &str) -> Option<String> {
        self.0
            .lock()
            .expect("catchup source lock poisoned")
            .get(relay)
            .cloned()
    }
    fn fail(&self, relay: &str, error: String) {
        self.0
            .lock()
            .expect("catchup source lock poisoned")
            .entry(relay.to_owned())
            .or_insert(error);
    }
}

pub async fn fetch_catchup_author_with_coverage(
    source: &mut impl CatchupSource,
    policy: &CatchupPolicy,
    run: &CatchupRunSources,
    author: &str,
    since: u64,
    until: u64,
) -> Result<CatchupAuthorResult> {
    if policy.source_mode == CatchupSourceMode::Strict {
        return Ok(CatchupAuthorResult {
            events: fetch_catchup_author(source, policy, author, since, until).await?,
            sources: policy
                .relays
                .iter()
                .map(|relay| CatchupSourceCoverage {
                    relay: relay.clone(),
                    status: CatchupSourceStatus::Complete,
                    error: None,
                })
                .collect(),
        });
    }
    let mut accepted = BTreeMap::new();
    let mut accepted_bytes = 0usize;
    let mut sources = Vec::with_capacity(policy.relays.len());
    for relay in &policy.relays {
        if let Some(error) = run.reason(relay) {
            sources.push(CatchupSourceCoverage {
                relay: relay.clone(),
                status: CatchupSourceStatus::Skipped,
                error: Some(error),
            });
            continue;
        }
        let mut single = policy.clone();
        single.relays = vec![relay.clone()];
        // A failed source has its own bounded attempt. It cannot spend the
        // healthy sources' page/event budget. Total fetch work remains bounded
        // by configured sources times the per-author policy limits.
        match fetch_catchup_author(source, &single, author, since, until).await {
            Ok(events) => {
                for event in events {
                    if accepted.contains_key(&event.id) {
                        continue;
                    }
                    accepted_bytes = accepted_bytes.saturating_add(
                        serde_json::to_vec(&event)
                            .map_err(|error| CatchupError(error.to_string()))?
                            .len(),
                    );
                    if accepted.len() >= policy.max_events_per_author
                        || accepted_bytes > policy.max_bytes_per_author
                    {
                        return Err(CatchupError(
                            "combined completed sources exceed the author event/byte budget".into(),
                        ));
                    }
                    accepted.insert(event.id.clone(), event);
                }
                sources.push(CatchupSourceCoverage {
                    relay: relay.clone(),
                    status: CatchupSourceStatus::Complete,
                    error: None,
                });
            }
            Err(error) => {
                let error = error.to_string().chars().take(256).collect::<String>();
                run.fail(relay, error.clone());
                sources.push(CatchupSourceCoverage {
                    relay: relay.clone(),
                    status: CatchupSourceStatus::Failed,
                    error: Some(error),
                });
            }
        }
    }
    if !sources
        .iter()
        .any(|source| source.status == CatchupSourceStatus::Complete)
    {
        return Err(CatchupError(format!(
            "author {author}: no source completed; checkpoint unchanged; sources={}",
            serde_json::to_string(&sources).map_err(|error| CatchupError(error.to_string()))?
        )));
    }
    Ok(CatchupAuthorResult {
        events: accepted.into_values().collect(),
        sources,
    })
}
