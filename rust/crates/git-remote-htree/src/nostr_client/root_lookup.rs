//! Standalone root observations through the same event buses as the daemon.
use super::*;
use anyhow::Result;
use async_trait::async_trait;
use hashtree_client::{BlobStore, ClientConfig};
use hashtree_core::{nhash_decode, nhash_encode_full, NHashData, TreeVisibility};
use hashtree_nostr::parse_verified_hashtree_root_event;
use hashtree_nostr_pubsub::{
    EventIndexCheckpoint, HashtreeNostrBoundedEventCache, HashtreeNostrIndexEventBus,
};
use nostr_pubsub::*;
use nostr_pubsub_relay::RelayEventBus;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const MAX_ROOT_EVENTS: usize = 256;
const MAX_INDEXES: usize = 8;

#[derive(Default)]
struct Observation {
    events: Vec<Event>,
    incomplete: bool,
}

struct VerifiedSources;
#[async_trait]
impl PubsubPolicy for VerifiedSources {
    async fn check_event(&self, _: EventPolicyContext<'_>) -> nostr_pubsub::Result<PolicyDecision> {
        Ok(PolicyDecision::allow_with_priority(0))
    }
    async fn check_source(
        &self,
        context: SourcePolicyContext<'_>,
    ) -> nostr_pubsub::Result<PolicyDecision> {
        Ok(PolicyDecision::allow_with_priority(
            context.candidate.priority,
        ))
    }
}

// An unavailable index must not hold up independent index or relay results.
struct BoundedReader {
    bus: Arc<dyn EventBus>,
    window: Duration,
}

#[async_trait]
impl EventBus for BoundedReader {
    async fn publish(
        &self,
        event: VerifiedEvent,
        source: EventSource,
    ) -> nostr_pubsub::Result<PublishReport> {
        tokio::time::timeout(self.window, self.bus.publish(event, source))
            .await
            .map_err(|_| PubsubError::Storage("repository event cache write timed out".into()))?
    }

    async fn query(
        &self,
        filters: Vec<Filter>,
        options: QueryOptions,
    ) -> nostr_pubsub::Result<QueryReport> {
        tokio::time::timeout(self.window, self.bus.query(filters, options))
            .await
            .map_err(|_| PubsubError::Storage("repository event index read timed out".into()))?
    }
}

struct HeadCheckpoint(PathBuf);
impl EventIndexCheckpoint for HeadCheckpoint {
    fn commit(&self, root: Option<&Cid>) -> nostr_pubsub::Result<()> {
        let Some(root) = root else {
            return Ok(());
        };
        let encoded = nhash_encode_full(&NHashData {
            hash: root.hash,
            decrypt_key: root.key,
        })
        .map_err(|e| PubsubError::Storage(e.to_string()))?;
        let temporary = self.0.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, encoded)
            .and_then(|_| std::fs::rename(&temporary, &self.0))
            .map_err(|e| PubsubError::Storage(e.to_string()))
    }
}

/// A cache namespace owned by the Git reader, independent of daemon databases.
pub(super) struct RootLookup {
    cache_dir: PathBuf,
    transport: ClientConfig,
}

impl RootLookup {
    pub(super) fn new(config: &Config, daemon_url: Option<String>, local_only: bool) -> Self {
        Self {
            cache_dir: PathBuf::from(&config.storage.data_dir).join("git-root-events"),
            transport: ClientConfig {
                daemon_url,
                local_only,
                relays: Vec::new(),
                read_servers: config.blossom.all_read_servers(),
                resolve_window: Duration::from_secs(3),
                request_timeout: Duration::from_secs(2),
            },
        }
    }

    async fn cache(
        &self,
    ) -> Result<(
        Arc<BlobStore>,
        Arc<HashtreeNostrBoundedEventCache<BlobStore>>,
    )> {
        let store = Arc::new(BlobStore::new(
            &self.cache_dir.join("blobs"),
            self.transport.clone(),
        )?);
        let head_path = self.cache_dir.join("head.nhash");
        let head = match tokio::fs::read_to_string(&head_path).await {
            Ok(value) => match nhash_decode(value.trim()) {
                Ok(decoded) => Some(Cid {
                    hash: decoded.hash,
                    key: decoded.decrypt_key,
                }),
                Err(error) => {
                    warn!(%error, "Ignoring invalid repository event cache head");
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                warn!(%error, "Repository event cache head unavailable");
                None
            }
        };
        let cache = Arc::new(
            HashtreeNostrBoundedEventCache::new(
                store.clone(),
                head,
                EventSource::local_index("git-root-cache"),
                EventRetentionPolicy::new(
                    MAX_ROOT_EVENTS,
                    vec![Filter::new().kinds(hashtree_root_kinds())],
                ),
            )
            .with_checkpoint(Arc::new(HeadCheckpoint(head_path))),
        );
        Ok((store, cache))
    }

    pub(super) async fn query(
        &self,
        filter: Filter,
        relays: &[String],
        window: Duration,
    ) -> Result<Vec<Event>> {
        let mut filters = vec![filter.clone()];
        filters.extend(index_filters());

        // Observe relays and read the cache concurrently. EOSE does not close a live observation;
        // late roots and updates remain eligible through the explicit window.
        let (local, live) = tokio::join!(
            async {
                let (store, cache) = self.cache().await?;
                let reader = BoundedReader { bus: cache, window };
                let result = reader
                    .query(
                        filters.clone(),
                        QueryOptions {
                            limit: Some(MAX_ROOT_EVENTS),
                        },
                    )
                    .await;
                Ok::<_, anyhow::Error>((store, reader, result))
            },
            observe_relays(relays, filters.clone(), window),
        );
        let mut failures = Vec::new();
        let mut observed = match live {
            Ok(observation) => {
                if observation.incomplete {
                    failures.push("repository root observation budget exceeded".into());
                }
                observation.events
            }
            Err(error) => {
                failures.push(error.to_string());
                Vec::new()
            }
        };
        if let Ok((_, _, cached)) = &local {
            match cached {
                Ok(report) => {
                    observed.extend(report.events.iter().map(|e| e.event.clone().into_event()))
                }
                Err(error) => failures.push(error.to_string()),
            }
        }

        // Publishers curate independent indexes. They are not ordered replicas
        // and need not be the author of the requested repository.
        match local {
            Ok((store, cache, _)) => {
                let mut router = NostrPubsubRouter::new(Arc::new(VerifiedSources));
                let indexes = index_roots(observed.clone());
                if indexes.len() > MAX_INDEXES {
                    failures.push("repository index observation budget exceeded".into());
                }
                for (name, root) in indexes.into_iter().take(MAX_INDEXES) {
                    let bus = Arc::new(HashtreeNostrIndexEventBus::new(
                        store.clone(),
                        Some(root),
                        EventSource::local_index(&name),
                    ));
                    router = router.with_query_source(RouterQuerySource::new(
                        bus.source_route(format!("index:{name}"))?,
                        Arc::new(BoundedReader { bus, window }),
                    ));
                }
                let report = router
                    .query_with_context(
                        vec![filter.clone()],
                        RoutedQueryOptions {
                            query: QueryOptions {
                                limit: Some(MAX_ROOT_EVENTS),
                            },
                        },
                        None,
                        None,
                    )
                    .await?;
                for attempt in report.attempts {
                    if let RouteAttemptOutcome::Failure { message } = attempt.outcome {
                        failures.push(message);
                    }
                }
                observed.extend(report.events.into_iter().map(|e| e.event.into_event()));
                let unique = observed
                    .iter()
                    .map(|event| (event.id, event))
                    .collect::<BTreeMap<_, _>>();
                for event in unique.into_values() {
                    if let Err(error) = cache
                        .publish(
                            event.clone().try_into()?,
                            EventSource::local_index("git-root-observation"),
                        )
                        .await
                    {
                        warn!(%error, "Could not retain repository events for offline lookup");
                        break;
                    }
                }
            }
            Err(error) => failures.push(error.to_string()),
        }
        observed.retain(|e| filter.match_event(e, Default::default()));
        for error in &failures {
            warn!(%error, "Repository root source unavailable; observation may be incomplete");
        }
        if observed.is_empty() && !failures.is_empty() {
            return Err(RootObservationIncomplete(failures.join("; ")).into());
        }
        Ok(observed)
    }
}

async fn observe_relays(
    relays: &[String],
    filters: Vec<Filter>,
    window: Duration,
) -> Result<Observation> {
    if relays.is_empty() {
        return Ok(Observation::default());
    }
    let bus = RelayEventBus::new(relays.iter().cloned(), window).await?;
    let result = observe(&bus, filters, window).await;
    bus.client().disconnect().await;
    result
}

fn index_filters() -> Vec<Filter> {
    vec![
        Filter::new()
            .kind(Kind::Custom(KIND_HASHTREE_ROOT))
            .identifier("nostr-event-index")
            .limit(MAX_INDEXES),
        Filter::new()
            .kind(Kind::Custom(KIND_HASHTREE_ROOT))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), "nostr-event-index")
            .limit(MAX_INDEXES),
    ]
}

fn index_roots(events: Vec<Event>) -> BTreeMap<String, Cid> {
    let mut latest = BTreeMap::<String, (Timestamp, EventId, Cid)>::new();
    for event in events {
        let Ok(Some(parsed)) = parse_verified_hashtree_root_event(&event) else {
            continue;
        };
        if !(parsed.tree_name == "nostr-event-index"
            || parsed.tree_name.starts_with("nostr-event-index/"))
            || parsed.visibility != TreeVisibility::Public
            || parsed.encrypted_key.is_some()
            || parsed.self_encrypted_key.is_some()
        {
            continue;
        }
        let id = format!("{}/{}", event.pubkey, parsed.tree_name);
        if latest.get(&id).is_none_or(|(time, previous, _)| {
            event.created_at > *time || (event.created_at == *time && event.id < *previous)
        }) {
            latest.insert(id, (event.created_at, event.id, parsed.root_cid));
        }
    }
    latest
        .into_iter()
        .map(|(name, (_, _, root))| (name, root))
        .collect()
}

async fn observe(
    bus: &RelayEventBus,
    filters: Vec<Filter>,
    window: Duration,
) -> Result<Observation> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_ROOT_EVENTS);
    let overflow = Arc::new(AtomicBool::new(false));
    let full = overflow.clone();
    let subscription = bus
        .subscribe(
            filters,
            Arc::new(move |event| {
                if sender.try_send(event.event.into_event()).is_err() {
                    full.store(true, Ordering::Relaxed);
                }
            }),
        )
        .await?;
    let deadline = tokio::time::sleep(window);
    tokio::pin!(deadline);
    let mut events = BTreeMap::new();
    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = receiver.recv() => match event {
                Some(event) => {
                    if events.len() < MAX_ROOT_EVENTS || events.contains_key(&event.id) {
                        events.insert(event.id, event);
                    } else { overflow.store(true, Ordering::Relaxed); }
                }
                None => {
                    overflow.store(true, Ordering::Relaxed);
                    break;
                },
            }
        }
    }
    if let Err(error) = subscription.close().await {
        warn!(%error, "Repository root subscription close failed");
    }
    let incomplete = overflow.load(Ordering::Relaxed);
    if incomplete {
        warn!("Repository root observation budget exceeded; results may be incomplete");
    }
    Ok(Observation {
        events: events.into_values().collect(),
        incomplete,
    })
}
