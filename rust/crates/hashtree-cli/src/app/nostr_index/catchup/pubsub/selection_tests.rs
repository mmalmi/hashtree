use super::*;
use nostr::{Event, EventBuilder};
use nostr_pubsub::{EventSource, InMemoryEventBus, PublishReport, VerifiedEvent};
use std::sync::atomic::{AtomicUsize, Ordering};

struct History {
    bus: InMemoryEventBus,
    witness: Event,
    requests: AtomicUsize,
    delay: Duration,
}

#[async_trait::async_trait]
impl EventBus for History {
    async fn publish(
        &self,
        event: VerifiedEvent,
        source: EventSource,
    ) -> nostr_pubsub::Result<PublishReport> {
        self.bus.publish(event, source).await
    }
    async fn query(
        &self,
        filters: Vec<Filter>,
        options: QueryOptions,
    ) -> nostr_pubsub::Result<QueryReport> {
        if filters
            .iter()
            .any(|filter| filter.match_event(&self.witness, Default::default()))
        {
            self.requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
        }
        self.bus.query(filters, options).await
    }
}

async fn provider(
    config: &Config,
    events: &[Event],
    delay: Duration,
) -> (BoundFipsEndpoint, FipsPubsubClient, Arc<History>) {
    let mut options = FipsEndpointOptions::new(Keys::generate().secret_key().to_bech32().unwrap());
    options.discovery_scope = config.server.fips_discovery_scope.clone();
    options.enable_udp = false;
    options.enable_webrtc = false;
    options.enable_lan_discovery = false;
    options.share_local_candidates = false;
    options.enable_local_rendezvous = true;
    let endpoint = bind_fips_endpoint_at_local_rendezvous(
        options,
        config
            .server
            .fips_local_rendezvous_addr
            .as_ref()
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap();
    let client = FipsPubsubClient::start(
        endpoint.native_endpoint.clone(),
        FipsPubsubClientOptions {
            max_replay_events: 128,
            max_inbound_routed_peers: 8,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let history = Arc::new(History {
        bus: InMemoryEventBus::new(),
        witness: events[0].clone(),
        requests: AtomicUsize::new(0),
        delay,
    });
    for event in events {
        history
            .bus
            .publish(
                VerifiedEvent::try_from(event.clone()).unwrap(),
                EventSource::local_index("fixture"),
            )
            .await
            .unwrap();
    }
    client.set_replay_source(Some(history.clone())).unwrap();
    (endpoint, client, history)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selected_peer_roster_excludes_discovered_provider_before_query_quota_and_dedup() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let mut config = Config::default();
    config.server.fips_local_rendezvous_addr = Some(addr.to_string());
    config.server.fips_discovery_scope = format!("catchup-selection-{}", uuid::Uuid::new_v4());
    let keys = Keys::generate();
    let event = |label: &str| {
        EventBuilder::new(Kind::TextNote, label)
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap()
    };
    let shared = event("same signed ID at both providers");
    let selected_only = event("selected provider only");
    let (selected_endpoint, selected, selected_history) = provider(
        &config,
        &[shared.clone(), selected_only.clone()],
        Duration::from_millis(300),
    )
    .await;
    // The unselected provider can answer faster with a shared ID plus enough
    // matching bodies to spend the entire global query quota before selection.
    let mut flood = vec![shared.clone()];
    flood.extend((1..128).map(|i| event(&format!("unselected {i}"))));
    let (other_endpoint, other, other_history) = provider(&config, &flood, Duration::ZERO).await;
    let runtime = Runtime::start(
        &config,
        vec![selected_endpoint.local_peer_id.clone()],
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    let result = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let adverts = runtime
                    .endpoint
                    .native_endpoint
                    .local_instance_advertisements()
                    .unwrap();
                let visible = [
                    &selected_endpoint.local_peer_id,
                    &other_endpoint.local_peer_id,
                ]
                .iter()
                .all(|npub| {
                    adverts.iter().any(|advert| {
                        &advert.npub == *npub
                            && advert
                                .capability(nostr_pubsub_fips::FIPS_NOSTR_PUBSUB_CAPABILITY)
                                .is_some()
                    })
                });
                if visible {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        let filter = Filter::new()
            .author(keys.public_key())
            .kind(Kind::TextNote)
            .limit(128);
        let report = query(runtime.client.as_ref(), filter, Duration::from_secs(3))
            .await
            .ok_or_else(|| anyhow::anyhow!("selected provider query unavailable"))?;
        ensure!(
            other_history.requests.load(Ordering::SeqCst) == 0,
            "unselected discovered provider received the author query before source filtering"
        );
        ensure!(
            selected_history.requests.load(Ordering::SeqCst) > 0,
            "selected provider was not queried"
        );
        ensure!(
            report.events.len() == 2,
            "unselected results consumed the selected provider's quota"
        );
        ensure!(
            report
                .events
                .iter()
                .all(|event| event.source.id.as_str() == selected_endpoint.local_peer_id),
            "unselected provider won source attribution"
        );
        for expected in [shared.id, selected_only.id] {
            ensure!(
                report
                    .events
                    .iter()
                    .any(|event| event.event.as_event().id == expected),
                "selected event lost to cross-provider deduplication"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    runtime.shutdown().await;
    selected.shutdown_shared().await;
    other.shutdown_shared().await;
    selected_endpoint.native_endpoint.shutdown().await.unwrap();
    other_endpoint.native_endpoint.shutdown().await.unwrap();
    result.unwrap();
}
