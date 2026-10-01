use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hashtree_core::Cid;
use hashtree_resolver::{nostr::NostrRootResolver, Event, ResolverError, RootResolver, ToBech32};
use nostr_pubsub::{EventSourceKind, NostrEventSubscriber, NostrEventSubscription};
use tokio::sync::mpsc;

use crate::UpdateEventCache;

/// Update discovery through an application-owned live pubsub provider.
///
/// The provider must deliver fresh peer observations, not replay its own cache
/// with a historical peer source. For FIPS use `FipsPubsubClient::fresh_subscriber`.
/// Local-index deliveries may advance the rollback watermark but never confirm
/// a check. Quiet windows and stale peer responses are inconclusive errors.
/// This resolver never creates a client, opens sockets, or stops the provider.
pub struct PubsubRootResolver {
    provider: Arc<dyn NostrEventSubscriber>,
    window: Duration,
    roots: Arc<Mutex<HashMap<String, UpdateEventCache>>>,
}

impl PubsubRootResolver {
    pub fn new(provider: Arc<dyn NostrEventSubscriber>, window: Duration) -> Self {
        Self {
            provider,
            window,
            roots: Arc::default(),
        }
    }

    /// Remember a signed announcement without treating it as a fresh check.
    pub async fn ingest_event(&self, event: Event) -> Result<bool, ResolverError> {
        let tree = event
            .tags
            .iter()
            .find_map(|tag| {
                let values = tag.as_slice();
                (values.first().is_some_and(|name| name == "d"))
                    .then(|| values.get(1))
                    .flatten()
            })
            .ok_or_else(|| ResolverError::Other("root event has no tree identifier".into()))?;
        let key = format!("{}/{}", event.pubkey.to_bech32().map_err(network)?, tree);
        let mut roots = self.roots.lock().map_err(network)?;
        if !roots.contains_key(&key) {
            roots.insert(
                key.clone(),
                UpdateEventCache::for_key(key.clone()).map_err(network)?,
            );
        }
        roots
            .get_mut(&key)
            .unwrap()
            .ingest_event(event)
            .map_err(network)
    }

    /// The newest authenticated observation, suitable for durable rollback protection.
    pub async fn latest_event(&self, key: &str) -> Result<Option<Event>, ResolverError> {
        Ok(self
            .roots
            .lock()
            .map_err(network)?
            .get(key)
            .and_then(UpdateEventCache::latest)
            .map(|event| event.as_event().clone()))
    }
}

#[async_trait]
impl RootResolver for PubsubRootResolver {
    async fn resolve(&self, key: &str) -> Result<Option<Cid>, ResolverError> {
        let filter = self
            .roots
            .lock()
            .map_err(network)?
            .entry(key.to_owned())
            .or_insert(UpdateEventCache::for_key(key.to_owned()).map_err(network)?)
            .filter()
            .clone();
        let observed = Arc::new(Mutex::new(None));
        let sink = observed.clone();
        let roots = self.roots.clone();
        let observed_key = key.to_owned();
        let deadline = tokio::time::Instant::now() + self.window;
        let subscription = tokio::time::timeout_at(
            deadline,
            self.provider.subscribe(
                vec![filter],
                Arc::new(move |incoming| {
                    let Ok(mut roots) = roots.lock() else { return };
                    let cache = roots.get_mut(&observed_key).unwrap();
                    let id = incoming.event.as_event().id;
                    cache.ingest(incoming.event);
                    if incoming.source.kind != EventSourceKind::LocalIndex
                        && cache
                            .latest()
                            .is_some_and(|event| event.as_event().id == id)
                    {
                        if let Ok(mut observed) = sink.lock() {
                            *observed = Some(id);
                        }
                    }
                }),
            ),
        )
        .await
        .map_err(network)?
        .map_err(network)?;
        let mut subscription = CloseOnDrop(Some(subscription));
        // Keep observing for the whole window: the first reply need not be the
        // newest release, and an early quiet interval says nothing about peers.
        tokio::time::sleep_until(deadline).await;
        subscription
            .0
            .take()
            .unwrap()
            .close()
            .await
            .map_err(network)?;
        let roots = self.roots.lock().map_err(network)?;
        match roots.get(key).and_then(UpdateEventCache::latest) {
            Some(event) if *observed.lock().map_err(network)? == Some(event.as_event().id) => {
                NostrRootResolver::root_from_event(key, event.as_event())
            }
            _ => Err(ResolverError::Network(format!(
                "Update check inconclusive: no current peer observation for {key}"
            ))),
        }
    }

    async fn subscribe(&self, _key: &str) -> Result<mpsc::Receiver<Option<Cid>>, ResolverError> {
        Err(ResolverError::Other(
            "Use the shared pubsub provider for continuous subscriptions".into(),
        ))
    }
}

fn network(error: impl std::fmt::Display) -> ResolverError {
    ResolverError::Network(error.to_string())
}

struct CloseOnDrop(Option<Box<dyn NostrEventSubscription>>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if let Some(subscription) = self.0.take() {
            tokio::spawn(async move {
                let _ = subscription.close().await;
            });
        }
    }
}
