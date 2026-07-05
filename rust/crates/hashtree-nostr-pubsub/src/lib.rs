//! `nostr-pubsub` adapter for hashtree-backed Nostr event indexes.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use hashtree_core::{Cid, Store};
use hashtree_nostr::{NostrEventStore, StoredNostrEvent, VerifiedStoredNostrEvent};
use nostr_pubsub::{
    EventBus, EventSource, PublishReport, PubsubError, QueryEvent, QueryOptions, QueryReport,
    Result, VerifiedEvent,
};

#[derive(Clone)]
pub struct HashtreeNostrIndexEventBus<S> {
    store: Arc<S>,
    root: Option<Cid>,
    source: EventSource,
    priority: i32,
}

impl<S> HashtreeNostrIndexEventBus<S> {
    pub fn new(store: Arc<S>, root: Option<Cid>, source: EventSource) -> Self {
        Self {
            store,
            root,
            source,
            priority: 0,
        }
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_root(mut self, root: Option<Cid>) -> Self {
        self.root = root;
        self
    }

    pub fn root(&self) -> Option<&Cid> {
        self.root.as_ref()
    }

    pub fn source(&self) -> &EventSource {
        &self.source
    }
}

#[async_trait]
impl<S> EventBus for HashtreeNostrIndexEventBus<S>
where
    S: Store + 'static,
{
    async fn publish(&self, _event: VerifiedEvent, _source: EventSource) -> Result<PublishReport> {
        Err(PubsubError::Validation(
            "hashtree nostr index event bus is read-only".to_string(),
        ))
    }

    async fn query(
        &self,
        filters: Vec<nostr_pubsub::Filter>,
        options: QueryOptions,
    ) -> Result<QueryReport> {
        let limit = query_limit(&filters, options);
        if limit == 0 {
            return Ok(QueryReport::default());
        }

        let filters = if filters.is_empty() {
            vec![nostr_pubsub::Filter::new()]
        } else {
            filters
        };

        let mut seen = HashSet::new();
        let mut events = Vec::new();
        for filter in filters {
            let remaining = limit.saturating_sub(events.len());
            if remaining == 0 {
                break;
            }
            let stored_events = query_index_filter(
                Arc::clone(&self.store),
                self.root.clone(),
                filter,
                remaining,
            )
            .await?;
            for stored in stored_events {
                if !seen.insert(stored.id.clone()) {
                    continue;
                }
                let verified = VerifiedStoredNostrEvent::try_from(stored).map_err(|error| {
                    PubsubError::Validation(format!("verify stored nostr event: {error}"))
                })?;
                let event = verified
                    .to_nostr_sdk_event()
                    .map_err(|error| {
                        PubsubError::Validation(format!("decode stored nostr event: {error}"))
                    })?
                    .into_event();
                let event = VerifiedEvent::try_from(event).map_err(|error| {
                    PubsubError::Validation(format!("verify nostr event: {error}"))
                })?;
                events.push(QueryEvent {
                    event,
                    source: self.source.clone(),
                    priority: self.priority,
                });
                if events.len() >= limit {
                    break;
                }
            }
        }

        Ok(QueryReport { events })
    }
}

async fn query_index_filter<S>(
    store: Arc<S>,
    root: Option<Cid>,
    filter: nostr_pubsub::Filter,
    limit: usize,
) -> Result<Vec<StoredNostrEvent>>
where
    S: Store + 'static,
{
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                PubsubError::Storage(format!("build hashtree query runtime: {error}"))
            })?;
        runtime
            .block_on(async move {
                NostrEventStore::new(store)
                    .query_events(root.as_ref(), &filter, limit)
                    .await
            })
            .map_err(|error| PubsubError::Storage(format!("query hashtree nostr index: {error}")))
    })
    .await
    .map_err(|error| PubsubError::Storage(format!("join hashtree nostr index query: {error}")))?
}

fn query_limit(filters: &[nostr_pubsub::Filter], options: QueryOptions) -> usize {
    options
        .limit
        .or_else(|| filters.iter().filter_map(|filter| filter.limit).min())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hashtree_core::MemoryStore;
    use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStore};
    use nostr_pubsub::{EventBus, EventSource, QueryOptions};
    use nostr_sdk::{EventBuilder, Filter, Keys, Kind, Timestamp};

    use super::HashtreeNostrIndexEventBus;

    #[tokio::test]
    async fn event_bus_queries_historical_index_with_normal_nostr_filter() {
        let backing = Arc::new(MemoryStore::new());
        let event_store = NostrEventStore::new(Arc::clone(&backing));
        let author = Keys::generate();
        let other = Keys::generate();
        let wanted = EventBuilder::text_note("historical hello")
            .custom_created_at(Timestamp::from(20))
            .sign_with_keys(&author)
            .expect("sign wanted event");
        let ignored = EventBuilder::text_note("wrong author")
            .custom_created_at(Timestamp::from(30))
            .sign_with_keys(&other)
            .expect("sign ignored event");
        let root = event_store
            .build(
                None,
                [wanted.clone(), ignored]
                    .iter()
                    .map(stored_event_from_nostr_sdk_event),
            )
            .await
            .expect("build nostr index")
            .expect("index root");

        let bus = HashtreeNostrIndexEventBus::new(
            Arc::clone(&backing),
            Some(root),
            EventSource::peer("hashtree-index"),
        )
        .with_priority(25);
        let report = bus
            .query(
                vec![Filter::new()
                    .author(author.public_key())
                    .kind(Kind::TextNote)],
                QueryOptions { limit: Some(10) },
            )
            .await
            .expect("query bus");

        assert_eq!(report.events.len(), 1);
        assert_eq!(report.events[0].event.as_event().id, wanted.id);
        assert_eq!(report.events[0].source, EventSource::peer("hashtree-index"));
        assert_eq!(report.events[0].priority, 25);
    }

    #[tokio::test]
    async fn publish_is_explicitly_rejected_for_read_only_index_bus() {
        let bus = HashtreeNostrIndexEventBus::new(
            Arc::new(MemoryStore::new()),
            None,
            EventSource::peer("hashtree-index"),
        );
        let event = EventBuilder::text_note("not written here")
            .sign_with_keys(&Keys::generate())
            .expect("sign event");
        let event = nostr_pubsub::VerifiedEvent::try_from(event).expect("verify event");

        let error = bus
            .publish(event, EventSource::peer("writer"))
            .await
            .expect_err("index bus should be read-only");
        assert!(error.to_string().contains("read-only"));
    }
}
