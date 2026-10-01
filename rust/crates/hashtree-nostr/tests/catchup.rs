use std::collections::BTreeMap;
use std::sync::Arc;

use hashtree_core::MemoryStore;
use hashtree_nostr::catchup::{
    fetch_catchup_author, CatchupError, CatchupPolicy, CatchupQuery, CatchupSource, CatchupState,
    Result,
};
use hashtree_nostr::{
    stored_event_from_nostr_sdk_event, ListEventsOptions, NostrEventStore, StoredNostrEvent,
};
use nostr::{EventBuilder, Keys, Kind, Timestamp};

fn policy() -> CatchupPolicy {
    CatchupPolicy {
        base_root: "exact-original-root".into(),
        authors_sha256: "ordered-authors".into(),
        author_count: 2,
        initial_since: 10,
        overlap_secs: 20,
        relays: vec!["relay-a".into(), "relay-b".into()],
        kinds: vec![1, 5],
        page_size: 4,
        max_pages_per_author: 100,
        max_events_per_author: 100,
        max_bytes_per_author: 1024 * 1024,
        fetch_timeout_secs: 1,
        index_commit_batch_size: 4,
    }
}

fn event(keys: &Keys, timestamp: u64, label: &str, kind: Kind) -> StoredNostrEvent {
    stored_event_from_nostr_sdk_event(
        &EventBuilder::new(kind, label)
            .custom_created_at(Timestamp::from_secs(timestamp))
            .sign_with_keys(keys)
            .unwrap(),
    )
}

#[derive(Default)]
struct Source {
    events: BTreeMap<String, Vec<StoredNostrEvent>>,
    cap: Option<usize>,
    failed_relay: Option<String>,
    requests: Vec<CatchupQuery>,
}

impl CatchupSource for Source {
    async fn query(&mut self, relay: &str, query: &CatchupQuery) -> Result<Vec<StoredNostrEvent>> {
        self.requests.push(query.clone());
        if self.failed_relay.as_deref() == Some(relay) {
            return Err(CatchupError("connection ended before EOSE".into()));
        }
        let mut events = self
            .events
            .get(relay)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|event| {
                event.pubkey == query.author
                    && event.created_at >= query.since
                    && event.created_at <= query.until
            })
            .collect::<Vec<_>>();
        events.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        events.truncate(query.limit.min(self.cap.unwrap_or(usize::MAX)));
        Ok(events)
    }
}

#[test]
fn resumes_exact_unfinished_interval_and_continues_completed_frontier() {
    let mut state = CatchupState::prepare(None, policy(), Some(100), 100).unwrap();
    state.next_author = 1;
    state.root = "committed-root".into();
    let resumed = CatchupState::prepare(Some(state.clone()), policy(), None, 200).unwrap();
    assert_eq!(resumed, state);
    assert!(CatchupState::prepare(Some(state.clone()), policy(), Some(200), 200).is_err());
    state.next_author = 2;
    let next = CatchupState::prepare(Some(state), policy(), None, 200).unwrap();
    assert_eq!(next.root, "committed-root");
    assert_eq!(
        (next.pass_since, next.pass_until, next.next_author),
        (80, 200, 0)
    );
}

#[test]
fn rejects_changed_identity_corrupt_frontier_and_future_windows() {
    let state = CatchupState::prepare(None, policy(), Some(100), 100).unwrap();
    let mut changed = policy();
    changed.relays.push("relay-c".into());
    assert!(CatchupState::prepare(Some(state.clone()), changed, None, 100).is_err());
    let mut corrupt = state;
    corrupt.next_author = 3;
    assert!(CatchupState::prepare(Some(corrupt), policy(), None, 100).is_err());
    assert!(CatchupState::prepare(None, policy(), Some(101), 100).is_err());
    assert!(CatchupState::prepare(None, policy(), Some(9), 100).is_err());
}

#[test]
fn resumes_failed_author_with_larger_operational_bounds() {
    let mut state = CatchupState::prepare(None, policy(), Some(100), 100).unwrap();
    state.next_author = 1;
    state.root = "first-author-durable-root".into();
    let mut larger = policy();
    larger.overlap_secs *= 2;
    larger.page_size *= 2;
    larger.max_pages_per_author *= 2;
    larger.max_events_per_author *= 2;
    larger.max_bytes_per_author *= 2;
    larger.fetch_timeout_secs *= 2;
    larger.index_commit_batch_size = 1;
    let resumed = CatchupState::prepare(Some(state), larger.clone(), None, 200).unwrap();
    assert_eq!(resumed.policy, larger);
    assert_eq!(resumed.next_author, 1);
    assert_eq!(resumed.root, "first-author-durable-root");
    assert_eq!(resumed.pass_until, 100);
    assert!(CatchupState::prepare(Some(resumed), policy(), None, 200).is_err());
}

#[test]
fn overlap_respects_initial_floor_and_defaults_older_candidate_checkpoints() {
    let mut state = CatchupState::prepare(None, policy(), Some(25), 25).unwrap();
    state.next_author = state.policy.author_count;
    let next = CatchupState::prepare(Some(state.clone()), policy(), Some(100), 100).unwrap();
    assert_eq!(next.pass_since, 10);
    let mut serialized = serde_json::to_value(state).unwrap();
    serialized["policy"]
        .as_object_mut()
        .unwrap()
        .remove("overlap_secs");
    let restored: CatchupState = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored.policy.overlap_secs, 86_400);
    let next =
        CatchupState::prepare(Some(restored.clone()), restored.policy, Some(100), 100).unwrap();
    assert_eq!(next.pass_since, 10);
}

#[tokio::test]
async fn overlapping_pass_collects_late_arrivals_and_deduplicates_retained_history() {
    let keys = Keys::generate();
    let author = keys.public_key().to_hex();
    let old = event(&keys, 1, "old archive", Kind::TextNote);
    let already_indexed = event(&keys, 90, "previous pass", Kind::TextNote);
    let late = event(&keys, 95, "arrived after previous EOSE", Kind::TextNote);
    let new = event(&keys, 150, "next pass", Kind::TextNote);
    let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
    let root = store
        .build(None, [old.clone(), already_indexed.clone()])
        .await
        .unwrap()
        .unwrap();
    let mut state = CatchupState::prepare(None, policy(), Some(100), 100).unwrap();
    state.next_author = state.policy.author_count;
    let next = CatchupState::prepare(Some(state), policy(), Some(200), 200).unwrap();
    assert_eq!((next.pass_since, next.pass_until), (80, 200));
    let mut source = Source::default();
    source.events.insert(
        "relay-a".into(),
        vec![already_indexed.clone(), late.clone(), new.clone()],
    );
    let incoming = fetch_catchup_author(
        &mut source,
        &next.policy,
        &author,
        next.pass_since,
        next.pass_until,
    )
    .await
    .unwrap();
    let next_root = store
        .build(Some(&root), incoming.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.build(Some(&next_root), incoming).await.unwrap(),
        Some(next_root.clone())
    );
    let events = store
        .list_by_author(Some(&next_root), &author, ListEventsOptions::default())
        .await
        .unwrap();
    assert_eq!(events.len(), 4);
    for expected in [old.clone(), already_indexed, late, new] {
        assert!(events.iter().any(|event| event.id == expected.id));
    }
    assert_eq!(
        store.get_by_id(Some(&root), &old.id).await.unwrap(),
        Some(old)
    );
}

#[tokio::test]
async fn catches_full_gap_with_lower_relay_cap_duplicates_and_boundary_ties() {
    let keys = Keys::generate();
    let author = keys.public_key().to_hex();
    let events = vec![
        event(&keys, 10, "inclusive beginning", Kind::TextNote),
        event(&keys, 20, "same second a", Kind::TextNote),
        event(&keys, 20, "same second b", Kind::TextNote),
        event(&keys, 30, "middle", Kind::TextNote),
        event(&keys, 1_000_000, "inclusive end", Kind::TextNote),
    ];
    let mut source = Source {
        cap: Some(3),
        ..Default::default()
    };
    source.events.insert("relay-a".into(), events.clone());
    source.events.insert("relay-b".into(), events.clone());
    let fetched = fetch_catchup_author(&mut source, &policy(), &author, 10, 1_000_000)
        .await
        .unwrap();
    assert_eq!(fetched.len(), events.len());
    for expected in events {
        assert!(fetched.iter().any(|event| event.id == expected.id));
    }
    assert!(source
        .requests
        .iter()
        .any(|query| query.since == 20 && query.until == 20));
    assert!(source
        .requests
        .iter()
        .all(|query| query.since >= 10 && query.until <= 1_000_000));
}

#[tokio::test]
async fn earlier_full_page_proves_capacity_for_short_same_second_tail() {
    let keys = Keys::generate();
    let author = keys.public_key().to_hex();
    let mut events = vec![
        event(&keys, 20, "tail a", Kind::TextNote),
        event(&keys, 20, "tail b", Kind::EventDeletion),
    ];
    events.extend(
        (30..=60)
            .step_by(10)
            .map(|at| event(&keys, at, "newer", Kind::TextNote)),
    );
    let mut source = Source::default();
    for relay in ["relay-a", "relay-b"] {
        source.events.insert(relay.into(), events.clone());
    }
    let fetched = fetch_catchup_author(&mut source, &policy(), &author, 10, 100)
        .await
        .unwrap();
    assert_eq!(fetched.len(), events.len());
    assert!(source
        .requests
        .iter()
        .any(|query| query.since == 20 && query.until == 20));
}

#[tokio::test]
async fn observed_capacity_is_not_shared_between_relays_or_authors() {
    let keys = Keys::generate();
    let other = Keys::generate();
    let mut source = Source::default();
    let mut full_page = (30..=60)
        .step_by(10)
        .map(|at| event(&keys, at, "newer", Kind::TextNote))
        .collect::<Vec<_>>();
    full_page.extend([
        event(&other, 20, "other a", Kind::TextNote),
        event(&other, 20, "other b", Kind::TextNote),
    ]);
    source.events.insert("relay-a".into(), full_page);
    source.events.insert(
        "relay-b".into(),
        vec![
            event(&keys, 20, "tail a", Kind::TextNote),
            event(&keys, 20, "tail b", Kind::TextNote),
        ],
    );
    let error = fetch_catchup_author(&mut source, &policy(), &keys.public_key().to_hex(), 10, 100)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("relay-b: ambiguous capped timestamp 20"));
    let error = fetch_catchup_author(
        &mut source,
        &policy(),
        &other.public_key().to_hex(),
        10,
        100,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("relay-a: ambiguous capped timestamp 20"));
}

#[tokio::test]
async fn saturated_single_second_is_incomplete_instead_of_skipping_ids() {
    let keys = Keys::generate();
    let mut source = Source::default();
    source.events.insert(
        "relay-a".into(),
        (0..5)
            .map(|i| event(&keys, 20, &i.to_string(), Kind::TextNote))
            .collect(),
    );
    let error = fetch_catchup_author(&mut source, &policy(), &keys.public_key().to_hex(), 10, 100)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("saturated timestamp 20"));
}

#[tokio::test]
async fn undisclosed_lower_cap_with_hidden_same_second_ids_is_incomplete() {
    let keys = Keys::generate();
    let mut source = Source {
        cap: Some(2),
        ..Default::default()
    };
    source.events.insert(
        "relay-a".into(),
        (0..3)
            .map(|i| event(&keys, 20, &i.to_string(), Kind::TextNote))
            .collect(),
    );
    let error = fetch_catchup_author(&mut source, &policy(), &keys.public_key().to_hex(), 10, 100)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("ambiguous capped timestamp 20"));
    assert!(error.to_string().contains("relay-a"));
}

#[tokio::test]
async fn any_required_source_failure_and_all_resource_caps_are_incomplete() {
    let keys = Keys::generate();
    let author = keys.public_key().to_hex();
    let events = vec![
        event(&keys, 20, "a", Kind::TextNote),
        event(&keys, 30, "b", Kind::TextNote),
    ];
    let mut source = Source {
        failed_relay: Some("relay-b".into()),
        ..Default::default()
    };
    source.events.insert("relay-a".into(), events.clone());
    let error = fetch_catchup_author(&mut source, &policy(), &author, 10, 100)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("relay-b"));
    source.failed_relay = None;
    for resource in ["pages", "events", "bytes"] {
        let mut limited = policy();
        match resource {
            "pages" => limited.max_pages_per_author = 1,
            "events" => limited.max_events_per_author = 1,
            _ => limited.max_bytes_per_author = 1,
        }
        assert!(
            fetch_catchup_author(&mut source, &limited, &author, 10, 100)
                .await
                .is_err(),
            "{resource}"
        );
    }
}

#[tokio::test]
async fn existing_index_retains_history_deletions_and_previous_root_on_replay() {
    let keys = Keys::generate();
    let author = keys.public_key().to_hex();
    let old = event(&keys, 1, "archived history", Kind::TextNote);
    let deletion = event(&keys, 20, "deletion", Kind::EventDeletion);
    let recent = event(&keys, 30, "new post", Kind::TextNote);
    let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
    let original = store.build(None, [old.clone()]).await.unwrap().unwrap();
    let mut source = Source::default();
    source
        .events
        .insert("relay-a".into(), vec![deletion.clone(), recent.clone()]);
    let incoming = fetch_catchup_author(&mut source, &policy(), &author, 10, 100)
        .await
        .unwrap();
    let report = store
        .build_with_superseded_nodes(Some(&original), incoming.clone())
        .await
        .unwrap();
    let updated = report.root.unwrap();
    let replayed = store
        .build(Some(&updated), incoming)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated, replayed);
    let actual = store
        .list_by_author(Some(&updated), &author, ListEventsOptions::default())
        .await
        .unwrap();
    assert_eq!(actual.len(), 3);
    for expected in [old.clone(), deletion, recent] {
        assert!(actual.iter().any(|event| event.id == expected.id));
    }
    assert_eq!(
        store.get_by_id(Some(&original), &old.id).await.unwrap(),
        Some(old)
    );
}

#[tokio::test]
async fn completed_empty_windows_do_not_reset_an_existing_root() {
    let keys = Keys::generate();
    let mut source = Source::default();
    let fetched =
        fetch_catchup_author(&mut source, &policy(), &keys.public_key().to_hex(), 10, 100)
            .await
            .unwrap();
    assert!(fetched.is_empty());
    assert_eq!(source.requests.len(), 2);
    let store = NostrEventStore::new(Arc::new(MemoryStore::new()));
    let original = store
        .build(None, [event(&keys, 1, "old", Kind::TextNote)])
        .await
        .unwrap();
    assert_eq!(
        store.build(original.as_ref(), fetched).await.unwrap(),
        original
    );
}
