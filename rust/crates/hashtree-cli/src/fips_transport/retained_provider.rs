use super::DaemonNostrCache;
use crate::{Config, NostrRootResolver};
use anyhow::{Context, Result};
use nostr::nips::nip19::ToBech32;
use nostr::JsonUtil;
use nostr_pubsub::{
    EventBus, EventSource, Filter, PublishReport, PubsubProvider, PubsubProviderMode, QueryOptions,
    QueryReport, VerifiedEvent,
};
use nostr_pubsub_fips::FipsPubsubClient;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Daemon-owned provider; shutdown closes intake and drains owned cache work.
/// Full daemon shutdown stops replay workers first so already-owned requests
/// cannot retain the durable namespace through a stopped external handle.
pub struct DaemonNostrProvider {
    mode: PubsubProviderMode,
    state: tokio::sync::RwLock<Option<ProviderState>>,
}

struct ProviderState {
    inner: Arc<dyn PubsubProvider>,
    cache: Option<Arc<DaemonNostrCache>>,
    client: Option<Arc<FipsPubsubClient>>,
    intake: Vec<JoinHandle<()>>,
}

impl Drop for ProviderState {
    fn drop(&mut self) {
        for task in &self.intake {
            task.abort();
        }
    }
}

impl DaemonNostrProvider {
    pub(super) fn new(
        inner: Arc<dyn PubsubProvider>,
        cache: Option<Arc<DaemonNostrCache>>,
        client: Option<Arc<FipsPubsubClient>>,
        intake: Vec<JoinHandle<()>>,
    ) -> Self {
        Self {
            mode: inner.mode(),
            state: tokio::sync::RwLock::new(Some(ProviderState {
                inner,
                cache,
                client,
                intake,
            })),
        }
    }

    pub async fn shutdown(&self) {
        let mut state = self.state.write().await;
        if let Some(mut owned) = state.take() {
            if let Some(client) = &owned.client {
                if let Err(error) = client.set_replay_source(None) {
                    tracing::warn!(%error, "Retained head replay source could not be detached");
                }
            }
            if let Some(cache) = &owned.cache {
                close_intake(std::mem::take(&mut owned.intake), cache).await;
            }
        }
    }
}

#[async_trait::async_trait]
impl EventBus for DaemonNostrProvider {
    async fn publish(
        &self,
        event: VerifiedEvent,
        source: EventSource,
    ) -> nostr_pubsub::Result<PublishReport> {
        // Keep admission alive through the operation. Shutdown waits for any
        // already admitted read/write before releasing its checkpoint lease.
        let state = self.state.read().await;
        let state = state.as_ref().ok_or_else(closed)?;
        if let Some(cache) = state.cache.as_ref().filter(|_| is_public_head(&event)) {
            if event.as_event().as_json().len()
                > nostr_pubsub_fips::FIPS_NOSTR_PUBSUB_MAX_FRAME_BYTES
            {
                return Err(nostr_pubsub::PubsubError::Validation(
                    "Signed root exceeds the retained event size limit".into(),
                ));
            }
            let report = cache.publish(event.clone(), source.clone()).await?;
            if !report.accepted {
                return Ok(report);
            }
        }
        state.inner.publish(event, source).await
    }

    async fn query(
        &self,
        filters: Vec<Filter>,
        options: QueryOptions,
    ) -> nostr_pubsub::Result<QueryReport> {
        let state = self.state.read().await;
        state
            .as_ref()
            .ok_or_else(closed)?
            .inner
            .query(filters, options)
            .await
    }
}

fn closed() -> nostr_pubsub::PubsubError {
    nostr_pubsub::PubsubError::Storage("Daemon Nostr provider is closed".into())
}

impl PubsubProvider for DaemonNostrProvider {
    fn mode(&self) -> PubsubProviderMode {
        // Provider mode is immutable; held separately from its closeable state.
        self.mode
    }
}

pub(super) fn is_public_head(event: &VerifiedEvent) -> bool {
    let event = event.as_event();
    let Some(tree) = event.tags.identifier() else {
        return false;
    };
    let author = event
        .pubkey
        .to_bech32()
        .expect("public key bech32 encoding is infallible");
    NostrRootResolver::event_matches_key(&format!("{author}/{tree}"), event).unwrap_or(false)
}

type IntakeEvent = (nostr_pubsub::QueryEvent, bool);

pub(super) async fn start_intake(
    config: &Config,
    cache: Arc<DaemonNostrCache>,
    client: Option<Arc<FipsPubsubClient>>,
) -> Result<Vec<JoinHandle<()>>> {
    if config.nostr.retained_roots.len() > 64 {
        anyhow::bail!("nostr.retained_roots is limited to 64 author/tree keys");
    }
    let filters = config
        .nostr
        .retained_roots
        .iter()
        .map(|key| NostrRootResolver::filter_for_key(key).map(|filter| filter.limit(1)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if filters.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(client) = &client {
        let options = client.options();
        // Reserve a slot for periodic reconciliation in addition to open intake.
        if filters.len().div_ceil(options.max_filters_per_subscription)
            >= options.max_active_subscriptions
        {
            anyhow::bail!("Retained roots exceed available FIPS subscription capacity");
        }
    }
    // Complete fallible setup before spawning owned tasks.
    let relay = if config.nostr.relays.is_empty() {
        None
    } else {
        Some(Arc::new(
            nostr_pubsub_relay::RelayEventBus::new(
                config.nostr.relays.clone(),
                Duration::from_secs(5),
            )
            .await
            .context("Retained root relay provider")?,
        ))
    };
    let (sender, receiver) = tokio::sync::mpsc::channel(128);
    let mut tasks = vec![tokio::spawn(consume_intake(
        cache,
        client.clone(),
        receiver,
    ))];
    if let Some(relay) = relay {
        tasks.push(reconcile(
            relay.clone(),
            filters.clone(),
            sender.clone(),
            true,
        ));
        let filters = filters.clone();
        let sender = sender.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let sender = sender.clone();
                let subscription = relay
                    .subscribe_with_admission(
                        filters.clone(),
                        Arc::new(move |event| {
                            if !is_public_head(&event.event) {
                                return true;
                            }
                            !matches!(
                                sender.try_send((event, true)),
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_))
                            )
                        }),
                    )
                    .await;
                if let Ok(subscription) = subscription {
                    // SDK reconnect keeps this subscription open. Dropping the
                    // owned task also drops/closes the subscription.
                    std::future::pending::<()>().await;
                    let _ = subscription.close().await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }));
    }
    if let Some(client) = client {
        tasks.push(reconcile_fips(
            client.clone(),
            filters.clone(),
            sender.clone(),
        ));
        for chunk in filters.chunks(client.options().max_filters_per_subscription) {
            let filters = chunk.to_vec();
            let client = client.clone();
            let sender = sender.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    if let Ok(mut subscription) = client.subscribe(filters.clone()).await {
                        while let Some(event) = subscription.recv().await {
                            if sender.send((event, false)).await.is_err() {
                                return;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }));
        }
    }
    Ok(tasks)
}

fn reconcile_fips(
    client: Arc<FipsPubsubClient>,
    filters: Vec<Filter>,
    sender: tokio::sync::mpsc::Sender<IntakeEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60) / filters.len() as u32);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        for filter in filters.into_iter().cycle() {
            tick.tick().await;
            if let Ok(Some(event)) = query_fresh_head(&client, filter).await {
                if sender.send((event, false)).await.is_err() {
                    return;
                }
            }
        }
    })
}

async fn query_fresh_head(
    client: &Arc<FipsPubsubClient>,
    filter: Filter,
) -> nostr_pubsub::Result<Option<nostr_pubsub::QueryEvent>> {
    use nostr_pubsub::NostrEventSubscriber;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let subscription = client
        .fresh_subscriber()
        .subscribe(
            vec![filter],
            Arc::new(move |event| {
                let _ = sender.try_send(event);
            }),
        )
        .await?;
    let event = tokio::time::timeout(client.options().query_timeout, receiver.recv())
        .await
        .ok()
        .flatten();
    subscription.close().await?;
    Ok(event)
}

fn reconcile(
    source: Arc<dyn EventBus>,
    filters: Vec<Filter>,
    sender: tokio::sync::mpsc::Sender<IntakeEvent>,
    forward: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // One outstanding one-filter query per transport. A slow relay cannot
        // block live intake or accumulate an unbounded reconciliation backlog.
        let mut tick = tokio::time::interval(Duration::from_secs(60) / filters.len() as u32);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        for filter in filters.into_iter().cycle() {
            tick.tick().await;
            // Results bypass notification dedup, recovering after queue/disk pressure.
            if let Ok(report) = source
                .query(vec![filter], QueryOptions { limit: Some(1) })
                .await
            {
                for event in report.events {
                    if sender.send((event, forward)).await.is_err() {
                        return;
                    }
                }
            }
        }
    })
}

async fn consume_intake(
    cache: Arc<DaemonNostrCache>,
    client: Option<Arc<FipsPubsubClient>>,
    mut receiver: tokio::sync::mpsc::Receiver<IntakeEvent>,
) {
    while let Some((event, forward)) = receiver.recv().await {
        forward_retained(
            &cache,
            if forward { client.as_deref() } else { None },
            event,
        )
        .await;
    }
}

async fn close_intake(tasks: Vec<JoinHandle<()>>, cache: &DaemonNostrCache) {
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
    // Cancellation cannot abandon an in-flight blocking index commit/read.
    // The owned operation releases this mutex only after its work finishes.
    let _ = cache.root_cid().await;
}

async fn forward_retained(
    cache: &DaemonNostrCache,
    client: Option<&FipsPubsubClient>,
    event: nostr_pubsub::QueryEvent,
) {
    if persist(cache, &event).await {
        if let Some(client) = client {
            // Announce exactly the signed relay event; no new timestamp or
            // publisher identity is introduced.
            let _ = client.publish(event.event, event.source).await;
        }
    }
}

async fn persist(cache: &DaemonNostrCache, event: &nostr_pubsub::QueryEvent) -> bool {
    if !is_public_head(&event.event) {
        return false;
    }
    if event.event.as_event().as_json().len() > nostr_pubsub_fips::FIPS_NOSTR_PUBSUB_MAX_FRAME_BYTES
    {
        return false;
    }
    match cache
        .publish(event.event.clone(), event.source.clone())
        .await
    {
        Ok(report) => report.accepted,
        Err(error) => {
            tracing::warn!(%error, "Retained signed head could not be committed");
            false
        }
    }
}

#[cfg(test)]
#[path = "../../tests/common/mod.rs"]
mod test_common;
#[cfg(test)]
mod tests;
