use std::collections::BTreeMap;

use hashtree_nostr::catchup::{
    fetch_catchup_author_with_coverage, CatchupError, CatchupPolicy, CatchupQuery,
    CatchupRunSources, CatchupSource, CatchupSourceMode, CatchupSourceStatus, CatchupState, Result,
};
use hashtree_nostr::{stored_event_from_nostr_sdk_event, StoredNostrEvent};
use nostr::{EventBuilder, Keys, Kind, Timestamp};

fn policy() -> CatchupPolicy {
    CatchupPolicy {
        base_root: "retained-root".into(),
        authors_sha256: "authors".into(),
        author_count: 2,
        initial_since: 10,
        overlap_secs: 10,
        relays: vec!["bad".into(), "good".into()],
        pubsub_peers: Vec::new(),
        source_mode: CatchupSourceMode::BestEffort,
        kinds: vec![1, 5],
        page_size: 4,
        max_pages_per_author: 100,
        max_events_per_author: 100,
        max_bytes_per_author: 1024 * 1024,
        fetch_timeout_secs: 1,
        index_commit_batch_size: 4,
    }
}
fn keys() -> Keys {
    Keys::parse(&format!("{:064x}", 1)).unwrap()
}
fn event(at: u64) -> StoredNostrEvent {
    stored_event_from_nostr_sdk_event(
        &EventBuilder::new(Kind::TextNote, format!("signed fixture {at}"))
            .custom_created_at(Timestamp::from_secs(at))
            .sign_with_keys(&keys())
            .unwrap(),
    )
}
#[derive(Default)]
struct Source {
    events: BTreeMap<String, Vec<StoredNostrEvent>>,
    fail_after: BTreeMap<String, usize>,
    calls: BTreeMap<String, usize>,
}
impl CatchupSource for Source {
    async fn query(&mut self, relay: &str, query: &CatchupQuery) -> Result<Vec<StoredNostrEvent>> {
        let calls = self.calls.entry(relay.into()).or_default();
        *calls += 1;
        if self.fail_after.get(relay).is_some_and(|n| *calls > *n) {
            return Err(CatchupError("source unavailable".repeat(100)));
        }
        let mut events = self
            .events
            .get(relay)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.created_at >= query.since && e.created_at <= query.until)
            .collect::<Vec<_>>();
        events.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        events.truncate(query.limit);
        Ok(events)
    }
}
async fn fetch(
    source: &mut Source,
    run: &CatchupRunSources,
    p: &CatchupPolicy,
) -> Result<hashtree_nostr::catchup::CatchupAuthorResult> {
    fetch_catchup_author_with_coverage(source, p, run, &keys().public_key().to_hex(), 10, 100).await
}
#[tokio::test]
async fn failed_source_does_not_block_signed_healthy_events_or_retry_each_author() {
    let mut source = Source::default();
    source.fail_after.insert("bad".into(), 0);
    source.events.insert("good".into(), vec![event(20)]);
    let run = CatchupRunSources::default();
    let first = fetch(&mut source, &run, &policy()).await.unwrap();
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.sources[0].status, CatchupSourceStatus::Failed);
    assert_eq!(
        first.sources[0].error.as_ref().unwrap().chars().count(),
        256
    );
    let second = fetch(&mut source, &run.clone(), &policy()).await.unwrap();
    assert_eq!(second.sources[0].status, CatchupSourceStatus::Skipped);
    assert_eq!(source.calls["bad"], 1);
    assert_eq!(second.events[0].id, event(20).id);
}
#[tokio::test]
async fn valid_earlier_pages_from_later_failed_source_are_discarded() {
    let mut source = Source::default();
    source.fail_after.insert("bad".into(), 1);
    source.events.insert("bad".into(), vec![event(40)]);
    source.events.insert("good".into(), vec![event(20)]);
    let result = fetch(&mut source, &CatchupRunSources::default(), &policy())
        .await
        .unwrap();
    assert_eq!(
        result.events.iter().map(|e| &e.id).collect::<Vec<_>>(),
        vec![&event(20).id]
    );
    assert_eq!(result.sources[0].status, CatchupSourceStatus::Failed);
}
#[tokio::test]
async fn invalid_filter_response_is_a_missing_source_not_accepted_data() {
    let mut source = Source::default();
    let mut wrong = event(40);
    wrong.pubkey = "f".repeat(64);
    source.events.insert("bad".into(), vec![wrong]);
    source.events.insert("good".into(), vec![event(20)]);
    let result = fetch(&mut source, &CatchupRunSources::default(), &policy())
        .await
        .unwrap();
    assert_eq!(result.events.len(), 1);
    assert_eq!(result.sources[0].status, CatchupSourceStatus::Failed);
}
#[tokio::test]
async fn all_failed_or_quarantined_sources_return_error_without_more_queries() {
    let mut source = Source::default();
    source
        .fail_after
        .extend([("bad".into(), 0), ("good".into(), 0)]);
    let run = CatchupRunSources::default();
    assert!(fetch(&mut source, &run, &policy()).await.is_err());
    let calls = source.calls.clone();
    assert!(fetch(&mut source, &run, &policy()).await.is_err());
    assert_eq!(source.calls, calls);
    assert!(fetch(&mut source, &CatchupRunSources::default(), &policy())
        .await
        .is_err());
    assert_eq!(source.calls["bad"], 2);
    assert_eq!(source.calls["good"], 2);
}
#[tokio::test]
async fn completed_empty_source_is_valid_and_failed_source_cannot_spend_its_budget() {
    let mut source = Source::default();
    source.events.insert("bad".into(), vec![event(40)]);
    let mut p = policy();
    p.max_pages_per_author = 1;
    let result = fetch(&mut source, &CatchupRunSources::default(), &p)
        .await
        .unwrap();
    assert!(result.events.is_empty());
    assert_eq!(result.sources[0].status, CatchupSourceStatus::Failed);
    assert_eq!(result.sources[1].status, CatchupSourceStatus::Complete);
}
#[tokio::test]
async fn completed_source_union_preserves_author_memory_bound_and_deduplicates() {
    let mut source = Source::default();
    source.events.insert("bad".into(), vec![event(20)]);
    source.events.insert("good".into(), vec![event(20)]);
    let mut p = policy();
    p.max_events_per_author = 1;
    assert_eq!(
        fetch(&mut source, &CatchupRunSources::default(), &p)
            .await
            .unwrap()
            .events
            .len(),
        1
    );
    source.events.insert("good".into(), vec![event(40)]);
    assert!(fetch(&mut source, &CatchupRunSources::default(), &p)
        .await
        .unwrap_err()
        .to_string()
        .contains("combined completed"));
}
#[test]
fn strict_legacy_serialization_and_policy_resume_identity_are_preserved() {
    let mut strict = policy();
    strict.source_mode = CatchupSourceMode::Strict;
    let saved = CatchupState::prepare(None, strict.clone(), Some(100), 100).unwrap();
    let raw = serde_json::to_value(&saved).unwrap();
    assert!(raw["policy"].get("source_mode").is_none());
    assert!(raw.get("coverage_head").is_none());
    assert_eq!(serde_json::from_value::<CatchupState>(raw).unwrap(), saved);
    assert!(CatchupState::prepare(Some(saved), policy(), Some(100), 100).is_err());
    let mut best = CatchupState::prepare(None, policy(), Some(100), 100).unwrap();
    let mut increased = policy();
    increased.max_pages_per_author += 1;
    assert!(CatchupState::prepare(Some(best.clone()), increased, Some(100), 100).is_err());
    best.coverage_head = Some("not-a-digest".into());
    assert!(CatchupState::prepare(Some(best), policy(), Some(100), 100).is_err());
}
