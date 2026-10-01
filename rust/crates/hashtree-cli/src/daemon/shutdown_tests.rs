use super::*;
use hashtree_fips_transport::{
    bind_fips_endpoint, set_fips_peer_configs, FipsEndpointOptions, FipsPeerConfig,
};
use nostr_pubsub::{
    EventBus, EventSource, Filter, PublishReport, QueryOptions, QueryReport, VerifiedEvent,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct ReplayReleaseGate {
    query_entered: Notify,
    query_deadline: StdMutex<Option<Instant>>,
    dropped_before_query_timeout: AtomicBool,
    entered: Notify,
    released: StdMutex<bool>,
    wake: Condvar,
}

impl ReplayReleaseGate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

struct ReleaseOnDrop(Arc<ReplayReleaseGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct GatedReplaySource {
    gate: Arc<ReplayReleaseGate>,
    query_timeout: Duration,
}

#[async_trait::async_trait]
impl EventBus for GatedReplaySource {
    async fn publish(
        &self,
        _: VerifiedEvent,
        _: EventSource,
    ) -> nostr_pubsub::Result<PublishReport> {
        unreachable!("this source observes shutdown, not publication")
    }

    async fn query(&self, _: Vec<Filter>, _: QueryOptions) -> nostr_pubsub::Result<QueryReport> {
        self.gate
            .query_deadline
            .lock()
            .unwrap()
            .get_or_insert_with(|| Instant::now() + self.query_timeout);
        self.gate.query_entered.notify_one();
        // The real replay request owns this source until its worker is stopped.
        // Provider detach alone must not release the observation gate.
        std::future::pending().await
    }
}

impl Drop for GatedReplaySource {
    fn drop(&mut self) {
        self.gate.dropped_before_query_timeout.store(
            self.gate
                .query_deadline
                .lock()
                .unwrap()
                .is_some_and(|deadline| Instant::now() < deadline),
            Ordering::SeqCst,
        );
        // The in-flight request has released its source during worker shutdown.
        // Pause that shutdown before the join can finish.
        self.gate.entered.notify_one();
        let released = self.gate.released.lock().unwrap();
        drop(
            self.gate
                .wake
                .wait_timeout_while(released, Duration::from_secs(10), |released| !*released)
                .unwrap(),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_shutdown_keeps_checkpoint_owned_until_replay_workers_stop() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("data");
    let mut config = Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().into_owned();
    config.server.enable_auth = false;
    config.server.fips_discovery_scope = format!("htree-shutdown-order-{}", uuid::Uuid::new_v4());
    config.server.fips_relays = Some(Vec::new());
    config.server.fips_websocket_seed_urls = Some(Vec::new());
    config.server.enable_fips_udp = true;
    config.server.fips_udp_bind_addr = Some("127.0.0.1:0".into());
    config.server.enable_fips_webrtc = false;
    config.server.enable_fips_lan_discovery = false;
    let rendezvous = std::net::UdpSocket::bind("127.0.0.1:0")?;
    config.server.fips_local_rendezvous_addr = Some(rendezvous.local_addr()?.to_string());
    drop(rendezvous);
    config.nostr.event_transport = crate::config::NostrEventTransport::FipsLocalOnly;
    config.nostr.relays.clear();
    config.nostr.bootstrap_follows.clear();
    config.nostr.social_graph_crawl_depth = 0;
    config.nostr.decentralized_pubsub = false;
    config.sync.enabled = false;
    let info = start_embedded(EmbeddedDaemonOptions {
        config,
        data_dir,
        config_dir: Some(temp.path().join("config")),
        bind_address: "127.0.0.1:0".to_owned(),
        relays: None,
        initial_tree_roots: Vec::new(),
        extra_routes: None,
        cors: None,
    })
    .await?;
    let controller = info.daemon_controller.clone();
    let provider = controller.nostr_provider.lock().await.clone().unwrap();
    let client = controller
        .fips_handle
        .as_ref()
        .unwrap()
        .pubsub_client
        .as_ref()
        .unwrap()
        .clone();
    let event = crate::NostrRootResolver::root_event_builder(
        "releases/shutdown",
        &Cid::public([19; 32]),
        None,
    )
    .sign_with_keys(&Keys::generate())?;
    assert!(
        provider
            .publish(
                VerifiedEvent::try_from(event.clone())?,
                EventSource::local_index("shutdown-test"),
            )
            .await?
            .accepted
    );

    let gate = Arc::new(ReplayReleaseGate::default());
    let release = ReleaseOnDrop(gate.clone());
    client.set_replay_source(Some(Arc::new(GatedReplaySource {
        gate: gate.clone(),
        query_timeout: client.options().query_timeout,
    })))?;
    let mut options = FipsEndpointOptions::new(Keys::generate().secret_key().to_bech32()?);
    options.enable_webrtc = false;
    options.enable_lan_discovery = false;
    options.enable_local_rendezvous = false;
    options.share_local_candidates = false;
    options.relays.clear();
    options.udp_bind_addr = Some("127.0.0.1:0".into());
    let remote = bind_fips_endpoint(options).await?;
    let observer = nostr_pubsub_fips::FipsPubsubClient::start(
        remote.native_endpoint.clone(),
        Default::default(),
    )
    .await?;
    let local = &controller.fips_handle.as_ref().unwrap().endpoint;
    for (local, remote) in [
        (local, &remote.native_endpoint),
        (&remote.native_endpoint, local),
    ] {
        set_fips_peer_configs(
            local,
            vec![FipsPeerConfig {
                npub: remote.npub().to_owned(),
                udp_addresses: vec![remote.bound_udp_listen_addrs().await?[0].to_string()],
            }],
        )
        .await?;
    }
    let subscription = observer
        .subscribe(vec![Filter::new().kind(nostr::Kind::TextNote)])
        .await?;
    let query_entered =
        tokio::time::timeout(Duration::from_secs(5), gate.query_entered.notified()).await;
    let stopping = controller.clone();
    let mut stopped = tokio::spawn(async move { stopping.shutdown().await });
    let entered = tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await;
    let admission_closed = client
        .set_replay_source(Some(Arc::new(nostr_pubsub::InMemoryEventBus::new())))
        .is_err();
    let checkpoint_still_owned =
        crate::fips_transport::open_daemon_nostr_cache(&info.store).is_err();
    // Release every gate and join the real shutdown even on the old-order red.
    drop(release);
    let completed = tokio::time::timeout(Duration::from_secs(5), &mut stopped).await;
    if completed.is_err() {
        stopped.abort();
        let _ = stopped.await;
    }
    drop(subscription);
    observer.shutdown().await;
    remote.native_endpoint.shutdown().await?;
    query_entered.context("real peer did not start a retained replay query")?;
    entered.context("replay source release boundary was not reached")?;
    completed.context("full shutdown did not finish after releasing the gate")??;
    assert!(
        admission_closed,
        "the replay source must drop during client shutdown"
    );
    assert!(
        gate.dropped_before_query_timeout.load(Ordering::SeqCst),
        "query expiry must not substitute for replay worker shutdown"
    );
    assert!(
        checkpoint_still_owned,
        "provider released writer lease before FIPS replay workers stopped"
    );

    // Retained external handles must neither keep the lease nor admit new work.
    assert!(provider
        .query(vec![Filter::new()], QueryOptions::default())
        .await
        .is_err());
    assert!(client.subscribe(vec![Filter::new()]).await.is_err());
    let reopened = crate::fips_transport::open_daemon_nostr_cache(&info.store)?;
    let retained = reopened
        .query(vec![Filter::new().id(event.id)], QueryOptions::default())
        .await?;
    assert_eq!(retained.events.len(), 1);
    assert_eq!(retained.events[0].event.as_event(), &event);
    Ok(())
}
