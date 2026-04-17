use super::*;
use async_trait::async_trait;
use hashtree_core::MemoryStore;
use std::sync::atomic::{AtomicUsize, Ordering};
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

struct MockReadSource {
    id: String,
    store: Arc<MemoryStore>,
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl MockReadSource {
    fn new(
        id: impl Into<String>,
        store: Arc<MemoryStore>,
        calls: Arc<AtomicUsize>,
        delay: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            store,
            calls,
            delay,
        }
    }
}

#[async_trait]
impl MeshReadSource for MockReadSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.store.get(hash).await.ok().flatten()
    }
}

fn mock_network_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn make_test_store(local_store: Arc<MemoryStore>, node_id: &str) -> TestStore {
    make_test_store_with_routing(local_store, node_id, MeshRoutingConfig::default())
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
    let transport = Arc::new(relay.create_transport(node_id.to_string()));
    let conn_factory = Arc::new(crate::mock::MockConnectionFactory::new(
        node_id.to_string(),
        0,
    ));
    let signaling = Arc::new(crate::signaling::MeshRouter::new(
        node_id.to_string(),
        transport.clone(),
        conn_factory,
        crate::types::PoolSettings::default(),
        false,
    ));
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

async fn run_two_gets_with_pumps(
    requester_a: Arc<TestStore>,
    requester_b: Arc<TestStore>,
    hash: Hash,
    nodes: &[&TestNode],
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let task_a = tokio::spawn(async move { requester_a.get(&hash).await.ok().flatten() });
    let task_b = tokio::spawn(async move { requester_b.get(&hash).await.ok().flatten() });
    let started = Instant::now();

    loop {
        if task_a.is_finished() && task_b.is_finished() {
            return (
                task_a.await.expect("request task a join"),
                task_b.await.expect("request task b join"),
            );
        }

        if started.elapsed() > Duration::from_secs(1) {
            task_a.abort();
            task_b.abort();
            return (None, None);
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
    let hash_key = hash_to_key(&hash);
    let (tx, mut rx) = oneshot::channel();
    requester.store.pending_requests.write().await.insert(
        hash_key,
        PendingRequest {
            response_tx: tx,
            started_at: Instant::now(),
            queried_peers: vec![gateway_peer_id.to_string()],
        },
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
                .remove(&hash_to_key(&hash));
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
fn test_hedged_wave_plan_flood_all() {
    let plan = build_hedged_wave_plan(7, RequestDispatchConfig::default());
    assert_eq!(plan, vec![7]);
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
async fn test_tit_for_tat_store_path_recovers_after_bad_peer_observation() {
    let tit_for_tat_successes = run_bad_peer_series(SelectionStrategy::TitForTat).await;

    assert!(
            tit_for_tat_successes >= 5,
            "expected tit-for-tat path to recover after the first consistently bad peer observation (successes={tit_for_tat_successes})"
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
async fn test_read_sources_prefer_previously_successful_source() {
    let local_store = Arc::new(MemoryStore::new());
    let store = make_test_store_with_routing(
        local_store,
        "source-pref",
        MeshRoutingConfig {
            dispatch: RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 2,
                hedge_interval_ms: 5,
            },
            ..Default::default()
        },
    );

    let good_store = Arc::new(MemoryStore::new());
    let bad_store = Arc::new(MemoryStore::new());
    let good_calls = Arc::new(AtomicUsize::new(0));
    let bad_calls = Arc::new(AtomicUsize::new(0));

    let payload_a = b"source-a".to_vec();
    let payload_b = b"source-b".to_vec();
    let hash_a = hashtree_core::sha256(&payload_a);
    let hash_b = hashtree_core::sha256(&payload_b);
    good_store
        .put(hash_a, payload_a.clone())
        .await
        .expect("put a");
    good_store
        .put(hash_b, payload_b.clone())
        .await
        .expect("put b");

    store
        .set_read_sources(vec![
            Arc::new(MockReadSource::new(
                "bad",
                bad_store,
                bad_calls.clone(),
                Duration::ZERO,
            )) as Arc<dyn MeshReadSource>,
            Arc::new(MockReadSource::new(
                "good",
                good_store,
                good_calls.clone(),
                Duration::ZERO,
            )) as Arc<dyn MeshReadSource>,
        ])
        .await;

    let first = store.get(&hash_a).await.expect("first get");
    assert_eq!(first, Some(payload_a));
    assert_eq!(bad_calls.load(Ordering::Relaxed), 1);
    assert_eq!(good_calls.load(Ordering::Relaxed), 1);

    let second = store.get(&hash_b).await.expect("second get");
    assert_eq!(second, Some(payload_b));
    assert_eq!(
        good_calls.load(Ordering::Relaxed),
        2,
        "successful source should be preferred on the next lookup",
    );
    assert_eq!(
        bad_calls.load(Ordering::Relaxed),
        1,
        "miss-heavy source should not be probed again immediately",
    );
}

#[tokio::test]
async fn test_read_source_timeout_records_timeout_not_miss() {
    let local_store = Arc::new(MemoryStore::new());
    let store = make_test_store_with_routing(
        local_store,
        "source-timeout",
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

    let slow_store = Arc::new(MemoryStore::new());
    let payload = b"slow-source-data".to_vec();
    let hash = hashtree_core::sha256(&payload);
    slow_store
        .put(hash, payload)
        .await
        .expect("put slow payload");

    store
        .set_read_sources(vec![Arc::new(MockReadSource::new(
            "slow",
            slow_store,
            Arc::new(AtomicUsize::new(0)),
            Duration::from_millis(250),
        )) as Arc<dyn MeshReadSource>])
        .await;

    let result = store.get(&hash).await.expect("get");
    assert!(result.is_none());

    let stats = store.read_route_stats.read().await;
    let route = stats.get("sources").expect("sources route stats");
    assert_eq!(route.timeouts, 1);
    assert_eq!(route.misses, 0);
}

#[tokio::test]
async fn test_forwarded_request_can_be_served_from_read_source() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-upstream",
        MeshRoutingConfig::default(),
    );
    let gateway = make_shared_test_node(relay, "gateway-upstream", MeshRoutingConfig::default());
    let nodes = [&requester, &gateway];

    requester.transport.connect(&[]).await.expect("connect");
    gateway.transport.connect(&[]).await.expect("connect");
    requester.store.start().await.expect("start");
    gateway.store.start().await.expect("start");
    pump_test_network(&nodes, 24).await;

    let payload = b"gateway-upstream-data".to_vec();
    let hash = hashtree_core::sha256(&payload);
    let upstream_store = Arc::new(MemoryStore::new());
    upstream_store
        .put(hash, payload.clone())
        .await
        .expect("put upstream");
    let calls = Arc::new(AtomicUsize::new(0));
    gateway
        .store
        .set_read_sources(vec![Arc::new(MockReadSource::new(
            "upstream",
            upstream_store,
            calls.clone(),
            Duration::from_millis(10),
        )) as Arc<dyn MeshReadSource>])
        .await;

    let result = run_get_with_pumps(requester.store.clone(), hash, &nodes).await;
    assert_eq!(result, Some(payload.clone()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        gateway
            .local_store
            .get(&hash)
            .await
            .expect("gateway local lookup"),
        Some(payload),
    );

    crate::mock::clear_channel_registry().await;
}

#[tokio::test]
async fn test_blocked_requester_miss_can_still_be_served_from_read_source() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let requester = make_shared_test_node(
        relay.clone(),
        "requester-blocked-upstream",
        MeshRoutingConfig::default(),
    );
    let gateway = make_shared_test_node(
        relay,
        "gateway-blocked-upstream",
        MeshRoutingConfig {
            cashu_payment_default_block_threshold: 1,
            ..Default::default()
        },
    );
    let nodes = [&requester, &gateway];

    requester.transport.connect(&[]).await.expect("connect");
    gateway.transport.connect(&[]).await.expect("connect");
    requester.store.start().await.expect("start");
    gateway.store.start().await.expect("start");
    pump_test_network(&nodes, 24).await;

    gateway
        .store
        .record_cashu_payment_default_from_peer("requester-blocked-upstream")
        .await;

    let payload = b"blocked-requester-upstream-data".to_vec();
    let hash = hashtree_core::sha256(&payload);
    let upstream_store = Arc::new(MemoryStore::new());
    upstream_store
        .put(hash, payload.clone())
        .await
        .expect("put upstream");
    let calls = Arc::new(AtomicUsize::new(0));
    gateway
        .store
        .set_read_sources(vec![Arc::new(MockReadSource::new(
            "upstream",
            upstream_store,
            calls.clone(),
            Duration::from_millis(10),
        )) as Arc<dyn MeshReadSource>])
        .await;

    let result = run_get_with_pumps(requester.store.clone(), hash, &nodes).await;
    assert_eq!(result, Some(payload.clone()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        gateway
            .local_store
            .get(&hash)
            .await
            .expect("gateway local lookup"),
        Some(payload),
    );

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
async fn test_forwarded_upstream_fetch_is_shared_across_multiple_requesters() {
    let _guard = mock_network_lock().lock().await;
    crate::mock::clear_channel_registry().await;

    let relay = crate::mock::MockRelay::new();
    let gateway = make_shared_test_node(relay.clone(), "a-gateway", MeshRoutingConfig::default());
    let requester_a =
        make_shared_test_node(relay.clone(), "b-requester", MeshRoutingConfig::default());
    let requester_b = make_shared_test_node(relay, "c-requester", MeshRoutingConfig::default());
    let nodes = [&gateway, &requester_a, &requester_b];

    for node in &nodes {
        node.transport.connect(&[]).await.expect("connect");
        node.store.start().await.expect("start");
    }
    pump_test_network(&nodes, 24).await;

    let payload = b"shared-upstream-fetch".to_vec();
    let hash = hashtree_core::sha256(&payload);
    let upstream_store = Arc::new(MemoryStore::new());
    upstream_store
        .put(hash, payload.clone())
        .await
        .expect("put upstream");
    let calls = Arc::new(AtomicUsize::new(0));
    gateway
        .store
        .set_read_sources(vec![Arc::new(MockReadSource::new(
            "upstream",
            upstream_store,
            calls.clone(),
            Duration::from_millis(25),
        )) as Arc<dyn MeshReadSource>])
        .await;

    let (result_a, result_b) = run_two_gets_with_pumps(
        requester_a.store.clone(),
        requester_b.store.clone(),
        hash,
        &nodes,
    )
    .await;
    assert_eq!(result_a, Some(payload.clone()));
    assert_eq!(result_b, Some(payload));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "gateway should coalesce concurrent upstream reads for the same hash",
    );

    crate::mock::clear_channel_registry().await;
}
