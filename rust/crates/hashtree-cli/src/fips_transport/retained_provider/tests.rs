use super::*;
use nostr::Timestamp;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_relay_head_is_retained_without_resigning_or_broad_intake() -> Result<()> {
    let relay = super::test_common::test_relay::TestRelay::new();
    let publisher = nostr_sdk::Client::default();
    publisher.add_relay(relay.url()).await?;
    publisher.connect().await;
    let keys = nostr::Keys::generate();
    let event = NostrRootResolver::root_event_builder(
        "releases/selected",
        &hashtree_core::Cid::public([3; 32]),
        None,
    )
    .custom_created_at(Timestamp::from(1))
    .sign_with_keys(&keys)?;
    let unrelated = NostrRootResolver::root_event_builder(
        "releases/unrelated",
        &hashtree_core::Cid::public([4; 32]),
        None,
    )
    .sign_with_keys(&keys)?;
    assert!(!publisher.send_event(&event).await?.success.is_empty());
    assert!(!publisher.send_event(&unrelated).await?.success.is_empty());
    publisher.shutdown().await;
    let temp = tempfile::tempdir()?;
    let cache = super::super::durable_cache::open(temp.path())?;
    let mut config = Config::default();
    config.nostr.relays = vec![relay.url()];
    config.nostr.retained_roots = vec![format!(
        "{}/releases/selected",
        keys.public_key().to_bech32()?
    )];
    let tasks = start_intake(&config, cache.clone(), None).await?;
    let observed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let report = cache
                .query(vec![Filter::new()], QueryOptions::default())
                .await?;
            if !report.events.is_empty() {
                return Ok::<_, nostr_pubsub::PubsubError>(report);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    close_intake(tasks, &cache).await;
    let report = observed??;
    assert_eq!(report.events.len(), 1);
    assert_eq!(report.events[0].event.as_event(), &event);
    drop(cache);
    let reopened = super::super::durable_cache::open(temp.path())?;
    let report = reopened
        .query(vec![Filter::new()], QueryOptions::default())
        .await?;
    assert_eq!(report.events.len(), 1);
    assert_eq!(report.events[0].event.as_event(), &event);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_reconciliation_does_not_block_live_intake_or_shutdown() -> Result<()> {
    struct PausedSource(Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl EventBus for PausedSource {
        async fn publish(
            &self,
            _: VerifiedEvent,
            _: EventSource,
        ) -> nostr_pubsub::Result<PublishReport> {
            unreachable!()
        }
        async fn query(
            &self,
            filters: Vec<Filter>,
            _: QueryOptions,
        ) -> nostr_pubsub::Result<QueryReport> {
            assert_eq!(filters.len(), 1);
            self.0.notify_one();
            std::future::pending().await
        }
    }
    let temp = tempfile::tempdir()?;
    let cache = super::super::durable_cache::open(temp.path())?;
    let entered = Arc::new(tokio::sync::Notify::new());
    let (sender, receiver) = tokio::sync::mpsc::channel(128);
    let tasks = vec![
        reconcile(
            Arc::new(PausedSource(entered.clone())),
            vec![Filter::new(); 64],
            sender.clone(),
            true,
        ),
        tokio::spawn(consume_intake(cache.clone(), None, receiver)),
    ];
    tokio::time::timeout(Duration::from_secs(1), entered.notified()).await?;
    let event =
        NostrRootResolver::root_event_builder("live", &hashtree_core::Cid::public([8; 32]), None)
            .sign_with_keys(&nostr::Keys::generate())?;
    sender
        .send((
            nostr_pubsub::QueryEvent {
                event: VerifiedEvent::try_from(event.clone())?,
                source: EventSource::peer("live"),
                priority: 0,
            },
            true,
        ))
        .await?;
    let observed = wait_for_heads(&cache, 1).await;
    close_intake(tasks, &cache).await;
    assert_eq!(observed?.events[0].event.as_event(), &event);
    drop(cache);
    // Explicit shutdown drained the owned blocking operation as well as tasks.
    let reopened = super::super::durable_cache::open(temp.path())?;
    assert_eq!(
        reopened
            .query(vec![Filter::new()], QueryOptions::default())
            .await?
            .events
            .len(),
        1
    );
    Ok(())
}

async fn wait_for_heads(cache: &DaemonNostrCache, count: usize) -> Result<QueryReport> {
    Ok(tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let report = cache
                .query(vec![Filter::new()], QueryOptions::default())
                .await?;
            if report.events.len() >= count {
                return Ok::<_, nostr_pubsub::PubsubError>(report);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_than_four_roots_use_actual_fips_filter_capacity() -> Result<()> {
    let (sender, receiver, publisher, consumer) = client_pair().await?;
    let temp = tempfile::tempdir()?;
    let cache = super::super::durable_cache::open(temp.path())?;
    let mut tasks = Vec::new();
    let result = async {
        let keys = nostr::Keys::generate();
        let mut expected = Vec::new();
        let mut config = Config::default();
        config.nostr.relays.clear();
        for i in 0..6 {
            let name = format!("releases/{i}");
            config
                .nostr
                .retained_roots
                .push(format!("{}/{name}", keys.public_key().to_bech32()?));
            let event = NostrRootResolver::root_event_builder(
                &name,
                &hashtree_core::Cid::public([i; 32]),
                None,
            )
            .custom_created_at(Timestamp::from(1))
            .sign_with_keys(&keys)?;
            publisher
                .publish(
                    VerifiedEvent::try_from(event.clone())?,
                    EventSource::local_index("publisher"),
                )
                .await?;
            expected.push(event);
        }
        tasks = start_intake(&config, cache.clone(), Some(consumer.clone())).await?;
        let report = wait_for_heads(&cache, 6).await?;
        assert_eq!(report.events.len(), 6);
        for event in expected {
            assert!(report
                .events
                .iter()
                .any(|entry| entry.event.as_event() == &event));
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    close_intake(tasks, &cache).await;
    publisher.shutdown_shared().await;
    consumer.shutdown_shared().await;
    sender.native_endpoint.shutdown().await?;
    receiver.native_endpoint.shutdown().await?;
    result
}

async fn client_pair() -> Result<(
    hashtree_fips_transport::BoundFipsEndpoint,
    hashtree_fips_transport::BoundFipsEndpoint,
    Arc<FipsPubsubClient>,
    Arc<FipsPubsubClient>,
)> {
    use hashtree_fips_transport::{
        bind_fips_endpoint, set_fips_peer_configs, FipsEndpointOptions, FipsPeerConfig,
    };
    let mut options = FipsEndpointOptions::new(nostr::Keys::generate().secret_key().to_bech32()?);
    options.enable_webrtc = false;
    options.enable_lan_discovery = false;
    options.enable_local_rendezvous = false;
    options.share_local_candidates = false;
    options.relays.clear();
    options.udp_bind_addr = Some("127.0.0.1:0".into());
    let sender = bind_fips_endpoint(options.clone()).await?;
    options.identity_nsec = nostr::Keys::generate().secret_key().to_bech32()?;
    let receiver = bind_fips_endpoint(options).await?;
    for (local, remote) in [(&sender, &receiver), (&receiver, &sender)] {
        set_fips_peer_configs(
            local.native_endpoint.as_ref(),
            vec![FipsPeerConfig {
                npub: remote.local_peer_id.clone(),
                udp_addresses: vec![
                    remote.native_endpoint.bound_udp_listen_addrs().await?[0].to_string()
                ],
            }],
        )
        .await?;
    }
    let publisher = Arc::new(
        FipsPubsubClient::start(sender.native_endpoint.clone(), Default::default()).await?,
    );
    let consumer = Arc::new(
        FipsPubsubClient::start(
            receiver.native_endpoint.clone(),
            nostr_pubsub_fips::FipsPubsubClientOptions {
                max_filters_per_subscription: 2,
                query_timeout: Duration::from_millis(
                    Config::default().server.fips_request_timeout_ms,
                ),
                ..Default::default()
            },
        )
        .await?,
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        while publisher.connected_peer_count()? == 0
            || consumer.connected_peer_count()? == 0
            || publisher.delivery_snapshot().req_frames_received == 0
            || consumer.delivery_snapshot().req_frames_received == 0
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok((sender, receiver, publisher, consumer))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_recovers_a_seen_head_after_hot_body_eviction() -> Result<()> {
    let (sender, receiver, publisher, consumer) = client_pair().await?;
    let temp = tempfile::tempdir()?;
    let source = super::super::durable_cache::open(&temp.path().join("source"))?;
    let cache = super::super::durable_cache::open(&temp.path().join("receiver"))?;
    publisher.set_replay_source(Some(source.clone()))?;
    let mut tasks = Vec::new();
    let result = async {
        let keys = nostr::Keys::generate();
        let mut expected = None;
        let mut first_filter = None;
        for i in 0..16 {
            let name = format!("releases/{i}");
            let filter = NostrRootResolver::filter_for_key(&format!(
                "{}/{name}",
                keys.public_key().to_bech32()?
            ))?
            .limit(1);
            let event = NostrRootResolver::root_event_builder(
                &name,
                &hashtree_core::Cid::public([i; 32]),
                None,
            )
            .custom_created_at(Timestamp::from(1))
            .sign_with_keys(&keys)?;
            let verified = VerifiedEvent::try_from(event.clone())?;
            source
                .publish(verified.clone(), EventSource::local_index("source"))
                .await?;
            publisher
                .publish(verified, EventSource::local_index("source"))
                .await?;
            // Populate the live window through the production open stream.
            // Cold stream setup is separate from the reconciliation deadline.
            let before = consumer.delivery_snapshot();
            let started = std::time::Instant::now();
            let mut subscription = consumer.subscribe(vec![filter.clone()]).await?;
            let observed = tokio::time::timeout(Duration::from_secs(3), subscription.recv()).await;
            subscription.close();
            if i == 0 || i == 15 {
                eprintln!("setup observation {i}: elapsed_us={} before={before:?} after={:?} publisher={:?}",
                    started.elapsed().as_micros(), consumer.delivery_snapshot(), publisher.delivery_snapshot());
            }
            let observed = observed
                .with_context(|| format!("initial stream observation of {i}"))?
                .context("initial stream closed")?;
            anyhow::ensure!(
                observed.event.as_event() == &event,
                "wrong initial event {i}"
            );
            // Deliberately leave the durable receiver empty: the peer has been
            // observed but its downstream commit/admission did not succeed.
            if i == 0 {
                expected = Some(event);
                first_filter = Some(filter);
            }
        }
        assert!(cache
            .query(vec![Filter::new()], QueryOptions::default())
            .await?
            .events
            .is_empty());
        let filter = first_filter.unwrap();
        assert!(
            consumer
                .query(vec![filter.clone()], QueryOptions { limit: Some(1) })
                .await?
                .events
                .is_empty(),
            "ordinary replay cannot refetch a body suppressed by its retained seen-ID"
        );
        let (writer, events) = tokio::sync::mpsc::channel(128);
        let before = consumer.delivery_snapshot();
        let publisher_before = publisher.delivery_snapshot();
        let started = std::time::Instant::now();
        tasks.push(tokio::spawn(consume_intake(cache.clone(), None, events)));
        tasks.push(reconcile_fips(consumer.clone(), vec![filter], writer));
        let recovered = wait_for_heads(&cache, 1).await;
        eprintln!("recovery: elapsed_us={} query_timeout_ms={} persisted={} before={before:?} after={:?} publisher_before={publisher_before:?} publisher_after={:?}",
            started.elapsed().as_micros(), consumer.options().query_timeout.as_millis(), recovered.is_ok(),
            consumer.delivery_snapshot(), publisher.delivery_snapshot());
        let recovered = recovered.context("fresh reconciliation did not persist the evicted head")?;
        let after = consumer.delivery_snapshot();
        assert!(after.want_frames_sent > before.want_frames_sent);
        assert!(after.event_frames_received > before.event_frames_received);
        assert!(publisher.delivery_snapshot().want_frames_received > publisher_before.want_frames_received);
        assert_eq!(
            recovered.events[0].event.as_event(),
            expected.as_ref().unwrap()
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    close_intake(tasks, &cache).await;
    publisher.set_replay_source(None)?;
    publisher.shutdown_shared().await;
    consumer.shutdown_shared().await;
    sender.native_endpoint.shutdown().await?;
    receiver.native_endpoint.shutdown().await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_admitted_query_and_releases_a_retained_handle() -> Result<()> {
    struct PausedProvider {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl EventBus for PausedProvider {
        async fn publish(
            &self,
            _: VerifiedEvent,
            _: EventSource,
        ) -> nostr_pubsub::Result<PublishReport> {
            unreachable!()
        }
        async fn query(
            &self,
            _: Vec<Filter>,
            _: QueryOptions,
        ) -> nostr_pubsub::Result<QueryReport> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(QueryReport::default())
        }
    }
    impl PubsubProvider for PausedProvider {
        fn mode(&self) -> PubsubProviderMode {
            PubsubProviderMode::LocalOnly
        }
    }
    let temp = tempfile::tempdir()?;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(DaemonNostrProvider::new(
        Arc::new(PausedProvider {
            entered: entered.clone(),
            release: release.clone(),
        }),
        Some(super::super::durable_cache::open(temp.path())?),
        None,
        Vec::new(),
    ));
    let reader = provider.clone();
    let query = tokio::spawn(async move {
        reader
            .query(vec![Filter::new()], QueryOptions::default())
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), entered.notified()).await?;
    let blocked = tokio::time::timeout(Duration::from_millis(25), provider.shutdown()).await;
    release.notify_one();
    query.await??;
    assert!(
        blocked.is_err(),
        "shutdown must wait for an admitted operation"
    );
    provider.shutdown().await;
    let reopened = super::super::durable_cache::open(temp.path())?;
    assert!(provider
        .query(vec![Filter::new()], QueryOptions::default())
        .await
        .is_err());
    drop(reopened);
    Ok(())
}
