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

async fn peer(
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
async fn discovers_multiple_fips_peers_without_a_provider_roster() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let mut config = Config::default();
    config.server.fips_local_rendezvous_addr = Some(addr.to_string());
    config.server.fips_discovery_scope = format!("catchup-selection-{}", uuid::Uuid::new_v4());
    config.server.enable_fips_udp = false;
    config.server.enable_fips_webrtc = false;
    config.server.enable_fips_lan_discovery = false;
    config.server.fips_relays = Some(Vec::new());
    let keys = Keys::generate();
    let event = |label: &str| {
        EventBuilder::new(Kind::TextNote, label)
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap()
    };
    let shared = event("same signed ID at both peers");
    let first_only = event("first peer only");
    let (first_endpoint, first, first_history) = peer(
        &config,
        &[shared.clone(), first_only.clone()],
        Duration::from_millis(300),
    )
    .await;
    let second_only = event("second peer only");
    let (other_endpoint, other, other_history) = peer(
        &config,
        &[shared.clone(), second_only.clone()],
        Duration::ZERO,
    )
    .await;
    let runtime = Runtime::start(&config, &[], Duration::from_secs(2))
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
                let visible = [&first_endpoint.local_peer_id, &other_endpoint.local_peer_id]
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
            .ok_or_else(|| anyhow::anyhow!("discovered peer query unavailable"))?;
        ensure!(
            other_history.requests.load(Ordering::SeqCst) > 0,
            "second discovered peer was not queried"
        );
        ensure!(
            first_history.requests.load(Ordering::SeqCst) > 0,
            "first discovered peer was not queried"
        );
        ensure!(
            report.events.len() == 3,
            "peer events must be merged and deduplicated"
        );
        for expected in [shared.id, first_only.id, second_only.id] {
            ensure!(
                report
                    .events
                    .iter()
                    .any(|event| event.event.as_event().id == expected),
                "discovered peer event lost"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    runtime.shutdown().await;
    first.shutdown_shared().await;
    other.shutdown_shared().await;
    first_endpoint.native_endpoint.shutdown().await.unwrap();
    other_endpoint.native_endpoint.shutdown().await.unwrap();
    result.unwrap();
}
