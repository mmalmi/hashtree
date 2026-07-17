use super::*;
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, MemoryStore, BLOB_MAX_BYTES, BLOB_MAX_HTL};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

type TestStore =
    MeshStoreCore<MemoryStore, crate::mock::MockRelayTransport, crate::mock::MockConnectionFactory>;

struct TestNode {
    store: Arc<TestStore>,
    local_store: Arc<MemoryStore>,
    transport: Arc<crate::mock::MockRelayTransport>,
}

fn mock_network_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn make_test_store(local_store: Arc<MemoryStore>, node_id: &str) -> TestStore {
    make_test_store_with_routing(local_store, node_id, MeshRoutingConfig::default())
}

#[tokio::test]
async fn blob_route_htl_zero_is_strictly_local() {
    let local = Arc::new(MemoryStore::new());
    let route = make_test_store(local.clone(), "blob-route-local");
    let local_data = b"local blob route data".to_vec();
    let local_hash = hashtree_core::sha256(&local_data);
    local
        .put(local_hash, local_data.clone())
        .await
        .expect("put local data");

    assert_eq!(
        route
            .route(BlobRequest {
                hash: local_hash,
                htl: 0,
            })
            .await
            .expect("route local hit"),
        BlobReply::Data(local_data),
    );

    let missing_hash = hashtree_core::sha256(b"strictly local miss");

    assert_eq!(
        route
            .route(BlobRequest {
                hash: missing_hash,
                htl: 0,
            })
            .await
            .expect("route local miss"),
        BlobReply::NoResult,
    );
}

#[tokio::test]
async fn blob_route_rejects_search_budgets_above_the_protocol_bound() {
    let route = make_test_store(Arc::new(MemoryStore::new()), "bounded-blob-route");
    let missing = hashtree_core::sha256(b"bounded HTL");

    assert_eq!(
        route
            .route(BlobRequest {
                hash: missing,
                htl: BLOB_MAX_HTL,
            })
            .await
            .unwrap(),
        BlobReply::NoResult,
    );
    for invalid_htl in [BLOB_MAX_HTL + 1, u8::MAX] {
        assert!(route
            .route(BlobRequest {
                hash: missing,
                htl: invalid_htl,
            })
            .await
            .is_err(),);
    }
}

#[tokio::test]
async fn blob_route_rejects_oversized_local_data() {
    let data = vec![0x42; BLOB_MAX_BYTES + 1];
    let hash = hashtree_core::sha256(&data);
    let local = Arc::new(MemoryStore::new());
    local.put(hash, data.clone()).await.unwrap();
    let local_route = make_test_store(local, "oversized-local-route");
    assert!(local_route
        .route(BlobRequest { hash, htl: 0 })
        .await
        .is_err());
}

#[tokio::test]
async fn concurrent_peer_requests_do_not_overwrite_same_or_different_htl() {
    let store = Arc::new(make_test_store(
        Arc::new(MemoryStore::new()),
        "distinct-peer-budgets",
    ));
    let data = b"one verified response can satisfy both budgets".to_vec();
    let hash = hashtree_core::sha256(&data);
    let low_key = PendingRequestKey::new(hash, 1);
    let high_key = PendingRequestKey::new(hash, 3);
    let (_, low_rx) = store
        .register_pending_request(low_key, vec!["provider".to_string()])
        .await;
    let (_, duplicate_low_rx) = store
        .register_pending_request(low_key, vec!["provider".to_string()])
        .await;
    let (_, high_rx) = store
        .register_pending_request(high_key, vec!["provider".to_string()])
        .await;
    let pending = store.pending_requests.read().await;
    assert_eq!(pending.len(), 2);
    assert_eq!(pending.get(&low_key).map(Vec::len), Some(2));
    drop(pending);
    assert!(store.begin_forward_request(low_key, "low-requester").await);
    assert!(
        store
            .begin_forward_request(high_key, "high-requester")
            .await
    );
    assert_eq!(store.pending_forward_requests.read().await.len(), 2);

    store
        .handle_response_message("provider", create_response(&hash, data.clone()))
        .await;

    assert_eq!(
        low_rx.await.expect("low-budget response"),
        Some(data.clone())
    );
    assert_eq!(
        duplicate_low_rx.await.expect("duplicate response"),
        Some(data.clone())
    );
    assert_eq!(high_rx.await.expect("high-budget response"), Some(data));
    assert!(store.pending_requests.read().await.is_empty());
    assert!(store.pending_forward_requests.read().await.is_empty());
}

fn make_test_store_with_routing(
    local_store: Arc<MemoryStore>,
    node_id: &str,
    routing: MeshRoutingConfig,
) -> TestStore {
    let relay = crate::mock::MockRelay::new();
    let transport = Arc::new(relay.create_transport(node_id.to_string()));
    let conn_factory = Arc::new(crate::mock::MockConnectionFactory::new(
        node_id.to_string(),
        0,
    ));
    let signaling = Arc::new(crate::signaling::MeshRouter::new(
        node_id.to_string(),
        transport,
        conn_factory,
        crate::types::PoolSettings::default(),
        false,
    ));

    TestStore::new_with_routing(
        local_store,
        signaling,
        Duration::from_millis(200),
        false,
        routing,
    )
}

fn make_shared_test_node(
    relay: Arc<crate::mock::MockRelay>,
    node_id: &str,
    routing: MeshRoutingConfig,
) -> TestNode {
    make_shared_test_node_with_hash_get(relay, node_id, routing, true)
}

fn make_shared_test_node_with_hash_get(
    relay: Arc<crate::mock::MockRelay>,
    node_id: &str,
    routing: MeshRoutingConfig,
    hash_get_enabled: bool,
) -> TestNode {
    let transport = Arc::new(relay.create_transport(node_id.to_string()));
    let conn_factory = Arc::new(crate::mock::MockConnectionFactory::new(
        node_id.to_string(),
        0,
    ));
    let mut router = crate::signaling::MeshRouter::new(
        node_id.to_string(),
        transport.clone(),
        conn_factory,
        crate::types::PoolSettings::default(),
        false,
    );
    router.set_hash_get_enabled(hash_get_enabled);
    let signaling = Arc::new(router);
    let local_store = Arc::new(MemoryStore::new());
    let store = Arc::new(TestStore::new_with_routing(
        local_store.clone(),
        signaling,
        Duration::from_millis(120),
        false,
        routing,
    ));

    TestNode {
        store,
        local_store,
        transport,
    }
}

async fn pump_test_signaling(nodes: &[&TestNode]) -> usize {
    let mut processed = 0usize;
    for node in nodes {
        while let Some(msg) = node.transport.try_recv() {
            node.store
                .process_signaling(msg)
                .await
                .expect("process signaling");
            processed += 1;
        }
    }
    processed
}

async fn pump_test_data(nodes: &[&TestNode]) -> usize {
    let mut processed = 0usize;
    for node in nodes {
        let peer_ids = node.store.signaling().peer_ids().await;
        for peer_id in peer_ids {
            let Some(channel) = node.store.signaling().get_channel(&peer_id).await else {
                continue;
            };
            while let Some(data) = channel.try_recv() {
                node.store.handle_data_message(&peer_id, &data).await;
                processed += 1;
            }
        }
    }
    processed
}

async fn pump_test_network(nodes: &[&TestNode], max_steps: usize) {
    for _ in 0..max_steps {
        let signaling = pump_test_signaling(nodes).await;
        let data = pump_test_data(nodes).await;
        if signaling + data == 0 {
            tokio::task::yield_now().await;
        }
    }
}

async fn run_get_with_pumps(
    requester: Arc<TestStore>,
    hash: Hash,
    nodes: &[&TestNode],
) -> Option<Vec<u8>> {
    let task = tokio::spawn(async move { requester.get(&hash).await.ok().flatten() });
    let started = Instant::now();

    loop {
        if task.is_finished() {
            return task.await.expect("request task join");
        }

        if started.elapsed() > Duration::from_secs(1) {
            task.abort();
            return None;
        }

        pump_test_network(nodes, 4).await;
    }
}

async fn run_forwarded_request_with_pumps(
    requester: &TestNode,
    gateway_peer_id: &str,
    hash: Hash,
    nodes: &[&TestNode],
) -> Option<Vec<u8>> {
    let request_key = PendingRequestKey::new(hash, MAX_HTL);
    let (tx, mut rx) = oneshot::channel();
    requester.store.pending_requests.write().await.insert(
        request_key,
        vec![PendingRequest {
            owner: Arc::new(()),
            response_tx: tx,
            started_at: Instant::now(),
            queried_peers: vec![gateway_peer_id.to_string()],
        }],
    );

    let channel = requester
        .store
        .signaling()
        .get_channel(gateway_peer_id)
        .await
        .expect("gateway channel");
    let request = create_request(&hash, MAX_HTL);
    channel
        .send(encode_request(&request))
        .await
        .expect("send forwarded request");

    let started = Instant::now();
    loop {
        if let Ok(Some(data)) = rx.try_recv() {
            return Some(data);
        }

        if started.elapsed() > Duration::from_secs(1) {
            requester
                .store
                .pending_requests
                .write()
                .await
                .remove(&request_key);
            return None;
        }

        pump_test_network(nodes, 4).await;
    }
}

async fn run_bad_peer_series(strategy: SelectionStrategy) -> usize {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-reject",
        MeshRoutingConfig {
            selection_strategy: strategy,
            fairness_enabled: false,
            dispatch: RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 5,
            },
            ..Default::default()
        },
    );
    let bad = make_shared_test_node(
        relay.clone(),
        "a-bad",
        MeshRoutingConfig {
            response_behavior: ResponseBehaviorConfig {
                drop_response_prob: 1.0,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let honest = make_shared_test_node(relay, "b-honest", MeshRoutingConfig::default());
    let nodes = [&requester, &bad, &honest];

    for node in &nodes {
        node.transport
            .connect(&[])
            .await
            .expect("connect transport");
        node.store.start().await.expect("start store");
    }
    pump_test_network(&nodes, 24).await;

    let mut successes = 0usize;
    for round in 0..6 {
        let payload = format!("payload-{round}").into_bytes();
        let hash = hashtree_core::sha256(&payload);
        let _ = bad.local_store.put(hash, payload.clone()).await;
        let _ = honest.local_store.put(hash, payload.clone()).await;

        let result = run_get_with_pumps(requester.store.clone(), hash, &nodes).await;
        if result.as_ref() == Some(&payload) {
            successes += 1;
        }
    }

    crate::mock::clear_channel_registry().await;
    successes
}

#[test]
fn test_default_hedged_wave_plan_is_bounded() {
    let plan = build_hedged_wave_plan(7, RequestDispatchConfig::default());
    assert_eq!(plan, vec![2, 1, 1]);
}

#[test]
fn test_hedged_wave_plan_staged() {
    let plan = build_hedged_wave_plan(
        10,
        RequestDispatchConfig {
            initial_fanout: 2,
            hedge_fanout: 3,
            max_fanout: 8,
            hedge_interval_ms: 25,
        },
    );
    assert_eq!(plan, vec![2, 3, 3]);
}

#[tokio::test]
async fn test_run_hedged_waves_uses_zero_interval_as_immediate_hedge() {
    let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let waits_clone = waits.clone();

    let result = run_hedged_waves(
        3,
        RequestDispatchConfig {
            initial_fanout: 1,
            hedge_fanout: 1,
            max_fanout: 3,
            hedge_interval_ms: 0,
        },
        Duration::from_millis(25),
        |_range| async { 1 },
        move |wait| {
            let waits = waits_clone.clone();
            async move {
                waits.lock().expect("wait lock").push(wait);
                HedgedWaveAction::<()>::Continue
            }
        },
    )
    .await;

    assert!(result.is_none());
    let waits = waits.lock().expect("wait lock");
    assert_eq!(waits.len(), 1);
    assert!(waits[0] <= Duration::from_millis(25));
}

#[test]
fn test_response_behavior_normalization_clamps_probs() {
    let raw = ResponseBehaviorConfig {
        drop_response_prob: -1.5,
        corrupt_response_prob: 9.0,
        extra_delay_ms: 12,
        first_byte_delay_ms: 7,
        bytes_per_second: 1234,
        stall_response_prob: 4.0,
        stall_delay_ms: 55,
    };
    let normalized = raw.normalized();
    assert_eq!(normalized.drop_response_prob, 0.0);
    assert_eq!(normalized.corrupt_response_prob, 1.0);
    assert_eq!(normalized.extra_delay_ms, 12);
    assert_eq!(normalized.first_byte_delay_ms, 7);
    assert_eq!(normalized.bytes_per_second, 1234);
    assert_eq!(normalized.stall_response_prob, 1.0);
    assert_eq!(normalized.stall_delay_ms, 55);
}

#[test]
fn test_response_behavior_delay_models_first_byte_throughput_and_stall() {
    let store = make_test_store_with_routing(
        Arc::new(MemoryStore::new()),
        "delay-model",
        MeshRoutingConfig {
            response_behavior: ResponseBehaviorConfig {
                extra_delay_ms: 12,
                first_byte_delay_ms: 18,
                bytes_per_second: 2000,
                stall_response_prob: 1.0,
                stall_delay_ms: 25,
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let hash = hashtree_core::sha256(b"delay-model-hash");
    let delay = store.response_send_delay(&hash, 5000);
    // 12ms baseline + 18ms first-byte + ceil(5000/2000 * 1000)=2500ms throughput + 25ms forced stall
    assert_eq!(delay, Duration::from_millis(2555));
}

#[test]
fn test_response_scheduler_prefers_helpful_peer_over_queue_order() {
    let ready_at = Instant::now();
    let ready_jobs = vec![
        (1, "leecher".to_string(), 4096usize, ready_at, 1),
        (2, "helper".to_string(), 4096usize, ready_at, 2),
    ];

    let mut stats = HashMap::new();
    stats.insert(
        "leecher".to_string(),
        PeerWireStats {
            bytes_sent: 16 * 1024,
            bytes_received: 0,
            useful_bytes_received: 0,
            bandwidth_debt: 0.0,
        },
    );
    stats.insert(
        "helper".to_string(),
        PeerWireStats {
            bytes_sent: 1024,
            bytes_received: 16 * 1024,
            useful_bytes_received: 16 * 1024,
            bandwidth_debt: 0.0,
        },
    );

    let (selected_job_id, _) = TestStore::choose_ready_response_job(&ready_jobs, &stats)
        .expect("selected ready response job");

    assert_eq!(
        selected_job_id, 2,
        "reciprocity should outrank queue order when payloads are otherwise equal",
    );
}

#[test]
fn test_response_scheduler_ignores_useless_ingress_spam() {
    let ready_at = Instant::now();
    let ready_jobs = vec![
        (1, "spammer".to_string(), 4096usize, ready_at, 1),
        (2, "useful".to_string(), 4096usize, ready_at, 2),
    ];

    let mut stats = HashMap::new();
    stats.insert(
        "spammer".to_string(),
        PeerWireStats {
            bytes_sent: 1024,
            bytes_received: 512 * 1024,
            useful_bytes_received: 0,
            bandwidth_debt: 0.0,
        },
    );
    stats.insert(
        "useful".to_string(),
        PeerWireStats {
            bytes_sent: 1024,
            bytes_received: 16 * 1024,
            useful_bytes_received: 16 * 1024,
            bandwidth_debt: 0.0,
        },
    );

    let (selected_job_id, _) = TestStore::choose_ready_response_job(&ready_jobs, &stats)
        .expect("selected ready response job");

    assert_eq!(
        selected_job_id, 2,
        "raw ingress spam must not buy response scheduling priority",
    );
}

#[tokio::test]
async fn test_pubsub_production_path_delivers_subscribed_stream() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let publisher = make_shared_test_node(relay.clone(), "publisher", MeshRoutingConfig::default());
    let subscriber = make_shared_test_node(relay, "subscriber", MeshRoutingConfig::default());
    let nodes = [&publisher, &subscriber];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    subscriber.store.subscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;
    assert_eq!(
        publisher.store.pubsub_interest_peers("author:alice").await,
        vec!["subscriber".to_string()]
    );

    let stats = publisher
        .store
        .publish_pubsub("author:alice", 1, b"live-bytes".to_vec())
        .await;
    assert_eq!(stats.selected_peers, 1);
    assert_eq!(stats.sent_peers, 1);
    pump_test_network(&nodes, 12).await;

    let events = subscriber.store.drain_pubsub_events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stream_id, "author:alice");
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[0].origin_peer_id, "publisher");
    assert_eq!(events[0].payload, b"live-bytes".to_vec());
    assert!(
        subscriber
            .store
            .peer_traffic_snapshot("publisher")
            .await
            .useful_bytes_received
            > 0
    );

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_pubsub_event_receiver_wakes_on_delivery() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let publisher = make_shared_test_node(relay.clone(), "publisher", MeshRoutingConfig::default());
    let subscriber = make_shared_test_node(relay, "subscriber", MeshRoutingConfig::default());
    let nodes = [&publisher, &subscriber];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    subscriber.store.subscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;

    publisher
        .store
        .publish_pubsub("author:alice", 7, b"event-bytes".to_vec())
        .await;
    pump_test_network(&nodes, 12).await;

    let event = tokio::time::timeout(Duration::from_secs(1), subscriber.store.recv_pubsub_event())
        .await
        .expect("pubsub receiver should wake");

    assert_eq!(event.stream_id, "author:alice");
    assert_eq!(event.seq, 7);
    assert_eq!(event.origin_peer_id, "publisher");
    assert_eq!(event.payload, b"event-bytes".to_vec());

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_pubsub_inv_want_publish_sends_inventory_before_payload() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let publisher = make_shared_test_node(relay.clone(), "publisher", MeshRoutingConfig::default());
    let subscriber = make_shared_test_node(relay, "subscriber", MeshRoutingConfig::default());
    let nodes = [&publisher, &subscriber];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    subscriber.store.subscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;

    let payload = vec![7; 4096];
    let stats = publisher
        .store
        .publish_pubsub("author:alice", 1, payload.clone())
        .await;
    assert_eq!(stats.selected_peers, 1);
    assert_eq!(stats.sent_peers, 1);
    assert!(
        stats.sent_bytes < payload.len() as u64,
        "publish path should send inventory first, not the full payload"
    );

    pump_test_network(&nodes, 24).await;
    let events = subscriber.store.drain_pubsub_events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload, payload);

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_pubsub_inventory_origin_preserves_forwarding_budget() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let publisher = make_shared_test_node(relay.clone(), "publisher", MeshRoutingConfig::default());
    let subscriber = make_shared_test_node(relay, "subscriber", MeshRoutingConfig::default());
    let nodes = [&publisher, &subscriber];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    subscriber.store.subscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;
    publisher.store.htl_configs.write().await.insert(
        "subscriber".to_string(),
        PeerHTLConfig::from_flags(true, true),
    );

    publisher
        .store
        .publish_pubsub("author:alice", 1, b"forwarding-budget".to_vec())
        .await;

    let channel = subscriber
        .store
        .signaling()
        .get_channel("publisher")
        .await
        .expect("publisher channel");
    let message = channel.try_recv().expect("publisher inventory");
    let DataMessage::PubsubInventory(inventory) =
        parse_message(&message).expect("decode inventory")
    else {
        panic!("publisher should send inventory before payload");
    };
    assert_eq!(inventory.htl, MESH_EVENT_POLICY.max_htl);

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_pubsub_inventory_accepts_a_later_higher_budget_route() {
    let store = Arc::new(make_test_store(
        Arc::new(MemoryStore::new()),
        "inventory-router",
    ));
    let low_budget = create_pubsub_inventory("author:alice", 7, "publisher", 512, 1);
    let high_budget = create_pubsub_inventory("author:alice", 7, "publisher", 512, 3);

    store
        .handle_pubsub_inventory_message("long-route", low_budget.clone(), 64)
        .await;
    assert_eq!(
        store
            .pubsub_inventory_routes
            .read()
            .await
            .get("publisher:author:alice:7")
            .map(String::as_str),
        Some("long-route")
    );

    store
        .handle_pubsub_inventory_message("short-route", high_budget, 64)
        .await;
    assert_eq!(
        store
            .pubsub_inventory_routes
            .read()
            .await
            .get("publisher:author:alice:7")
            .map(String::as_str),
        Some("short-route"),
        "a duplicate with more forwarding budget must replace the exhausted route"
    );

    store
        .handle_pubsub_inventory_message("stale-route", low_budget, 64)
        .await;
    assert_eq!(
        store
            .pubsub_inventory_routes
            .read()
            .await
            .get("publisher:author:alice:7")
            .map(String::as_str),
        Some("short-route"),
        "an equal or lower budget must remain deduplicated"
    );
}

#[tokio::test]
async fn test_pubsub_inv_want_inventory_follows_subscription_state() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let publisher = make_shared_test_node(relay.clone(), "publisher", MeshRoutingConfig::default());
    let subscriber = make_shared_test_node(relay, "subscriber", MeshRoutingConfig::default());
    let nodes = [&publisher, &subscriber];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    let stats = publisher
        .store
        .publish_pubsub("author:alice", 1, b"before-sub".to_vec())
        .await;
    assert_eq!(stats.sent_peers, 0);
    pump_test_network(&nodes, 12).await;
    assert!(subscriber.store.drain_pubsub_events().await.is_empty());

    subscriber.store.subscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;

    let stats = publisher
        .store
        .publish_pubsub("author:alice", 2, b"while-subbed".to_vec())
        .await;
    assert_eq!(stats.sent_peers, 1);
    pump_test_network(&nodes, 24).await;
    let events = subscriber.store.drain_pubsub_events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload, b"while-subbed".to_vec());

    subscriber.store.unsubscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;

    let stats = publisher
        .store
        .publish_pubsub("author:alice", 3, b"after-unsub".to_vec())
        .await;
    assert_eq!(stats.sent_peers, 0);
    pump_test_network(&nodes, 12).await;
    assert!(subscriber.store.drain_pubsub_events().await.is_empty());

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_pubsub_spam_ingress_does_not_buy_reciprocity_credit() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let spammer = make_shared_test_node(relay.clone(), "spammer", MeshRoutingConfig::default());
    let target = make_shared_test_node(relay, "target", MeshRoutingConfig::default());
    let nodes = [&spammer, &target];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    let channel = spammer
        .store
        .signaling()
        .get_channel("target")
        .await
        .expect("target channel");
    let spam = create_pubsub_frame("spam", 1, "spammer", vec![9; 256], MAX_HTL);
    channel
        .send(encode_pubsub_frame(&spam))
        .await
        .expect("send spam frame");
    pump_test_network(&nodes, 8).await;

    let after_spam = target.store.peer_traffic_snapshot("spammer").await;
    assert!(after_spam.bytes_received > 0);
    assert_eq!(
        after_spam.useful_bytes_received, 0,
        "unsubscribed publish spam must not count as useful ingress"
    );

    target.store.subscribe_pubsub("good").await;
    pump_test_network(&nodes, 24).await;
    spammer
        .store
        .publish_pubsub("good", 1, b"useful".to_vec())
        .await;
    pump_test_network(&nodes, 12).await;

    let events = target.store.drain_pubsub_events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload, b"useful".to_vec());
    assert!(
        target
            .store
            .peer_traffic_snapshot("spammer")
            .await
            .useful_bytes_received
            > 0
    );

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_pubsub_scheduler_uses_production_reciprocity_credit() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let publisher = make_shared_test_node(
        relay.clone(),
        "publisher",
        MeshRoutingConfig {
            pubsub_delivery_mode: PubsubDeliveryMode::InterestPush,
            pubsub_scheduler: PubsubSchedulerConfig {
                policy: crate::pubsub_strategy::PubsubSchedulingPolicy::Reciprocal,
                fanout: 1,
                anonymous_free_credit_bytes: 0,
                reciprocal_credit_multiplier: 1.0,
                aging_credit_bytes: 0,
            },
            ..Default::default()
        },
    );
    let leecher = make_shared_test_node(relay.clone(), "leecher", MeshRoutingConfig::default());
    let helper = make_shared_test_node(relay, "helper", MeshRoutingConfig::default());
    let nodes = [&publisher, &leecher, &helper];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 32).await;

    leecher.store.subscribe_pubsub("author:alice").await;
    helper.store.subscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 48).await;

    let mut interested = publisher.store.pubsub_interest_peers("author:alice").await;
    interested.sort();
    assert_eq!(
        interested,
        vec!["helper".to_string(), "leecher".to_string()]
    );

    publisher
        .store
        .record_useful_bytes_received_from_peer("helper", 64 * 1024)
        .await;
    let helper_before = publisher.store.peer_traffic_snapshot("helper").await;
    let leecher_before = publisher.store.peer_traffic_snapshot("leecher").await;

    let stats = publisher
        .store
        .publish_pubsub("author:alice", 1, vec![7; 512])
        .await;
    assert_eq!(stats.selected_peers, 1);
    assert_eq!(stats.sent_peers, 1);
    assert_eq!(stats.deferred_peers, 1);

    let helper_after = publisher.store.peer_traffic_snapshot("helper").await;
    let leecher_after = publisher.store.peer_traffic_snapshot("leecher").await;
    assert!(helper_after.bytes_sent > helper_before.bytes_sent);
    assert_eq!(leecher_after.bytes_sent, leecher_before.bytes_sent);

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_pubsub_unsubscribe_withdraws_interest_path() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let routing = MeshRoutingConfig {
        pubsub_delivery_mode: PubsubDeliveryMode::InterestPush,
        ..Default::default()
    };
    let publisher = make_shared_test_node(relay.clone(), "publisher", routing.clone());
    let subscriber = make_shared_test_node(relay, "subscriber", routing);
    let nodes = [&publisher, &subscriber];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    subscriber.store.subscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;
    assert_eq!(
        publisher.store.pubsub_interest_peers("author:alice").await,
        vec!["subscriber".to_string()]
    );

    subscriber.store.unsubscribe_pubsub("author:alice").await;
    pump_test_network(&nodes, 24).await;
    assert!(publisher
        .store
        .pubsub_interest_peers("author:alice")
        .await
        .is_empty());

    let stats = publisher
        .store
        .publish_pubsub("author:alice", 1, b"after-unsub".to_vec())
        .await;
    assert_eq!(stats.sent_peers, 0);
    pump_test_network(&nodes, 12).await;
    assert!(subscriber.store.drain_pubsub_events().await.is_empty());

    crate::mock::clear_channel_registry().await;
}

#[test]
fn test_actor_draw_is_deterministic_per_peer_hash_and_salt() {
    let hash = hashtree_core::sha256(b"deterministic");
    let a = TestStore::deterministic_actor_draw_for("peer-a", &hash, 7);
    let b = TestStore::deterministic_actor_draw_for("peer-a", &hash, 7);
    assert!((a - b).abs() < f64::EPSILON);
}

#[tokio::test]
async fn test_load_peer_metadata_returns_false_when_missing() {
    let local_store = Arc::new(MemoryStore::new());
    let store = make_test_store(local_store, "0");
    assert!(!store.load_peer_metadata().await.expect("load result"));
}

#[tokio::test]
async fn test_persist_and_load_peer_metadata_with_existing_store_adapter() {
    let local_store = Arc::new(MemoryStore::new());
    let writer = make_test_store(local_store.clone(), "0");
    {
        let mut selector = writer.peer_selector.write().await;
        selector.add_peer("npub1stable");
        selector.record_request("npub1stable", 64);
        selector.record_success("npub1stable", 35, 1024);
        selector.record_cashu_payment("npub1stable", 120);
        selector.record_cashu_receipt("npub1stable", 40);
        selector.record_cashu_payment_default("npub1stable");
    }

    let snapshot_hash = writer
        .persist_peer_metadata()
        .await
        .expect("persist peer metadata");
    assert!(local_store
        .get(&snapshot_hash)
        .await
        .expect("snapshot lookup")
        .is_some());

    let reader = make_test_store(local_store, "1");
    assert!(reader
        .load_peer_metadata()
        .await
        .expect("load peer metadata snapshot"));

    let mut selector = reader.peer_selector.write().await;
    selector.add_peer("npub1stable");
    let stats = selector
        .get_stats("npub1stable")
        .expect("restored peer stats");
    assert_eq!(stats.requests_sent, 1);
    assert_eq!(stats.successes, 1);
    assert_eq!(stats.cashu_paid_sat, 120);
    assert_eq!(stats.cashu_received_sat, 40);
    assert_eq!(stats.cashu_payment_receipts, 1);
    assert_eq!(stats.cashu_payment_defaults, 1);
}

#[tokio::test]
async fn test_should_refuse_requests_from_peer_after_payment_defaults() {
    let local_store = Arc::new(MemoryStore::new());
    let store = make_test_store_with_routing(
        local_store,
        "0",
        MeshRoutingConfig {
            cashu_payment_default_block_threshold: 1,
            ..Default::default()
        },
    );
    store.record_cashu_payment_default_from_peer("peer-a").await;

    let selector = store.peer_selector.read().await;
    assert!(store.should_refuse_requests_from_peer(&selector, "peer-a"));
    assert!(!store.should_refuse_requests_from_peer(&selector, "peer-b"));
}

#[tokio::test]
async fn test_take_valid_quote_consumes_once_and_rejects_expired_quotes() {
    let local_store = Arc::new(MemoryStore::new());
    let store = make_test_store(local_store, "0");
    let hash = hashtree_core::sha256(b"quote-test");
    let hash_key = hash_to_key(&hash);

    {
        let mut issued = store.issued_quotes.write().await;
        issued.insert(
            ("peer-a".to_string(), hash_key.clone(), 11),
            IssuedQuote {
                expires_at: Instant::now() + Duration::from_secs(1),
                payment_sat: 5,
                mint_url: Some("https://mint-a.example".to_string()),
            },
        );
        issued.insert(
            ("peer-a".to_string(), hash_key.clone(), 12),
            IssuedQuote {
                expires_at: Instant::now() - Duration::from_millis(1),
                payment_sat: 5,
                mint_url: Some("https://mint-a.example".to_string()),
            },
        );
    }

    assert!(store.take_valid_quote("peer-a", &hash_key, 11).await);
    assert!(!store.take_valid_quote("peer-a", &hash_key, 11).await);
    assert!(!store.take_valid_quote("peer-a", &hash_key, 12).await);
}

async fn run_quote_with_pumps(
    requester: Arc<TestStore>,
    hash: Hash,
    payment_sat: u64,
    quote_ttl: Duration,
    peer_ids: Vec<String>,
    nodes: &[&TestNode],
) -> Option<NegotiatedQuote> {
    let task = tokio::spawn(async move {
        requester
            .request_quote_from_peers(&hash, payment_sat, quote_ttl, &peer_ids)
            .await
    });
    let started = Instant::now();

    loop {
        if task.is_finished() {
            return task.await.expect("quote task join");
        }
        if started.elapsed() > Duration::from_secs(1) {
            task.abort();
            return None;
        }
        pump_test_network(nodes, 4).await;
    }
}

#[tokio::test]
async fn test_request_quote_from_peers_rejects_unaccepted_mint() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-reject",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
            cashu_default_mint: Some("https://mint-a.example".to_string()),
            dispatch: RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 5,
            },
            ..Default::default()
        },
    );
    let provider = make_shared_test_node(
        relay,
        "provider-reject",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-b.example".to_string()],
            ..Default::default()
        },
    );
    let nodes = [&requester, &provider];

    requester.transport.connect(&[]).await.expect("connect");
    provider.transport.connect(&[]).await.expect("connect");
    requester.store.start().await.expect("start");
    provider.store.start().await.expect("start");
    pump_test_network(&nodes, 24).await;

    let payload = b"quoted-data".to_vec();
    let hash = hashtree_core::sha256(&payload);
    provider.local_store.put(hash, payload).await.expect("put");

    let quote = run_quote_with_pumps(
        requester.store.clone(),
        hash,
        9,
        Duration::from_millis(80),
        vec!["provider-reject".to_string()],
        &nodes,
    )
    .await;
    assert!(
        quote.is_none(),
        "expected quote to be rejected on mint mismatch"
    );
}

#[tokio::test]
async fn test_request_quote_from_peers_accepts_small_peer_suggested_mint_under_cap() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-suggested",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
            cashu_default_mint: Some("https://mint-a.example".to_string()),
            cashu_peer_suggested_mint_base_cap_sat: 3,
            cashu_peer_suggested_mint_max_cap_sat: 3,
            dispatch: RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 5,
            },
            ..Default::default()
        },
    );
    let provider = make_shared_test_node(
        relay,
        "provider-suggested",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-b.example".to_string()],
            cashu_default_mint: Some("https://mint-b.example".to_string()),
            ..Default::default()
        },
    );
    let nodes = [&requester, &provider];

    requester.transport.connect(&[]).await.expect("connect");
    provider.transport.connect(&[]).await.expect("connect");
    requester.store.start().await.expect("start");
    provider.store.start().await.expect("start");
    pump_test_network(&nodes, 24).await;

    let payload = b"quoted-data".to_vec();
    let hash = hashtree_core::sha256(&payload);
    provider.local_store.put(hash, payload).await.expect("put");

    let quote = run_quote_with_pumps(
        requester.store.clone(),
        hash,
        3,
        Duration::from_millis(80),
        vec!["provider-suggested".to_string()],
        &nodes,
    )
    .await
    .expect("expected bounded peer-suggested mint quote");
    assert_eq!(quote.mint_url.as_deref(), Some("https://mint-b.example"));
}

#[tokio::test]
async fn test_request_quote_from_peers_scales_peer_suggested_mint_cap_with_reputation() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-reputation",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
            cashu_default_mint: Some("https://mint-a.example".to_string()),
            cashu_peer_suggested_mint_base_cap_sat: 1,
            cashu_peer_suggested_mint_success_step_sat: 1,
            cashu_peer_suggested_mint_receipt_step_sat: 2,
            cashu_peer_suggested_mint_max_cap_sat: 5,
            dispatch: RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 5,
            },
            ..Default::default()
        },
    );
    let provider = make_shared_test_node(
        relay,
        "provider-reputation",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-b.example".to_string()],
            cashu_default_mint: Some("https://mint-b.example".to_string()),
            ..Default::default()
        },
    );
    let nodes = [&requester, &provider];

    requester.transport.connect(&[]).await.expect("connect");
    provider.transport.connect(&[]).await.expect("connect");
    requester.store.start().await.expect("start");
    provider.store.start().await.expect("start");
    pump_test_network(&nodes, 24).await;

    let payload = b"quoted-data".to_vec();
    let hash = hashtree_core::sha256(&payload);
    provider.local_store.put(hash, payload).await.expect("put");

    let quote = run_quote_with_pumps(
        requester.store.clone(),
        hash,
        4,
        Duration::from_millis(80),
        vec!["provider-reputation".to_string()],
        &nodes,
    )
    .await;
    assert!(
        quote.is_none(),
        "new peer should not get a 4 sat untrusted-mint quote"
    );

    {
        let mut selector = requester.store.peer_selector.write().await;
        selector.add_peer("provider-reputation");
        selector.record_success("provider-reputation", 20, 1024);
        selector.record_cashu_receipt("provider-reputation", 2);
    }

    let quote = run_quote_with_pumps(
        requester.store.clone(),
        hash,
        4,
        Duration::from_millis(80),
        vec!["provider-reputation".to_string()],
        &nodes,
    )
    .await
    .expect("reputable peer should get larger bounded quote");
    assert_eq!(quote.mint_url.as_deref(), Some("https://mint-b.example"));

    requester
        .store
        .record_cashu_payment_default_from_peer("provider-reputation")
        .await;
    let quote = run_quote_with_pumps(
        requester.store.clone(),
        hash,
        4,
        Duration::from_millis(80),
        vec!["provider-reputation".to_string()],
        &nodes,
    )
    .await;
    assert!(
        quote.is_none(),
        "peer-suggested mint exposure should drop to zero after defaults exceed receipts"
    );
}

#[tokio::test]
async fn test_request_quote_from_peers_returns_matching_mint() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-match",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
            cashu_default_mint: Some("https://mint-a.example".to_string()),
            dispatch: RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 5,
            },
            ..Default::default()
        },
    );
    let provider = make_shared_test_node(
        relay,
        "provider-match",
        MeshRoutingConfig {
            cashu_accepted_mints: vec!["https://mint-a.example".to_string()],
            ..Default::default()
        },
    );
    let nodes = [&requester, &provider];

    requester.transport.connect(&[]).await.expect("connect");
    provider.transport.connect(&[]).await.expect("connect");
    requester.store.start().await.expect("start");
    provider.store.start().await.expect("start");
    pump_test_network(&nodes, 24).await;

    let payload = b"quoted-data".to_vec();
    let hash = hashtree_core::sha256(&payload);
    provider.local_store.put(hash, payload).await.expect("put");

    let quote = run_quote_with_pumps(
        requester.store.clone(),
        hash,
        9,
        Duration::from_millis(80),
        vec!["provider-match".to_string()],
        &nodes,
    )
    .await
    .expect("expected quote");
    assert_eq!(quote.mint_url.as_deref(), Some("https://mint-a.example"));
}

#[tokio::test]
async fn test_weighted_store_path_recovers_after_bad_peer_observation() {
    let weighted_successes = run_bad_peer_series(SelectionStrategy::Weighted).await;

    assert!(
            weighted_successes >= 5,
            "expected weighted path to recover after the first consistently bad peer observation (successes={weighted_successes})"
        );
}

#[tokio::test]
async fn test_ordered_connected_peers_treats_active_load_as_temporary() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester =
        make_shared_test_node(relay.clone(), "a-requester", MeshRoutingConfig::default());
    let peer_a = make_shared_test_node(relay.clone(), "b-peer", MeshRoutingConfig::default());
    let peer_b = make_shared_test_node(relay, "c-peer", MeshRoutingConfig::default());
    let nodes = [&requester, &peer_a, &peer_b];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    {
        let mut selector = requester.store.peer_selector.write().await;
        selector.add_peer("b-peer");
        selector.add_peer("c-peer");
        selector.record_success("b-peer", 15, 1024);
    }

    let ordered_idle = requester.store.ordered_connected_peers(None).await;
    assert_eq!(ordered_idle.first().map(String::as_str), Some("b-peer"));

    requester.store.reserve_peer_request("b-peer").await;
    let ordered_busy = requester.store.ordered_connected_peers(None).await;
    assert_eq!(ordered_busy.first().map(String::as_str), Some("c-peer"));

    requester.store.release_peer_request("b-peer").await;
    let ordered_released = requester.store.ordered_connected_peers(None).await;
    assert_eq!(ordered_released.first().map(String::as_str), Some("b-peer"));

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_request_from_mesh_skips_peers_with_hash_get_disabled() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-hash-get-filter",
        MeshRoutingConfig {
            dispatch: RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 5,
            },
            ..Default::default()
        },
    );
    let assist = make_shared_test_node_with_hash_get(
        relay.clone(),
        "a-assist-hash-get-filter",
        MeshRoutingConfig::default(),
        false,
    );
    let capable = make_shared_test_node_with_hash_get(
        relay,
        "b-capable-hash-get-filter",
        MeshRoutingConfig::default(),
        true,
    );
    let nodes = [&requester, &assist, &capable];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 12).await;

    {
        let mut selector = requester.store.peer_selector.write().await;
        selector.record_request("a-assist-hash-get-filter", 40);
        selector.record_success("a-assist-hash-get-filter", 10, 128);
    }

    let payload = b"hash-get-capable-route".to_vec();
    let hash = hashtree_core::sha256(&payload);
    capable
        .local_store
        .put(hash, payload.clone())
        .await
        .expect("put capable payload");

    let result = run_get_with_pumps(requester.store.clone(), hash, &nodes).await;
    assert_eq!(result, Some(payload));
    assert_eq!(
        requester
            .store
            .peer_selector
            .read()
            .await
            .get_stats("a-assist-hash-get-filter")
            .expect("assist stats")
            .requests_sent,
        1,
        "requester should skip sending fresh hash_get traffic to assist-only peers",
    );
    assert!(
        requester
            .store
            .signaling()
            .peer_supports_hash_get("a-assist-hash-get-filter")
            .await
            .eq(&false),
        "router should retain the remote hash_get capability from hello messages",
    );

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_forwarded_request_with_exhausted_htl_does_not_forward_to_peers() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let gateway = make_shared_test_node(relay.clone(), "a-gateway", MeshRoutingConfig::default());
    let provider = make_shared_test_node(relay, "b-provider", MeshRoutingConfig::default());
    let nodes = [&gateway, &provider];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    let payload = b"should-not-forward".to_vec();
    let hash = hashtree_core::sha256(&payload);
    provider
        .local_store
        .put(hash, payload)
        .await
        .expect("put provider");

    gateway
        .store
        .handle_request_message("requester", create_request(&hash, 0))
        .await;

    tokio::task::yield_now().await;
    let provider_stats = provider.store.drain_available_data_messages().await;
    assert_eq!(
        provider_stats.request_messages, 0,
        "gateway must not forward peer requests once HTL is exhausted",
    );

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_blob_request_forwarding_decrements_htl_exactly_once() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let gateway = make_shared_test_node(relay.clone(), "a-gateway", MeshRoutingConfig::default());
    let provider = make_shared_test_node(relay, "b-provider", MeshRoutingConfig::default());
    let nodes = [&gateway, &provider];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    let hash = hashtree_core::sha256(b"exact-htl-decrement");
    let provider_channel = provider
        .store
        .signaling()
        .get_channel("a-gateway")
        .await
        .expect("provider channel");

    for (request_htl, expected_wire_htl) in [(1, 0), (3, 2), (MAX_HTL, MAX_HTL - 1)] {
        assert!(
            gateway
                .store
                .send_request_to_peer("b-provider", &hash, request_htl, None)
                .await,
            "request should be sent",
        );
        let bytes = provider_channel.try_recv().expect("forwarded request");
        let DataMessage::Request(request) = parse_message(&bytes).expect("request message") else {
            panic!("forwarding must send a blob request");
        };
        assert_eq!(request.htl, expected_wire_htl);
    }

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_forwarded_request_can_be_served_from_another_peer() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester =
        make_shared_test_node(relay.clone(), "a-requester", MeshRoutingConfig::default());
    let gateway = make_shared_test_node(relay.clone(), "b-gateway", MeshRoutingConfig::default());
    let provider = make_shared_test_node(relay, "c-provider", MeshRoutingConfig::default());
    let nodes = [&requester, &gateway, &provider];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    let payload = b"forwarded-via-peer".to_vec();
    let hash = hashtree_core::sha256(&payload);
    provider
        .local_store
        .put(hash, payload.clone())
        .await
        .expect("put");

    let result = run_forwarded_request_with_pumps(&requester, "b-gateway", hash, &nodes).await;
    assert_eq!(result, Some(payload));

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_incomplete_forwarded_search_can_retry_immediately() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let gateway = make_shared_test_node(relay.clone(), "a-gateway", MeshRoutingConfig::default());
    let provider = make_shared_test_node(relay, "b-provider", MeshRoutingConfig::default());
    let nodes = [&gateway, &provider];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    let hash = hashtree_core::sha256(b"recent-forward-miss");
    gateway
        .store
        .handle_request_message("requester", create_request(&hash, MAX_HTL))
        .await;

    tokio::task::yield_now().await;
    let first_attempt = provider.store.drain_available_data_messages().await;
    assert_eq!(first_attempt.request_messages, 1);

    tokio::time::sleep(Duration::from_millis(160)).await;
    tokio::task::yield_now().await;

    assert!(!gateway
        .store
        .pending_requests
        .read()
        .await
        .contains_key(&PendingRequestKey::new(hash, MAX_HTL)));
    gateway
        .store
        .handle_request_message("requester", create_request(&hash, MAX_HTL))
        .await;

    tokio::task::yield_now().await;
    let second_attempt = provider.store.drain_available_data_messages().await;
    assert_eq!(
        second_attempt.request_messages, 1,
        "silence is not an authoritative miss and must not suppress retry",
    );

    crate::mock::clear_channel_registry().await;
}
