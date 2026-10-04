mod transit;

use super::*;
use crate::NostrRootResolver;
use nostr::{JsonUtil, Timestamp};
use nostr_pubsub::{EventBus, NostrEventSubscriber, QueryEvent, VerifiedEvent};
use transit::Transit;

fn provider_config(scope: &str, admission: usize) -> Config {
    let mut config = Config::default();
    config.server.fips_discovery_scope = scope.into();
    config.server.fips_relays = Some(Vec::new());
    config.server.fips_websocket_seed_urls = Some(Vec::new());
    config.server.enable_fips_udp = true;
    config.server.fips_udp_bind_addr = Some(reserve_udp_addr());
    config.server.enable_fips_webrtc = false;
    config.server.enable_fips_lan_discovery = false;
    config.server.fips_local_rendezvous_addr = Some(reserve_local_rendezvous_addr().to_string());
    config.nostr.relays.clear();
    config.nostr.event_transport = NostrEventTransport::FipsLocalOnly;
    config.nostr.fips_pubsub_max_inbound_routed_peers = admission;
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_public_admission_limit_releases_bound_endpoint() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let scope = format!("htree-invalid-admission-{}", uuid::Uuid::new_v4());
    let config = provider_config(&scope, usize::MAX);
    let result = start_daemon_fips_transport(
        &config,
        &nostr::Keys::generate(),
        Arc::new(HashtreeStore::new(temp.path())?),
        Vec::new(),
    )
    .await;
    let error = result.err().expect("oversized admission must fail startup");
    assert!(format!("{error:#}").contains("max_inbound_routed_peers"));
    let _socket = std::net::UdpSocket::bind(config.server.fips_udp_bind_addr.as_ref().unwrap())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_client_replays_retained_root_through_transit_after_restart() -> Result<()> {
    let scope = format!("htree-public-provider-{}", uuid::Uuid::new_v4());
    let mut transit = Transit::local(&scope).await;
    let result = retained_root_rounds(&scope, &mut transit, 1, false).await;
    transit.shutdown().await?;
    result
}

// This explicit release gate runs old published provider/client dependencies
// against a separately built transit with the concurrent-discovery fix.
#[cfg(feature = "public-provider-stress")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sixteen_unknown_clients_replay_retained_root_through_transit_after_restart() -> Result<()>
{
    let scope = format!("htree-public-provider-{}", uuid::Uuid::new_v4());
    let mut transit = Transit::process(&scope).await?;
    let result = retained_root_rounds(&scope, &mut transit, 16, true).await;
    transit.shutdown().await?;
    result
}

async fn retained_root_rounds(
    scope: &str,
    transit: &mut Transit,
    clients: usize,
    check_capacity: bool,
) -> Result<()> {
    // Transit has no pubsub service or event cache.
    let temp = tempfile::tempdir()?;
    let store = Arc::new(HashtreeStore::new(temp.path().join("blobs"))?);
    let author = nostr::Keys::generate();
    let provider_keys = nostr::Keys::generate();
    let cid = hashtree_core::Cid::public([7; 32]);
    let root = NostrRootResolver::root_event_builder("releases/app", &cid, None)
        .custom_created_at(Timestamp::from(1))
        .sign_with_keys(&author)?;
    let key = format!("{}/releases/app", author.public_key().to_bech32()?);
    let cache = open_daemon_nostr_cache(&store)?;
    cache
        .publish(
            VerifiedEvent::try_from(root.clone())?,
            EventSource::local_index("retained-public-root"),
        )
        .await?;
    drop(cache);
    let mut config = provider_config(scope, clients);
    config.nostr.retained_roots = vec![key.clone()];
    config.server.fips_peers = vec![crate::config::ConfiguredFipsPeer {
        npub: transit.npub().into(),
        udp_addresses: vec![transit.address().into()],
    }];
    assert!(config.nostr.fips_pubsub_peers.is_empty());
    for round in 0..2 {
        // Reopen the durable index and restart the actual endpoint, using
        // fresh client identities in each round. No client or hot provider
        // cache can satisfy the signed root read from the previous round.
        let cache = open_daemon_nostr_cache(&store)?;
        let daemon =
            start_daemon_fips_transport(&config, &provider_keys, store.clone(), Vec::new())
                .await?
                .expect("public provider endpoint");
        let provider = start_daemon_nostr_provider(&config, Some(&daemon), None, Some(cache))
            .await?
            .expect("retained root provider");
        let began = std::time::Instant::now();
        let result = query_fresh_clients(
            scope,
            transit,
            &daemon,
            &key,
            &root,
            clients,
            check_capacity,
        )
        .await;
        provider.shutdown().await;
        daemon.shutdown().await;
        result?;
        println!(
                "public retained provider round={round} clients={clients} overflow_denied={} elapsed_ms={}",
                usize::from(check_capacity),
                began.elapsed().as_millis()
            );
    }
    Ok(())
}

async fn query_fresh_clients(
    scope: &str,
    transit: &mut Transit,
    daemon: &DaemonFipsHandle,
    key: &str,
    root: &nostr::Event,
    admitted: usize,
    check_capacity: bool,
) -> Result<()> {
    let mut clients = Vec::new();
    for _ in 0..admitted + usize::from(check_capacity) {
        let (endpoint, addr) = udp_endpoint(scope).await;
        let client = Arc::new(
            FipsPubsubClient::start(
                endpoint.native_endpoint.clone(),
                FipsPubsubClientOptions {
                    // Match the native release metadata lookup deadline.
                    query_timeout: Duration::from_secs(8),
                    ..Default::default()
                },
            )
            .await?,
        );
        clients.push((endpoint, addr, client));
    }
    let result = async {
        let mut transit_peers = vec![FipsPeerConfig {
            npub: daemon.endpoint_npub.clone(),
            udp_addresses: daemon
                .endpoint
                .bound_udp_listen_addrs()
                .await?
                .iter()
                .map(ToString::to_string)
                .collect(),
        }];
        for (endpoint, addr, _) in &clients {
            set_fips_peer_configs(
                endpoint.native_endpoint.as_ref(),
                vec![FipsPeerConfig {
                    npub: transit.npub().into(),
                    udp_addresses: vec![transit.address().into()],
                }],
            )
            .await?;
            transit_peers.push(FipsPeerConfig {
                npub: endpoint.local_peer_id.clone(),
                udp_addresses: vec![addr.clone()],
            });
        }
        transit.configure(transit_peers).await?;
        wait_for_link(&daemon.endpoint, transit.npub()).await?;
        for (endpoint, _, _) in &clients {
            wait_for_link(&endpoint.native_endpoint, transit.npub()).await?;
        }
        let queries_started = std::time::Instant::now();
        let events = futures::future::try_join_all(
            clients[..admitted]
                .iter()
                .map(|(_, _, client)| query_fresh_root(client, &daemon.endpoint_npub, key)),
        )
        .await?;
        println!(
            "public retained provider concurrent_read_ms={} clients={admitted}",
            queries_started.elapsed().as_millis()
        );
        if events.iter().any(Option::is_none) {
            eprintln!("received_roots={} provider_connected_tcp={} provider_subscriptions={} provider_query_timeout_ms={} provider={:?}",
                events.iter().filter(|event| event.is_some()).count(),
                daemon.pubsub_client.as_ref().unwrap().connected_peer_count()?,
                daemon.pubsub_client.as_ref().unwrap().peer_subscription_count()?,
                daemon.pubsub_client.as_ref().unwrap().options().query_timeout.as_millis(),
                daemon.pubsub_client.as_ref().unwrap().delivery_snapshot());
            for (index, (endpoint, _, client)) in clients.iter().enumerate().take(admitted) {
                let links = endpoint.native_endpoint.peers().await?
                    .iter()
                    .map(|peer| (peer.connected, peer.packets_sent, peer.packets_recv, peer.last_outbound_route.clone()))
                    .collect::<Vec<_>>();
                eprintln!("client={index} received={} connected_tcp={} subscriptions={} snapshot={:?} links={links:?}", events[index].is_some(), client.connected_peer_count()?, client.peer_subscription_count()?, client.delivery_snapshot());
            }
        }
        for event in events {
            let event = event.context("admitted client did not receive the retained root")?;
            anyhow::ensure!(
                event.event.as_event().as_json() == root.as_json(),
                "signed root changed"
            );
        }
        if check_capacity {
            // This additional client stays idle until all slots are occupied.
            // Rejection is not a claim that the requested root is absent.
            anyhow::ensure!(
                query_fresh_root(&clients[admitted].2, &daemon.endpoint_npub, key)
                    .await?
                    .is_none(),
                "an extra routed client exceeded the admission limit"
            );
        }
        // Fresh subscriptions cannot succeed from the clients' own hot caches.
        // The admitted clients must still obtain the unchanged signed event.
        let events = futures::future::try_join_all(
            clients[..admitted]
                .iter()
                .map(|(_, _, client)| query_fresh_root(client, &daemon.endpoint_npub, key)),
        )
        .await?;
        for event in events {
            let event = event.context("admitted client stopped receiving fresh root replay")?;
            anyhow::ensure!(
                event.event.as_event().as_json() == root.as_json(),
                "signed root changed"
            );
        }
        let pubsub = daemon.pubsub_client.as_ref().unwrap();
        anyhow::ensure!(pubsub.connected_peer_count()? <= pubsub.options().max_connected_peers);
        anyhow::ensure!(pubsub.options().routed_peers.is_empty());
        anyhow::ensure!(daemon
            .endpoint
            .peers()
            .await?
            .iter()
            .all(|peer| !peer.connected
                || clients
                    .iter()
                    .all(|(endpoint, _, _)| peer.npub != endpoint.local_peer_id)));
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for (endpoint, _, client) in clients {
        client.shutdown_shared().await;
        endpoint.native_endpoint.shutdown().await?;
    }
    result
}

async fn query_fresh_root(
    client: &Arc<FipsPubsubClient>,
    provider: &str,
    key: &str,
) -> Result<Option<QueryEvent>> {
    client.set_routed_peers(vec![provider.into()])?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let subscription = client
        .fresh_subscriber()
        .subscribe(
            vec![NostrRootResolver::filter_for_key(key)?],
            Arc::new(move |event| {
                let _ = sender.try_send(event);
            }),
        )
        .await?;
    let result = timeout(client.options().query_timeout, receiver.recv()).await;
    subscription.close().await?;
    Ok(result.ok().flatten())
}

async fn wait_for_link(endpoint: &FipsEndpoint, peer: &str) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            if endpoint
                .peers()
                .await?
                .iter()
                .any(|entry| entry.connected && entry.npub == peer)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("local transit link did not connect")??;
    Ok(())
}
