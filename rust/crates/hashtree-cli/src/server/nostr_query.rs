use super::auth::AppState;
use nostr::{Event, EventId, Filter as NostrFilter};
use nostr_sdk::ClientBuilder;
use std::collections::HashSet;
use std::time::Duration;

const LOCAL_REQUEST_NOSTR_QUERY_TIMEOUT: Duration = Duration::from_secs(4);

pub(super) struct LocalRequestNostrQuery {
    pub(super) local_events: Vec<Event>,
    pub(super) upstream_events: Vec<Event>,
}

impl LocalRequestNostrQuery {
    pub(super) fn merged_events(&self, limit: usize) -> Vec<Event> {
        let mut seen = HashSet::<EventId>::new();
        let mut merged = Vec::new();

        for event in self.local_events.iter().chain(self.upstream_events.iter()) {
            if seen.insert(event.id) {
                merged.push(event.clone());
            }
        }

        merged.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        merged.truncate(limit);
        merged
    }
}

fn normalized_limit(filter: &NostrFilter, limit: usize) -> usize {
    match filter.limit {
        Some(filter_limit) => filter_limit.min(limit),
        None => limit,
    }
}

fn is_locally_answerable_id_query(filter: &NostrFilter) -> bool {
    filter.ids.as_ref().is_some_and(|ids| !ids.is_empty())
        && filter.authors.is_none()
        && filter.kinds.is_none()
        && filter.search.is_none()
        && filter.since.is_none()
        && filter.until.is_none()
        && filter.generic_tags.is_empty()
}

fn normalized_relay_urls(relays: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut output = Vec::new();

    for relay in relays {
        let normalized = relay.trim();
        if normalized.is_empty() || !seen.insert(normalized.to_string()) {
            continue;
        }
        output.push(normalized.to_string());
    }

    output
}

async fn fetch_upstream_events(relays: &[String], filter: &NostrFilter) -> Vec<Event> {
    if relays.is_empty() {
        return Vec::new();
    }

    let client = ClientBuilder::default().build();
    let mut connected_relays = 0usize;
    for relay in relays {
        if client.add_relay(relay).await.is_ok() {
            connected_relays += 1;
        }
    }
    if connected_relays == 0 {
        return Vec::new();
    }

    client.connect().await;
    let result = client
        .fetch_events(filter.clone(), LOCAL_REQUEST_NOSTR_QUERY_TIMEOUT)
        .await
        .map(|events| events.to_vec())
        .unwrap_or_default();
    let _ = client.disconnect().await;
    result
}

pub(super) async fn query_events_for_local_request(
    state: &AppState,
    filter: &NostrFilter,
    limit: usize,
) -> LocalRequestNostrQuery {
    let limit = normalized_limit(filter, limit);
    if limit == 0 {
        return LocalRequestNostrQuery {
            local_events: Vec::new(),
            upstream_events: Vec::new(),
        };
    }

    let local_events = match state.nostr_relay.as_ref() {
        Some(relay) => relay.query_events(filter, limit).await,
        None => Vec::new(),
    };
    if is_locally_answerable_id_query(filter) && !local_events.is_empty() {
        return LocalRequestNostrQuery {
            local_events,
            upstream_events: Vec::new(),
        };
    }

    let mut upstream_filter = filter.clone();
    upstream_filter.limit = Some(limit);
    let upstream_events = fetch_upstream_events(
        &normalized_relay_urls(&state.nostr_relay_urls),
        &upstream_filter,
    )
    .await;

    if let Some(relay) = &state.nostr_relay {
        for event in &upstream_events {
            let _ = relay.ingest_trusted_event_silent(event.clone()).await;
        }
    }

    LocalRequestNostrQuery {
        local_events,
        upstream_events,
    }
}
