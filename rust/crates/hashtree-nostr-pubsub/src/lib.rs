//! `nostr-pubsub` adapter for hashtree-backed Nostr event indexes.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use hashtree_core::{Cid, Store};
use hashtree_nostr::{
    stored_event_from_nostr_sdk_event, NostrEventStore, StoredNostrEvent, VerifiedStoredNostrEvent,
};
use nostr_pubsub::{
    EventBus, EventRetentionPolicy, EventSource, PublishReport, PubsubError, QueryEvent,
    QueryOptions, QueryReport, Result, VerifiedEvent,
};
use tokio::sync::Mutex;

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
        query_index(
            Arc::clone(&self.store),
            self.root.clone(),
            self.source.clone(),
            self.priority,
            filters,
            options,
        )
        .await
    }
}

#[derive(Clone)]
pub struct HashtreeNostrBoundedEventCache<S> {
    store: Arc<S>,
    root: Arc<Mutex<Option<Cid>>>,
    source: EventSource,
    priority: i32,
    retention: EventRetentionPolicy,
}

impl<S> HashtreeNostrBoundedEventCache<S> {
    pub fn new(
        store: Arc<S>,
        root: Option<Cid>,
        source: EventSource,
        retention: EventRetentionPolicy,
    ) -> Self {
        Self {
            store,
            root: Arc::new(Mutex::new(root)),
            source,
            priority: 0,
            retention,
        }
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub async fn root_cid(&self) -> Option<Cid> {
        self.root.lock().await.clone()
    }

    pub fn source(&self) -> &EventSource {
        &self.source
    }

    pub fn retention(&self) -> &EventRetentionPolicy {
        &self.retention
    }
}

#[async_trait]
impl<S> EventBus for HashtreeNostrBoundedEventCache<S>
where
    S: Store + 'static,
{
    async fn publish(&self, event: VerifiedEvent, _source: EventSource) -> Result<PublishReport> {
        if !self.retention.accepts(&event) {
            return Ok(PublishReport {
                accepted: false,
                priority: 0,
                reason: Some("event outside retention policy".to_string()),
            });
        }

        let stored_event = stored_event_from_nostr_sdk_event(event.as_event());
        let mut root = self.root.lock().await;
        let next_root = append_bounded_index_event(
            Arc::clone(&self.store),
            root.clone(),
            self.retention.clone(),
            stored_event,
        )
        .await?;
        *root = next_root;

        Ok(PublishReport {
            accepted: true,
            priority: self.priority,
            reason: None,
        })
    }

    async fn query(
        &self,
        filters: Vec<nostr_pubsub::Filter>,
        options: QueryOptions,
    ) -> Result<QueryReport> {
        let root = self.root.lock().await.clone();
        query_index(
            Arc::clone(&self.store),
            root,
            self.source.clone(),
            self.priority,
            filters,
            options,
        )
        .await
    }
}

async fn append_bounded_index_event<S>(
    store: Arc<S>,
    root: Option<Cid>,
    retention: EventRetentionPolicy,
    event: StoredNostrEvent,
) -> Result<Option<Cid>>
where
    S: Store + 'static,
{
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                PubsubError::Storage(format!("build hashtree cache runtime: {error}"))
            })?;
        runtime
            .block_on(async move {
                let event_store = NostrEventStore::new(store);
                let mut seen = HashSet::new();
                let mut retained = Vec::new();
                let filters = if retention.filters.is_empty() {
                    vec![nostr_pubsub::Filter::new()]
                } else {
                    retention.filters
                };
                for filter in filters {
                    for stored in event_store
                        .query_events(root.as_ref(), &filter, retention.max_events)
                        .await?
                    {
                        if seen.insert(stored.id.clone()) {
                            retained.push(stored);
                        }
                    }
                }

                retained.retain(|stored| stored.id != event.id);
                retained.push(event);
                retained.sort_by(|left, right| {
                    right
                        .created_at
                        .cmp(&left.created_at)
                        .then_with(|| right.id.cmp(&left.id))
                });
                retained.truncate(retention.max_events);
                event_store.build(None, retained).await
            })
            .map_err(|error| PubsubError::Storage(format!("write hashtree nostr cache: {error}")))
    })
    .await
    .map_err(|error| PubsubError::Storage(format!("join hashtree nostr cache write: {error}")))?
}

async fn query_index<S>(
    store: Arc<S>,
    root: Option<Cid>,
    source: EventSource,
    priority: i32,
    filters: Vec<nostr_pubsub::Filter>,
    options: QueryOptions,
) -> Result<QueryReport>
where
    S: Store + 'static,
{
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
        let stored_events =
            query_index_filter(Arc::clone(&store), root.clone(), filter, remaining).await?;
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
            let event = VerifiedEvent::try_from(event)
                .map_err(|error| PubsubError::Validation(format!("verify nostr event: {error}")))?;
            events.push(QueryEvent {
                event,
                source: source.clone(),
                priority,
            });
            if events.len() >= limit {
                break;
            }
        }
    }

    Ok(QueryReport { events })
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
    use nostr_pubsub::{EventBus, EventRetentionPolicy, EventSource, QueryOptions, VerifiedEvent};
    use nostr_sdk::{EventBuilder, Filter, Keys, Kind, Timestamp};

    use super::{HashtreeNostrBoundedEventCache, HashtreeNostrIndexEventBus};

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

    #[tokio::test]
    async fn bounded_cache_publishes_matching_events_into_hashtree_index() {
        let backing = Arc::new(MemoryStore::new());
        let author = Keys::generate();
        let event = EventBuilder::text_note("cached hello")
            .custom_created_at(Timestamp::from(20))
            .sign_with_keys(&author)
            .expect("sign event");
        let bus = HashtreeNostrBoundedEventCache::new(
            Arc::clone(&backing),
            None,
            EventSource::local_index("hashtree-cache"),
            EventRetentionPolicy::new(4, vec![Filter::new().kind(Kind::TextNote)]),
        )
        .with_priority(15);

        let report = bus
            .publish(
                VerifiedEvent::try_from(event.clone()).expect("verify event"),
                EventSource::peer("writer"),
            )
            .await
            .expect("publish event");

        assert!(report.accepted);
        assert_eq!(report.priority, 15);
        assert!(bus.root_cid().await.is_some());

        let query = bus
            .query(
                vec![Filter::new()
                    .author(author.public_key())
                    .kind(Kind::TextNote)],
                QueryOptions { limit: Some(10) },
            )
            .await
            .expect("query event");

        assert_eq!(query.events.len(), 1);
        assert_eq!(query.events[0].event.as_event().id, event.id);
        assert_eq!(
            query.events[0].source,
            EventSource::local_index("hashtree-cache")
        );
        assert_eq!(query.events[0].priority, 15);
    }

    #[tokio::test]
    async fn bounded_cache_rejects_events_outside_retention_policy() {
        let bus = HashtreeNostrBoundedEventCache::new(
            Arc::new(MemoryStore::new()),
            None,
            EventSource::local_index("hashtree-cache"),
            EventRetentionPolicy::new(4, vec![Filter::new().kind(Kind::TextNote)]),
        );
        let event = EventBuilder::new(Kind::Metadata, "{}")
            .sign_with_keys(&Keys::generate())
            .expect("sign event");

        let report = bus
            .publish(
                VerifiedEvent::try_from(event).expect("verify event"),
                EventSource::peer("writer"),
            )
            .await
            .expect("publish event");
        let query = bus
            .query(vec![Filter::new()], QueryOptions { limit: Some(10) })
            .await
            .expect("query cache");

        assert!(!report.accepted);
        assert_eq!(
            report.reason.as_deref(),
            Some("event outside retention policy")
        );
        assert!(bus.root_cid().await.is_none());
        assert!(query.events.is_empty());
    }

    #[tokio::test]
    async fn bounded_cache_keeps_newest_events() {
        let bus = HashtreeNostrBoundedEventCache::new(
            Arc::new(MemoryStore::new()),
            None,
            EventSource::local_index("hashtree-cache"),
            EventRetentionPolicy::new(2, vec![Filter::new().kind(Kind::TextNote)]),
        );
        let author = Keys::generate();
        for created_at in [10, 20, 30] {
            let event = EventBuilder::text_note(format!("event-{created_at}"))
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&author)
                .expect("sign event");
            bus.publish(
                VerifiedEvent::try_from(event).expect("verify event"),
                EventSource::peer("writer"),
            )
            .await
            .expect("publish event");
        }

        let query = bus
            .query(
                vec![Filter::new().kind(Kind::TextNote)],
                QueryOptions { limit: Some(10) },
            )
            .await
            .expect("query cache");
        let contents = query
            .events
            .iter()
            .map(|event| event.event.as_event().content.as_str())
            .collect::<Vec<_>>();

        assert_eq!(contents, vec!["event-30", "event-20"]);
    }
}
