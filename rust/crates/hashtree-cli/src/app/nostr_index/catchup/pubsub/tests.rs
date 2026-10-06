use super::*;
use nostr::{Event, EventBuilder};
use nostr_pubsub::{EventSource, QueryEvent, VerifiedEvent};

fn policy() -> CatchupPolicy {
    serde_json::from_value(serde_json::json!({
        "base_root":"original", "authors_sha256":"authors", "author_count":1,
        "initial_since":10, "overlap_secs":10, "relays":["relay"], "kinds":[1,5],
        "page_size":10, "max_pages_per_author":10, "max_events_per_author":10,
        "max_bytes_per_author":8192, "fetch_timeout_secs":1, "index_commit_batch_size":10
    }))
    .unwrap()
}
fn event(keys: &Keys, at: u64, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from_secs(at))
        .sign_with_keys(keys)
        .unwrap()
}
fn report(events: &[Event]) -> QueryReport {
    QueryReport {
        events: events
            .iter()
            .cloned()
            .map(|event| QueryEvent {
                event: VerifiedEvent::try_from(event).unwrap(),
                source: EventSource::fips_endpoint("test-peer"),
                priority: 0,
            })
            .collect(),
    }
}
#[test]
fn merge_deduplicates_peer_and_relay_ids_without_claiming_coverage() {
    let policy = policy();
    let keys = Keys::generate();
    let one = event(&keys, 20, "relay and peer");
    let two = event(&keys, 30, "peer only");
    let filter = filter(&policy, &keys.public_key().to_hex(), 10, 100).unwrap();
    let mut events = vec![stored_event_from_nostr_sdk_event(&one)];
    let receipt = merge(
        &mut events,
        Some(report(&[one.clone(), two.clone(), two.clone()])),
        &filter,
        &policy,
    )
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(receipt.events, 2);
    assert_eq!(receipt.added_events, 1);
    assert_eq!(receipt.status, Status::Observed);
    let unchanged = events.clone();
    assert_eq!(
        merge(&mut events, None, &filter, &policy).unwrap().status,
        Status::Unavailable
    );
    assert_eq!(events, unchanged);
}
#[test]
fn mismatching_peer_events_and_exhausted_union_fail_without_mutating_relays() {
    let mut policy = policy();
    let keys = Keys::generate();
    let relay = event(&keys, 20, "relay");
    let peer = event(&keys, 30, "peer");
    let filter = filter(&policy, &keys.public_key().to_hex(), 10, 100).unwrap();
    let original = vec![stored_event_from_nostr_sdk_event(&relay)];
    for bad in [
        event(&Keys::generate(), 20, "wrong author"),
        event(&keys, 101, "future"),
        EventBuilder::new(Kind::Metadata, "{}")
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap(),
    ] {
        let mut events = original.clone();
        assert!(merge(&mut events, Some(report(&[bad])), &filter, &policy).is_err());
        assert_eq!(events, original);
    }
    policy.max_events_per_author = 1;
    let mut events = original.clone();
    assert!(merge(&mut events, Some(report(&[peer.clone()])), &filter, &policy).is_err());
    assert_eq!(events, original);
    policy.max_events_per_author = 10;
    policy.max_bytes_per_author = 1;
    assert!(merge(&mut events, Some(report(&[peer])), &filter, &policy).is_err());
    assert_eq!(events, original);
}
#[test]
fn peer_discovery_is_not_part_of_relay_resume_policy() {
    let policy = policy();
    let bytes = serde_json::to_vec(&policy).unwrap();
    assert!(!String::from_utf8(bytes.clone()).unwrap().contains("pubsub"));
    let roundtrip: CatchupPolicy = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(roundtrip, policy);
    let mut state =
        hashtree_nostr::catchup::CatchupState::prepare(None, policy.clone(), Some(100), 100)
            .unwrap();
    state.coverage_head = Some("a".repeat(64));
    hashtree_nostr::catchup::CatchupState::prepare(Some(state), policy, Some(100), 100).unwrap();
}

#[test]
fn any_authenticated_fips_peer_can_contribute_with_source_provenance() {
    let policy = policy();
    let keys = Keys::generate();
    let event = event(&keys, 20, "discovered peer");
    let filter = filter(&policy, &keys.public_key().to_hex(), 10, 100).unwrap();
    let mut peer_report = report(&[event.clone()]);
    peer_report.events[0].source = EventSource::fips_endpoint("new-peer");
    let mut events = Vec::new();
    let receipt = merge(&mut events, Some(peer_report), &filter, &policy).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(receipt.sources["new-peer"], 1);
    let mut relay_report = report(&[event]);
    relay_report.events[0].source = EventSource::relay("new-peer");
    let receipt = merge(&mut Vec::new(), Some(relay_report), &filter, &policy).unwrap();
    assert_eq!(receipt.events, 0, "relay coverage must stay separate");
}

#[tokio::test]
async fn failed_pubsub_query_is_unavailable_and_does_not_replace_relay_success() {
    struct Unavailable;
    #[async_trait::async_trait]
    impl EventBus for Unavailable {
        async fn publish(
            &self,
            _: nostr_pubsub::VerifiedEvent,
            _: nostr_pubsub::EventSource,
        ) -> nostr_pubsub::Result<nostr_pubsub::PublishReport> {
            unreachable!()
        }
        async fn query(
            &self,
            _: Vec<Filter>,
            _: QueryOptions,
        ) -> nostr_pubsub::Result<QueryReport> {
            std::future::pending().await
        }
    }
    let policy = policy();
    let keys = Keys::generate();
    let filter = filter(&policy, &keys.public_key().to_hex(), 10, 100).unwrap();
    let report = query(&Unavailable, filter.clone(), Duration::from_millis(10)).await;
    let mut events = vec![stored_event_from_nostr_sdk_event(&event(
        &keys, 20, "relay",
    ))];
    let before = events.clone();
    let receipt = merge(&mut events, report, &filter, &policy).unwrap();
    assert_eq!(receipt.status, Status::Unavailable);
    assert_eq!(events, before);
}
