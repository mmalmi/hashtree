use super::auth::AppState;
use nostr::{Event, EventId, Filter as NostrFilter};
use nostr_pubsub::{EventSourceKind, QueryOptions};
use std::collections::HashSet;

pub(super) struct LocalRequestNostrQuery {
    pub(super) local_events: Vec<Event>,
    pub(super) upstream_events: Vec<Event>,
    upstream_sources: std::collections::HashMap<EventId, EventSourceKind>,
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

    pub(super) fn upstream_source(&self, event_id: &EventId) -> Option<EventSourceKind> {
        self.upstream_sources.get(event_id).copied()
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
            upstream_sources: std::collections::HashMap::new(),
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
            upstream_sources: std::collections::HashMap::new(),
        };
    }

    let mut upstream_filter = filter.clone();
    upstream_filter.limit = Some(limit);
    let upstream_query = match state.nostr_provider.as_ref() {
        Some(provider) => match provider
            .query(vec![upstream_filter], QueryOptions { limit: Some(limit) })
            .await
        {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!("configured Nostr event provider query failed: {error}");
                Default::default()
            }
        },
        None => Default::default(),
    };
    let mut upstream_sources = std::collections::HashMap::new();
    let upstream_events = upstream_query
        .events
        .into_iter()
        .map(|query_event| {
            let event = query_event.event.into_event();
            upstream_sources.insert(event.id, query_event.source.kind);
            event
        })
        .collect::<Vec<_>>();

    if let Some(relay) = &state.nostr_relay {
        for event in &upstream_events {
            let _ = relay.ingest_trusted_event_silent(event.clone()).await;
        }
    }

    LocalRequestNostrQuery {
        local_events,
        upstream_events,
        upstream_sources,
    }
}
