use super::*;
use fips_core::config::{
    IdentityConfig, PeerAddress, PeerAdmissionConfig, PeerConfig, TransportInstances, UdpConfig,
};
use nostr::Kind;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_public_author_receiver_reads_daemon_root_and_revokes_provider() -> Result<()> {
    let scope = format!("htree-strict-reader-{}", uuid::Uuid::new_v4());
    let rendezvous = reserve_local_rendezvous_addr();
    let anchor = local_only_endpoint(rendezvous, &scope).await;
    let identified = local_only_endpoint(rendezvous, &scope).await;
    let temp = tempfile::tempdir()?;
    let store = Arc::new(HashtreeStore::new(temp.path().join("blobs"))?);
    let author = nostr::Keys::generate();
    let root = NostrRootResolver::root_event_builder(
        "releases/strict-reader",
        &hashtree_core::Cid::public([8; 32]),
        None,
    )
    .custom_created_at(Timestamp::from(1))
    .sign_with_keys(&author)?;
    anyhow::ensure!(
        root.kind == Kind::Custom(30064),
        "exercise current Hashtree roots"
    );
    let key = format!(
        "{}/releases/strict-reader",
        author.public_key().to_bech32()?
    );
    let cache = open_daemon_nostr_cache(&store)?;
    cache
        .publish(
            VerifiedEvent::try_from(root.clone())?,
            EventSource::local_index("strict-reader-retained-root"),
        )
        .await?;
    drop(cache);
    let cache = open_daemon_nostr_cache(&store)?;
    let mut config = provider_config(&scope, 1);
    config.server.fips_local_rendezvous_addr = Some(rendezvous.to_string());
    config.nostr.retained_roots = vec![key.clone()];
    let daemon = start_daemon_fips_transport(&config, &nostr::Keys::generate(), store, Vec::new())
        .await?
        .context("daemon endpoint")?;
    let provider = start_daemon_nostr_provider(&config, Some(&daemon), None, Some(cache))
        .await?
        .context("retained root provider")?;

    let result = check_strict_reader(&identified, &anchor, &daemon, &author, &key, &root).await;
    provider.shutdown().await;
    daemon.shutdown().await;
    identified.native_endpoint.shutdown().await?;
    anchor.native_endpoint.shutdown().await?;
    result
}

async fn check_strict_reader(
    identified: &BoundFipsEndpoint,
    anchor: &BoundFipsEndpoint,
    daemon: &DaemonFipsHandle,
    author: &nostr::Keys,
    key: &str,
    root: &nostr::Event,
) -> Result<()> {
    timeout(Duration::from_secs(15), async {
        loop {
            let ready = identified
                .native_endpoint
                .local_instance_advertisements()?
                .iter()
                .any(|advert| {
                    advert.npub == daemon.endpoint_npub
                        && advert
                            .capability(nostr_pubsub_fips::FIPS_NOSTR_PUBSUB_CAPABILITY)
                            .is_some()
                });
            if ready {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("daemon capability discovery timed out")??;
    anyhow::ensure!(
        identified
            .native_endpoint
            .peers()
            .await?
            .iter()
            .all(|peer| !peer.connected || peer.npub != daemon.endpoint_npub),
        "provider hint must arrive through the authenticated anchor roster"
    );
    let addresses = identified
        .native_endpoint
        .authenticated_local_udp_listen_addrs(PeerIdentity::from_npub(&daemon.endpoint_npub)?)
        .await?;
    let listening = daemon.endpoint.bound_udp_listen_addrs().await?;
    anyhow::ensure!(
        !addresses.is_empty()
            && addresses
                .iter()
                .all(|address| address.ip().is_loopback() && listening.contains(address)),
        "daemon must expose an authenticated accepting loopback listener"
    );

    let mut config = fips_core::Config::new();
    config.node.identity = IdentityConfig {
        nsec: Some(nostr::Keys::generate().secret_key().to_bech32()?),
        persistent: false,
    };
    config.node.strict_client_only = true;
    let mut denied = vec![
        identified.local_peer_id.clone(),
        anchor.local_peer_id.clone(),
    ];
    config.node.peer_admission = PeerAdmissionConfig {
        configured_only: true,
        denied_npubs: denied.clone(),
    };
    config.node.discovery.nostr.enabled = false;
    config.node.discovery.local.enabled = false;
    config.node.discovery.lan.enabled = false;
    config.transports.udp = TransportInstances::Single(UdpConfig {
        bind_addr: Some("127.0.0.1:0".into()),
        advertise_on_nostr: Some(false),
        accept_connections: Some(false),
        ..Default::default()
    });
    let mut peer = PeerConfig::new(&daemon.endpoint_npub, "udp", addresses[0].to_string());
    peer.addresses = addresses
        .iter()
        .map(|address| PeerAddress::new("udp", address.to_string()).authenticated())
        .collect();
    config.peers = vec![peer];
    let endpoint = Arc::new(
        Box::pin(
            FipsEndpoint::builder()
                .config(config)
                .without_system_tun()
                .bind(),
        )
        .await?,
    );
    // Current kind-30064 roots use the explicit public-service author contract.
    // The anonymous-discovery role's allowlist does not include this event kind.
    // This receiver may export allowed public events; it is not a never-exporting lookup.
    let reader = FipsPubsubClient::start_receive_only_with_public_authors(
        endpoint.clone(),
        FipsPubsubClientOptions {
            query_timeout: Duration::from_secs(8),
            routed_peers: vec![daemon.endpoint_npub.clone()],
            max_connected_peers: 1,
            fanout: 1,
            ..Default::default()
        },
        vec![author.public_key()],
    )
    .await?;
    let result = async {
        let mut subscription = reader
            .subscribe(vec![NostrRootResolver::filter_for_key(key)?])
            .await?;
        let event = timeout(Duration::from_secs(15), subscription.recv())
            .await
            .context("strict reader did not receive the daemon's stored root")?
            .context("strict reader subscription closed")?;
        anyhow::ensure!(
            event.event.as_event().as_json() == root.as_json(),
            "signed root changed"
        );
        anyhow::ensure!(endpoint.routing_status().await?.client_only);
        anyhow::ensure!(endpoint.bound_udp_listen_addrs().await?.is_empty());
        let connected: Vec<_> = endpoint
            .peers()
            .await?
            .into_iter()
            .filter(|peer| peer.connected)
            .map(|peer| peer.npub)
            .collect();
        anyhow::ensure!(connected == vec![daemon.endpoint_npub.clone()]);
        anyhow::ensure!(reader.delivery_snapshot().req_frames_received == 0);
        denied.push(daemon.endpoint_npub.clone());
        endpoint
            .update_peer_admission(PeerAdmissionConfig {
                configured_only: true,
                denied_npubs: denied,
            })
            .await?;
        anyhow::ensure!(
            endpoint.peers().await?.iter().all(|peer| !peer.connected),
            "revoking the only configured provider must immediately remove its path"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    reader.shutdown().await;
    endpoint.shutdown().await?;
    result
}
