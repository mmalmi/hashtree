use super::*;
use hashtree_cli::fips_transport::{start_daemon_fips_transport, start_daemon_nostr_provider};
use hashtree_cli::{HashtreeServer, HashtreeStore, NostrToBech32};
use hashtree_core::{DirEntry, HashTree, HashTreeConfig, LinkType};
use hashtree_fips_transport::{
    bind_fips_endpoint, set_fips_peer_configs, FipsEndpointOptions, FipsPeerConfig,
};
use hashtree_updater::PubsubRootResolver;
use nostr_pubsub_fips::FipsPubsubClient;
use std::sync::Arc;

#[test]
fn release_daemon_url_stays_loopback() {
    assert_eq!(
        daemon_url("0.0.0.0:8080").unwrap().as_str(),
        "http://127.0.0.1:8080/"
    );
    assert_eq!(
        daemon_url("[::]:8080").unwrap().as_str(),
        "http://[::1]:8080/"
    );
    assert_eq!(
        daemon_url("localhost:8080").unwrap().as_str(),
        "http://127.0.0.1:8080/"
    );
    assert!(daemon_url("192.0.2.1:8080").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_daemon_handoff_reaches_late_fips_consumer_without_relays() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = Arc::new(HashtreeStore::new(temp.path().join("db"))?);
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let old_release = tree.put_directory(Vec::new()).await?;
    let (asset, size) = tree.put_file(b"new release asset").await?;
    let new_release = tree
        .put_directory(vec![DirEntry::from_cid("release.bin", &asset)
            .with_size(size)
            .with_link_type(LinkType::File)])
        .await?;
    let old_root =
        super::super::publish_release_root(&tree, None, "v1", &old_release, false).await?;
    let keys = NostrKeys::generate();
    let key = format!("{}/releases/test", keys.public_key().to_bech32()?);
    let prior_timestamp = Timestamp::from_secs(Timestamp::now().as_secs() + 1);
    let prior_event = NostrRootResolver::root_event_builder("releases/test", &old_root, None)
        .custom_created_at(prior_timestamp)
        .sign_with_keys(&keys)?;

    let mut config = Config::default();
    config.nostr.event_transport = NostrEventTransport::FipsLocalOnly;
    config.nostr.relays.clear();
    config.server.fips_relays = Some(Vec::new());
    config.server.fips_websocket_seed_urls = Some(Vec::new());
    config.server.fips_udp_bind_addr = Some("127.0.0.1:0".into());
    config.server.enable_fips_webrtc = false;
    config.server.enable_fips_lan_discovery = false;
    config.server.fips_request_timeout_ms = 300;
    let rendezvous = std::net::UdpSocket::bind("127.0.0.1:0")?;
    config.server.fips_local_rendezvous_addr = Some(rendezvous.local_addr()?.to_string());
    drop(rendezvous);
    let daemon =
        start_daemon_fips_transport(&config, &NostrKeys::generate(), store.clone(), Vec::new())
            .await?
            .context("daemon FIPS")?;
    let provider = start_daemon_nostr_provider(&config, Some(&daemon), None)
        .await?
        .context("daemon provider")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    config.server.bind_address = listener.local_addr()?.to_string();
    let base = daemon_url(&config.server.bind_address)?;
    let server = HashtreeServer::new(store, config.server.bind_address.clone())
        .with_nostr_provider(provider)
        .with_nostr_event_transport(config.nostr.event_transport);
    let server_task = tokio::spawn(server.run_with_listener(listener));
    let http = reqwest::Client::new();
    // A migration explicitly hands the existing signed release root to the daemon.
    http.post(base.join("api/nostr/events")?)
        .json(&prior_event)
        .send()
        .await?
        .error_for_status()?;
    let mismatch = http
        .post(base.join("api/nostr/events?transport=relay")?)
        .json(&prior_event)
        .send()
        .await?;
    assert_eq!(mismatch.status(), reqwest::StatusCode::CONFLICT);

    let publisher = ReleasePublisher::connect(&config, keys.clone()).await?;
    assert!(publisher
        .resolve(&format!("{}/unobserved", keys.public_key().to_bech32()?))
        .await
        .is_err());
    let (current_root, created_at) = publisher.resolve(&key).await?;
    assert_eq!(current_root, Some(old_root));
    assert_eq!(created_at, Some(prior_timestamp));
    let new_root =
        super::super::publish_release_root(&tree, current_root, "v2", &new_release, false).await?;
    publisher.publish(&key, &new_root, created_at).await?;
    drop(publisher);

    // Start the consumer after the publishing command has exited: the daemon
    // must still serve the signed event through the real FIPS WANT exchange.
    let mut options = FipsEndpointOptions::new(NostrKeys::generate().secret_key().to_bech32()?);
    options.enable_webrtc = false;
    options.enable_lan_discovery = false;
    options.share_local_candidates = false;
    options.udp_bind_addr = Some("127.0.0.1:0".into());
    let consumer_endpoint = bind_fips_endpoint(options).await?;
    set_fips_peer_configs(
        consumer_endpoint.native_endpoint.as_ref(),
        vec![FipsPeerConfig {
            npub: daemon.endpoint_npub.clone(),
            udp_addresses: vec![daemon.endpoint.bound_udp_listen_addrs().await?[0].to_string()],
        }],
    )
    .await?;
    let consumer = Arc::new(
        FipsPubsubClient::start(
            consumer_endpoint.native_endpoint.clone(),
            Default::default(),
        )
        .await?,
    );
    let resolver = PubsubRootResolver::new(
        Arc::new(consumer.fresh_subscriber()),
        Duration::from_secs(8),
    );
    assert_eq!(resolver.resolve(&key).await?, Some(new_root.clone()));
    let observed = resolver
        .latest_event(&key)
        .await?
        .context("signed release event")?;
    assert_eq!(observed.created_at.as_secs(), prior_timestamp.as_secs() + 1);
    assert_eq!(
        NostrRootResolver::root_from_event(&key, &observed)?,
        Some(new_root.clone())
    );
    assert_eq!(
        tree.resolve_path(&new_root, "v1").await?.unwrap(),
        old_release
    );
    assert_eq!(
        tree.resolve_path(&new_root, "v2").await?.unwrap(),
        new_release
    );
    assert_eq!(
        tree.resolve_path(&new_root, "latest").await?.unwrap(),
        new_release
    );

    drop(resolver);
    consumer.shutdown_shared().await;
    consumer_endpoint.native_endpoint.shutdown().await?;
    server_task.abort();
    daemon.shutdown().await;
    Ok(())
}
