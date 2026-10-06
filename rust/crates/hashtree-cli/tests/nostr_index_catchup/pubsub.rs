use super::*;
use hashtree_fips_transport::{bind_fips_endpoint_at_local_rendezvous, FipsEndpointOptions};
use hashtree_nostr_pubsub::HashtreeNostrBoundedEventCache;
use nostr::nips::nip19::ToBech32;
use nostr_pubsub::{EventBus, EventRetentionPolicy, EventSource, VerifiedEvent};
use nostr_pubsub_fips::FipsPubsubClient;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_indexes_peer_only_event_and_preserves_relay_coverage_and_resume_identity() {
    let temp = TempDir::new().unwrap();
    let alice = Keys::generate();
    let old = event(&alice, 1, "archive");
    let shared = event(&alice, 20, "relay and peer");
    let peer_only = event(&alice, 30, "P2P only");
    let root = import(&temp, &old);
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n", alice.public_key().to_hex()),
    )
    .unwrap();
    let relay = Relay::new(vec![shared.clone()]).await;
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let scope = format!("catchup-test-{}", uuid::Uuid::new_v4());
    let mut options = FipsEndpointOptions::new(Keys::generate().secret_key().to_bech32().unwrap());
    options.discovery_scope = scope.clone();
    options.enable_udp = false;
    options.enable_webrtc = false;
    options.enable_lan_discovery = false;
    options.enable_local_rendezvous = true;
    options.share_local_candidates = false;
    let endpoint =
        bind_fips_endpoint_at_local_rendezvous(options, addr.to_string().parse().unwrap())
            .await
            .unwrap();
    let publisher = FipsPubsubClient::start(
        endpoint.native_endpoint.clone(),
        nostr_pubsub_fips::FipsPubsubClientOptions {
            max_inbound_routed_peers: 8,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let cache = Arc::new(HashtreeNostrBoundedEventCache::new(
        Arc::new(hashtree_core::MemoryStore::new()),
        None,
        EventSource::local_index("test-history"),
        EventRetentionPolicy::new(100, vec![]),
    ));
    for event in [&shared, &peer_only] {
        cache
            .publish(
                VerifiedEvent::try_from(event.clone()).unwrap(),
                EventSource::local_index("test-history"),
            )
            .await
            .unwrap();
    }
    publisher.set_replay_source(Some(cache)).unwrap();
    std::fs::write(temp.path().join("config/config.toml"), format!(
        "[storage]\nmax_size_gb = 1\nevict_orphans = false\n[server]\nfips_discovery_scope = {scope:?}\nfips_local_rendezvous_addr = {addr:?}\nfips_request_timeout_ms = 800\n",
        addr = addr.to_string()
    )).unwrap();
    let mut cmd = catchup(&temp, &root, &relay);
    cmd.args(["--until", "100", "--pubsub-peer", &endpoint.local_peer_id]);
    let output = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    let result = success(output);
    assert_eq!(
        result["events_received"], 2,
        "peer-only event must enter the actual ordered writer"
    );
    let saved = checkpoint(&temp);
    let head = saved["coverage_head"].as_str().unwrap();
    let receipt: Value = serde_json::from_slice(
        &std::fs::read(
            temp.path()
                .join(format!("data/nostr-index/catchup-coverage/{head}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["sources"][0]["status"], "complete");
    assert_eq!(
        receipt["sources"].as_array().unwrap().len(),
        1,
        "peer is not a complete relay"
    );
    assert_eq!(receipt["pubsub"]["status"], "observed");
    assert_eq!(receipt["pubsub"]["added_events"], 1);
    let query = success(
        command(&temp)
            .args([
                "nostr-index",
                "query",
                "--root",
                result["root"].as_str().unwrap(),
                "--filter",
                "{\"kinds\":[1]}",
            ])
            .output()
            .unwrap(),
    );
    let raw = serde_json::to_string(&query).unwrap();
    assert!(raw.contains(&peer_only.id.to_hex()));
    assert!(raw.contains(&old.id.to_hex()));
    let before = std::fs::read(temp.path().join("data/nostr-index/catchup-state.json")).unwrap();
    let rejected = catchup(&temp, &root, &relay)
        .args(["--until", "100"])
        .output()
        .unwrap();
    assert!(
        !rejected.status.success(),
        "cannot silently remove pubsub from an existing pass"
    );
    assert_eq!(
        std::fs::read(temp.path().join("data/nostr-index/catchup-state.json")).unwrap(),
        before
    );
    // P2P delivery does not rescue a required relay that never completes.
    relay.omit_eose.store(true, Ordering::SeqCst);
    let mut cmd = catchup(&temp, &root, &relay);
    cmd.args(["--until", "101", "--pubsub-peer", &endpoint.local_peer_id]);
    let rejected = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert!(!rejected.status.success());
    let failed = checkpoint(&temp);
    assert_eq!(failed["next_author"], 0);
    assert_eq!(failed["root"], saved["root"]);
    assert_eq!(failed["events_received"], saved["events_received"]);
    assert_eq!(failed["coverage_head"], saved["coverage_head"]);
    relay.omit_eose.store(false, Ordering::SeqCst);
    let mut cmd = catchup(&temp, &root, &relay);
    cmd.args(["--until", "101", "--pubsub-peer", &endpoint.local_peer_id]);
    let resumed = success(
        tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(resumed["next_author"], 1);
    publisher.shutdown_shared().await;
    endpoint.native_endpoint.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_quiet_peer_keeps_relay_events_without_peer_completeness_claim() {
    let temp = TempDir::new().unwrap();
    let alice = Keys::generate();
    let root = import(&temp, &event(&alice, 1, "archive"));
    std::fs::write(
        temp.path().join("authors.txt"),
        format!("{}\n", alice.public_key().to_hex()),
    )
    .unwrap();
    let relay = Relay::new(vec![event(&alice, 20, "relay only")]).await;
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    std::fs::write(temp.path().join("config/config.toml"), format!(
        "[storage]\nmax_size_gb = 1\nevict_orphans = false\n[server]\nfips_discovery_scope = \"quiet-peer-test\"\nfips_local_rendezvous_addr = {addr:?}\nfips_request_timeout_ms = 250\n", addr = addr.to_string()
    )).unwrap();
    let mut cmd = catchup(&temp, &root, &relay);
    cmd.args([
        "--until",
        "100",
        "--source-mode",
        "best-effort",
        "--pubsub-peer",
        &Keys::generate().public_key().to_bech32().unwrap(),
    ]);
    let result = success(
        tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(result["events_received"], 1);
    let saved = checkpoint(&temp);
    let head = saved["coverage_head"].as_str().unwrap();
    let receipt: Value = serde_json::from_slice(
        &std::fs::read(
            temp.path()
                .join(format!("data/nostr-index/catchup-coverage/{head}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["sources"].as_array().unwrap().len(), 1);
    assert_eq!(receipt["sources"][0]["status"], "complete");
    assert_eq!(receipt["pubsub"]["events"], 0);
    assert!(receipt["pubsub"]["sources"].as_object().unwrap().is_empty());
    assert_ne!(receipt["pubsub"]["status"], "complete");
}
