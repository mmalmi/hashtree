//! Production pubsub workload driver using the shared mesh store core.
//!
//! This module intentionally contains only workload orchestration and reporting.
//! Pubsub routing, dedupe, useful-byte credit, and fanout scheduling all run
//! through `hashtree_network::MeshStoreCore`.

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use hashtree_core::MemoryStore;
use hashtree_network::{
    clear_channel_registry, decrement_htl_with_policy, should_forward_htl, MeshRouter,
    MeshRoutingConfig, MeshStoreCore, MockConnectionFactory, MockLatencyMode, MockRelay,
    MockRelayTransport, PeerHTLConfig, PoolConfig, PoolSettings, PubsubDeliveryMode,
    PubsubSchedulerConfig, SignalingTransport, SimMeshStore, MESH_EVENT_POLICY,
};

#[derive(Debug, Clone)]
pub struct MeshPubsubWorkloadConfig {
    pub seed: u64,
    pub node_count: usize,
    pub author_count: usize,
    pub subscribers_per_author: usize,
    pub publish_rounds: usize,
    pub payload_bytes: usize,
    pub pool: PoolConfig,
    pub pubsub_scheduler: PubsubSchedulerConfig,
    pub pubsub_delivery_mode: PubsubDeliveryMode,
    pub reciprocal_provider_fraction: f64,
    pub reciprocal_credit_bytes: u64,
    pub subscription_churn_rate: f64,
    pub allow_rejoin: bool,
    pub spam_author_count: usize,
    pub spam_subscribers_per_author: usize,
    pub spam_publish_rounds_per_round: usize,
    pub pump_steps_after_setup: usize,
    pub pump_steps_per_publish_round: usize,
    pub latency_per_pump_step_ms: u64,
}

impl Default for MeshPubsubWorkloadConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            node_count: 32,
            author_count: 4,
            subscribers_per_author: 8,
            publish_rounds: 4,
            payload_bytes: 1024,
            pool: PoolConfig {
                max_connections: 16,
                satisfied_connections: 8,
            },
            pubsub_scheduler: PubsubSchedulerConfig::default(),
            pubsub_delivery_mode: PubsubDeliveryMode::HtlInvWant,
            reciprocal_provider_fraction: 0.75,
            reciprocal_credit_bytes: 256 * 1024,
            subscription_churn_rate: 0.0,
            allow_rejoin: false,
            spam_author_count: 0,
            spam_subscribers_per_author: 0,
            spam_publish_rounds_per_round: 0,
            pump_steps_after_setup: 96,
            pump_steps_per_publish_round: 64,
            latency_per_pump_step_ms: 10,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshPubsubWorkloadReport {
    pub seed: u64,
    pub node_count: usize,
    pub active_nodes: usize,
    pub average_peer_count: f64,
    pub min_peer_count: usize,
    pub max_peer_count: usize,
    pub isolated_nodes: usize,
    pub author_count: usize,
    pub publish_rounds: usize,
    pub subscriber_attempts: u64,
    pub active_subscriptions: u64,
    pub delivery_opportunities: u64,
    pub delivered_events: u64,
    pub delivery_rate: f64,
    pub loss_rate: f64,
    pub duplicate_deliveries: u64,
    pub forwarded_bytes_sent: u64,
    pub wire_bytes_received: u64,
    pub useful_bytes_received: u64,
    pub bytes_sent_per_delivered_event: f64,
    pub delivery_latency_p50_ms: u64,
    pub delivery_latency_p95_ms: u64,
    pub delivery_latency_max_ms: u64,
    pub churn_unsubscribes: u64,
    pub churn_rejoins: u64,
    pub spam_author_count: usize,
    pub spam_publish_events: u64,
    pub spam_delivery_opportunities: u64,
    pub spam_delivered_events: u64,
    pub spam_delivery_rate: f64,
}

#[derive(Debug, Clone)]
pub struct MeshPubsubSweepResult {
    pub config: MeshPubsubWorkloadConfig,
    pub report: MeshPubsubWorkloadReport,
}

struct MeshPubsubNode {
    id: String,
    store: Arc<SimMeshStore<MemoryStore>>,
    transport: Arc<MockRelayTransport>,
}

#[derive(Default)]
struct PumpStats {
    data_messages: usize,
    data_bytes: u64,
}

fn author_stream(index: usize) -> String {
    format!("author:{index:04}")
}

fn spam_stream(index: usize) -> String {
    format!("spam:{index:04}")
}

async fn make_node(
    relay: Arc<MockRelay>,
    node_id: String,
    config: &MeshPubsubWorkloadConfig,
) -> MeshPubsubNode {
    let transport = Arc::new(relay.create_transport(node_id.clone()));
    let conn_factory = Arc::new(MockConnectionFactory::new_with_latency_mode(
        node_id.clone(),
        0,
        MockLatencyMode::YieldOnly,
    ));
    let pools = PoolSettings {
        follows: PoolConfig {
            max_connections: 0,
            satisfied_connections: 0,
        },
        other: config.pool,
    };
    let signaling = Arc::new(MeshRouter::new(
        node_id.clone(),
        transport.clone(),
        conn_factory,
        pools,
        false,
    ));
    let store = Arc::new(MeshStoreCore::new_with_routing(
        Arc::new(MemoryStore::new()),
        signaling,
        Duration::from_secs(1),
        false,
        MeshRoutingConfig {
            pubsub_scheduler: config.pubsub_scheduler.clone(),
            pubsub_delivery_mode: config.pubsub_delivery_mode,
            ..Default::default()
        },
    ));

    transport.connect(&[]).await.expect("connect transport");
    store.start().await.expect("start store");

    MeshPubsubNode {
        id: node_id,
        store,
        transport,
    }
}

async fn make_workload_nodes(config: &MeshPubsubWorkloadConfig) -> Vec<MeshPubsubNode> {
    let relay_capacity = config
        .node_count
        .saturating_mul(
            config
                .pool
                .max_connections
                .max(config.pool.satisfied_connections),
        )
        .saturating_mul(4)
        .max(1000);
    let relay = MockRelay::new_with_capacity(relay_capacity);
    let node_ids = (0..config.node_count)
        .map(|index| format!("node-{index:04}"))
        .collect::<Vec<_>>();
    let mut nodes = Vec::with_capacity(config.node_count);
    for node_id in &node_ids {
        nodes.push(make_node(relay.clone(), node_id.clone(), config).await);
    }
    for _ in 0..3 {
        broadcast_hellos(&nodes).await;
        pump_mesh(&nodes, config.pump_steps_after_setup / 3).await;
    }
    pump_mesh(&nodes, config.pump_steps_after_setup).await;
    nodes
}

async fn pump_mesh(nodes: &[MeshPubsubNode], steps: usize) -> PumpStats {
    let mut stats = PumpStats::default();
    for _ in 0..steps {
        let mut node_indices = (0..nodes.len()).collect::<Vec<_>>();
        node_indices.sort_by(|left, right| nodes[*left].id.cmp(&nodes[*right].id));

        for index in &node_indices {
            let node = &nodes[*index];
            while let Some(msg) = node.transport.try_recv() {
                let _ = node.store.process_signaling(msg).await;
            }
        }

        for index in &node_indices {
            let node = &nodes[*index];
            let data = node.store.drain_available_data_messages().await;
            stats.data_messages += data.processed;
            stats.data_bytes = stats.data_bytes.saturating_add(data.processed_bytes);
        }

        tokio::task::yield_now().await;
    }
    stats
}

async fn broadcast_hellos(nodes: &[MeshPubsubNode]) {
    for node in nodes {
        let _ = node.store.send_hello().await;
    }
}

fn choose_publishers(rng: &mut StdRng, node_ids: &[String], stream_count: usize) -> Vec<String> {
    let mut pool = node_ids.to_vec();
    pool.shuffle(rng);
    (0..stream_count)
        .map(|index| pool[index % pool.len()].clone())
        .collect()
}

fn insert_subscription(
    subscriptions: &mut BTreeMap<String, BTreeSet<String>>,
    subscriber_id: &str,
    stream_id: &str,
) -> bool {
    subscriptions
        .entry(stream_id.to_string())
        .or_default()
        .insert(subscriber_id.to_string())
}

fn remove_subscription(
    subscriptions: &mut BTreeMap<String, BTreeSet<String>>,
    subscriber_id: &str,
    stream_id: &str,
) -> bool {
    subscriptions
        .get_mut(stream_id)
        .map(|subscribers| subscribers.remove(subscriber_id))
        .unwrap_or(false)
}

async fn subscribe_stream(
    nodes_by_id: &HashMap<String, Arc<SimMeshStore<MemoryStore>>>,
    subscriptions: &mut BTreeMap<String, BTreeSet<String>>,
    subscriber_id: &str,
    stream_id: &str,
) -> bool {
    let Some(store) = nodes_by_id.get(subscriber_id) else {
        return false;
    };
    let inserted = insert_subscription(subscriptions, subscriber_id, stream_id);
    if inserted {
        store.subscribe_pubsub(stream_id.to_string()).await;
    }
    inserted
}

async fn unsubscribe_stream(
    nodes_by_id: &HashMap<String, Arc<SimMeshStore<MemoryStore>>>,
    subscriptions: &mut BTreeMap<String, BTreeSet<String>>,
    subscriber_id: &str,
    stream_id: &str,
) -> bool {
    let Some(store) = nodes_by_id.get(subscriber_id) else {
        return false;
    };
    let removed = remove_subscription(subscriptions, subscriber_id, stream_id);
    if removed {
        store.unsubscribe_pubsub(stream_id.to_string()).await;
    }
    removed
}

async fn seed_reciprocal_credit(
    nodes: &[MeshPubsubNode],
    provider_ids: &BTreeSet<String>,
    credit_bytes: u64,
) {
    if credit_bytes == 0 {
        return;
    }

    for node in nodes {
        for provider_id in provider_ids {
            if provider_id == &node.id {
                continue;
            }
            if node
                .store
                .signaling()
                .get_channel(provider_id)
                .await
                .is_some()
            {
                node.store
                    .record_useful_bytes_received_from_peer(provider_id, credit_bytes)
                    .await;
            }
        }
    }
}

fn apply_subscription_churn_plan(
    rng: &mut StdRng,
    subscriptions: &BTreeMap<String, BTreeSet<String>>,
    config: &MeshPubsubWorkloadConfig,
) -> Vec<(String, String, bool)> {
    if config.subscription_churn_rate <= 0.0 {
        return Vec::new();
    }

    let churn_rate = config.subscription_churn_rate.clamp(0.0, 1.0);
    let mut actions = Vec::new();
    for (stream_id, subscribers) in subscriptions {
        for subscriber_id in subscribers {
            if rng.gen::<f64>() < churn_rate {
                actions.push((stream_id.clone(), subscriber_id.clone(), false));
                if config.allow_rejoin {
                    actions.push((stream_id.clone(), subscriber_id.clone(), true));
                }
            }
        }
    }
    actions.shuffle(rng);
    actions
}

fn percentile(values: &[u64], percentile: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let idx = ((values.len() - 1) as f64 * percentile).round() as usize;
    values[idx.min(values.len() - 1)]
}

fn finalize_rates(report: &mut MeshPubsubWorkloadReport, latencies_ms: &mut [u64]) {
    report.delivery_rate = if report.delivery_opportunities == 0 {
        0.0
    } else {
        report.delivered_events as f64 / report.delivery_opportunities as f64
    };
    report.loss_rate = 1.0 - report.delivery_rate;
    report.spam_delivery_rate = if report.spam_delivery_opportunities == 0 {
        0.0
    } else {
        report.spam_delivered_events as f64 / report.spam_delivery_opportunities as f64
    };
    report.bytes_sent_per_delivered_event = if report.delivered_events == 0 {
        0.0
    } else {
        report.forwarded_bytes_sent as f64 / report.delivered_events as f64
    };

    latencies_ms.sort_unstable();
    report.delivery_latency_p50_ms = percentile(latencies_ms, 0.50);
    report.delivery_latency_p95_ms = percentile(latencies_ms, 0.95);
    report.delivery_latency_max_ms = latencies_ms.last().copied().unwrap_or_default();
}

async fn record_topology_report(nodes: &[MeshPubsubNode], report: &mut MeshPubsubWorkloadReport) {
    if nodes.is_empty() {
        return;
    }

    let mut total_peers = 0usize;
    let mut min_peers = usize::MAX;
    let mut max_peers = 0usize;
    let mut isolated = 0usize;
    for node in nodes {
        let peer_count = node.store.peer_count().await;
        total_peers = total_peers.saturating_add(peer_count);
        min_peers = min_peers.min(peer_count);
        max_peers = max_peers.max(peer_count);
        if peer_count == 0 {
            isolated = isolated.saturating_add(1);
        }
    }

    report.average_peer_count = total_peers as f64 / nodes.len() as f64;
    report.min_peer_count = min_peers;
    report.max_peer_count = max_peers;
    report.isolated_nodes = isolated;
}

async fn peer_graph(nodes: &[MeshPubsubNode]) -> BTreeMap<String, Vec<String>> {
    let mut graph = BTreeMap::new();
    for node in nodes {
        let mut peers = node.store.signaling().peer_ids().await;
        peers.sort();
        graph.insert(node.id.clone(), peers);
    }
    graph
}

fn deterministic_htl_config(from: &str, to: &str) -> PeerHTLConfig {
    fn sample(from: &str, to: &str, salt: u8) -> f64 {
        let mut hasher = DefaultHasher::new();
        from.hash(&mut hasher);
        to.hash(&mut hasher);
        salt.hash(&mut hasher);
        hasher.finish() as f64 / u64::MAX as f64
    }

    PeerHTLConfig::from_samples(sample(from, to, 1), sample(from, to, 2))
}

struct HtlFloodResult {
    delivered_hops: BTreeMap<String, u64>,
    parents: BTreeMap<String, String>,
}

fn htl_flood_publish(
    graph: &BTreeMap<String, Vec<String>>,
    publisher_id: &str,
    payload_bytes: u64,
    htl: u8,
    report: &mut MeshPubsubWorkloadReport,
) -> HtlFloodResult {
    let htl = htl.clamp(1, MESH_EVENT_POLICY.max_htl);
    let mut delivered_hops = BTreeMap::from([(publisher_id.to_string(), 0u64)]);
    let mut parents = BTreeMap::new();
    let mut queue = VecDeque::new();
    if let Some(neighbors) = graph.get(publisher_id) {
        for neighbor in neighbors {
            report.forwarded_bytes_sent = report.forwarded_bytes_sent.saturating_add(payload_bytes);
            queue.push_back((neighbor.clone(), publisher_id.to_string(), htl, 1u64));
        }
    }

    while let Some((node_id, sender_id, frame_htl, hop_count)) = queue.pop_front() {
        if delivered_hops.contains_key(&node_id) {
            report.duplicate_deliveries = report.duplicate_deliveries.saturating_add(1);
            continue;
        }
        delivered_hops.insert(node_id.clone(), hop_count);
        parents.insert(node_id.clone(), sender_id.clone());

        let Some(neighbors) = graph.get(&node_id) else {
            continue;
        };
        for neighbor in neighbors {
            if neighbor == &sender_id {
                continue;
            }
            let htl_config = deterministic_htl_config(&node_id, neighbor);
            let next_htl = decrement_htl_with_policy(frame_htl, &MESH_EVENT_POLICY, &htl_config);
            if !should_forward_htl(next_htl) {
                continue;
            }

            report.forwarded_bytes_sent = report.forwarded_bytes_sent.saturating_add(payload_bytes);
            queue.push_back((
                neighbor.clone(),
                node_id.clone(),
                next_htl,
                hop_count.saturating_add(1),
            ));
        }
    }

    HtlFloodResult {
        delivered_hops,
        parents,
    }
}

fn htl_inv_want_publish(
    graph: &BTreeMap<String, Vec<String>>,
    publisher_id: &str,
    subscribers: &BTreeSet<String>,
    payload_bytes: u64,
    htl: u8,
    report: &mut MeshPubsubWorkloadReport,
) -> BTreeMap<String, u64> {
    const INV_BYTES: u64 = 96;
    const WANT_BYTES: u64 = 64;

    let inv = htl_flood_publish(graph, publisher_id, INV_BYTES, htl, report);
    let mut want_edges = BTreeSet::<(String, String)>::new();
    let mut data_edges = BTreeSet::<(String, String)>::new();
    let mut delivered_latency_steps = BTreeMap::new();

    for subscriber in subscribers {
        let Some(inv_hops) = inv.delivered_hops.get(subscriber).copied() else {
            continue;
        };

        let mut current = subscriber.as_str();
        let mut path_hops = 0u64;
        while current != publisher_id {
            let Some(parent) = inv.parents.get(current) else {
                path_hops = 0;
                break;
            };
            want_edges.insert((current.to_string(), parent.clone()));
            data_edges.insert((parent.clone(), current.to_string()));
            path_hops = path_hops.saturating_add(1);
            current = parent;
        }

        if current == publisher_id {
            delivered_latency_steps.insert(
                subscriber.clone(),
                inv_hops.saturating_add(path_hops.saturating_mul(2)),
            );
        }
    }

    report.forwarded_bytes_sent = report
        .forwarded_bytes_sent
        .saturating_add(want_edges.len() as u64 * WANT_BYTES)
        .saturating_add(data_edges.len() as u64 * payload_bytes);

    delivered_latency_steps
}

fn record_htl_delivery_round(
    subscriptions: &BTreeMap<String, BTreeSet<String>>,
    stream_id: &str,
    delivered_hops: &BTreeMap<String, u64>,
    config: &MeshPubsubWorkloadConfig,
    report: &mut MeshPubsubWorkloadReport,
    latencies_ms: &mut Vec<u64>,
) {
    let Some(subscribers) = subscriptions.get(stream_id) else {
        return;
    };
    for subscriber in subscribers {
        if let Some(hops) = delivered_hops.get(subscriber) {
            report.delivered_events = report.delivered_events.saturating_add(1);
            latencies_ms.push(hops.saturating_mul(config.latency_per_pump_step_ms));
        }
    }
}

fn record_htl_spam_delivery_round(
    subscriptions: &BTreeMap<String, BTreeSet<String>>,
    stream_id: &str,
    delivered_hops: &BTreeMap<String, u64>,
    report: &mut MeshPubsubWorkloadReport,
) {
    let Some(subscribers) = subscriptions.get(stream_id) else {
        return;
    };
    for subscriber in subscribers {
        if delivered_hops.contains_key(subscriber) {
            report.spam_delivered_events = report.spam_delivered_events.saturating_add(1);
        }
    }
}

pub async fn run_mesh_pubsub_workload(
    config: MeshPubsubWorkloadConfig,
) -> MeshPubsubWorkloadReport {
    if config.node_count == 0 || config.publish_rounds == 0 {
        return MeshPubsubWorkloadReport {
            seed: config.seed,
            node_count: config.node_count,
            author_count: config.author_count,
            publish_rounds: config.publish_rounds,
            spam_author_count: config.spam_author_count,
            ..Default::default()
        };
    }

    let _mock_registry = crate::mock_registry::lock_mock_channel_registry().await;
    clear_channel_registry().await;

    let mut rng = StdRng::seed_from_u64(config.seed);
    let node_ids = (0..config.node_count)
        .map(|index| format!("node-{index:04}"))
        .collect::<Vec<_>>();
    let nodes = make_workload_nodes(&config).await;

    let nodes_by_id = nodes
        .iter()
        .map(|node| (node.id.clone(), node.store.clone()))
        .collect::<HashMap<_, _>>();
    let provider_ids = node_ids
        .iter()
        .filter(|_| rng.gen::<f64>() < config.reciprocal_provider_fraction.clamp(0.0, 1.0))
        .cloned()
        .collect::<BTreeSet<_>>();
    seed_reciprocal_credit(&nodes, &provider_ids, config.reciprocal_credit_bytes).await;

    let stream_count = config.author_count.saturating_add(config.spam_author_count);
    let publisher_ids = choose_publishers(&mut rng, &node_ids, stream_count.max(1));
    let useful_publishers = publisher_ids
        .iter()
        .take(config.author_count)
        .cloned()
        .collect::<Vec<_>>();
    let spam_publishers = publisher_ids
        .iter()
        .skip(config.author_count)
        .cloned()
        .collect::<Vec<_>>();

    let mut subscriptions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut report = MeshPubsubWorkloadReport {
        seed: config.seed,
        node_count: config.node_count,
        active_nodes: config.node_count,
        author_count: config.author_count,
        publish_rounds: config.publish_rounds,
        spam_author_count: config.spam_author_count,
        ..Default::default()
    };
    record_topology_report(&nodes, &mut report).await;

    for (author_index, publisher_id) in useful_publishers.iter().enumerate() {
        let stream_id = author_stream(author_index);
        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher_id)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        for subscriber_id in candidates.into_iter().take(
            config
                .subscribers_per_author
                .min(config.node_count.saturating_sub(1)),
        ) {
            report.subscriber_attempts = report.subscriber_attempts.saturating_add(1);
            if subscribe_stream(&nodes_by_id, &mut subscriptions, &subscriber_id, &stream_id).await
            {
                pump_mesh(&nodes, 1).await;
            }
        }
    }

    for (spam_index, publisher_id) in spam_publishers.iter().enumerate() {
        let stream_id = spam_stream(spam_index);
        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher_id)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        for subscriber_id in candidates.into_iter().take(
            config
                .spam_subscribers_per_author
                .min(config.node_count.saturating_sub(1)),
        ) {
            if subscribe_stream(&nodes_by_id, &mut subscriptions, &subscriber_id, &stream_id).await
            {
                pump_mesh(&nodes, 1).await;
            }
        }
    }

    pump_mesh(&nodes, config.pump_steps_after_setup).await;

    let mut seen = HashSet::<(String, String, u64)>::new();
    let mut publish_started_step = HashMap::<(String, u64), usize>::new();
    let mut latencies_ms = Vec::new();
    let mut total_pump_stats = PumpStats::default();

    for round in 0..config.publish_rounds {
        let churn_actions = apply_subscription_churn_plan(&mut rng, &subscriptions, &config);
        for (stream_id, subscriber_id, active) in churn_actions {
            if active {
                if subscribe_stream(&nodes_by_id, &mut subscriptions, &subscriber_id, &stream_id)
                    .await
                {
                    report.churn_rejoins = report.churn_rejoins.saturating_add(1);
                    pump_mesh(&nodes, 1).await;
                }
            } else if unsubscribe_stream(
                &nodes_by_id,
                &mut subscriptions,
                &subscriber_id,
                &stream_id,
            )
            .await
            {
                report.churn_unsubscribes = report.churn_unsubscribes.saturating_add(1);
                pump_mesh(&nodes, 1).await;
            }
        }
        if config.subscription_churn_rate > 0.0 {
            let pump = pump_mesh(&nodes, config.pump_steps_after_setup / 2).await;
            total_pump_stats.data_messages += pump.data_messages;
            total_pump_stats.data_bytes =
                total_pump_stats.data_bytes.saturating_add(pump.data_bytes);
        }

        for spam_round in 0..config.spam_publish_rounds_per_round {
            let seq = (round * config.spam_publish_rounds_per_round + spam_round + 1) as u64;
            for (spam_index, publisher_id) in spam_publishers.iter().enumerate() {
                let stream_id = spam_stream(spam_index);
                if let Some(subscribers) = subscriptions.get(&stream_id) {
                    report.spam_delivery_opportunities = report
                        .spam_delivery_opportunities
                        .saturating_add(subscribers.len() as u64);
                }
                report.spam_publish_events = report.spam_publish_events.saturating_add(1);
                if let Some(store) = nodes_by_id.get(publisher_id) {
                    store
                        .publish_pubsub(stream_id.clone(), seq, vec![0x5a; config.payload_bytes])
                        .await;
                    publish_started_step.insert((stream_id, seq), 0);
                }
            }
        }

        let seq = (round + 1) as u64;
        for (author_index, publisher_id) in useful_publishers.iter().enumerate() {
            let stream_id = author_stream(author_index);
            if let Some(subscribers) = subscriptions.get(&stream_id) {
                report.delivery_opportunities = report
                    .delivery_opportunities
                    .saturating_add(subscribers.len() as u64);
            }
            if let Some(store) = nodes_by_id.get(publisher_id) {
                store
                    .publish_pubsub(stream_id.clone(), seq, vec![0x7a; config.payload_bytes])
                    .await;
                publish_started_step.insert((stream_id, seq), 0);
            }
        }

        for step in 0..config.pump_steps_per_publish_round {
            let pump = pump_mesh(&nodes, 1).await;
            total_pump_stats.data_messages += pump.data_messages;
            total_pump_stats.data_bytes =
                total_pump_stats.data_bytes.saturating_add(pump.data_bytes);

            for node in &nodes {
                for event in node.store.drain_pubsub_events().await {
                    let key = (node.id.clone(), event.stream_id.clone(), event.seq);
                    let expected = subscriptions
                        .get(&event.stream_id)
                        .is_some_and(|subscribers| subscribers.contains(&node.id));
                    if !expected || !seen.insert(key) {
                        report.duplicate_deliveries = report.duplicate_deliveries.saturating_add(1);
                        continue;
                    }

                    if event.stream_id.starts_with("spam:") {
                        report.spam_delivered_events =
                            report.spam_delivered_events.saturating_add(1);
                    } else {
                        report.delivered_events = report.delivered_events.saturating_add(1);
                        let start = publish_started_step
                            .get(&(event.stream_id.clone(), event.seq))
                            .copied()
                            .unwrap_or_default();
                        let elapsed_steps = step.saturating_sub(start);
                        latencies_ms.push(elapsed_steps as u64 * config.latency_per_pump_step_ms);
                    }
                }
            }
        }
    }

    report.active_subscriptions = subscriptions
        .values()
        .map(|subscribers| subscribers.len() as u64)
        .sum();

    for node in &nodes {
        for snapshot in node.store.peer_traffic_snapshots().await.values() {
            report.forwarded_bytes_sent = report
                .forwarded_bytes_sent
                .saturating_add(snapshot.bytes_sent);
            report.wire_bytes_received = report
                .wire_bytes_received
                .saturating_add(snapshot.bytes_received);
            report.useful_bytes_received = report
                .useful_bytes_received
                .saturating_add(snapshot.useful_bytes_received);
        }
    }
    report.wire_bytes_received = report.wire_bytes_received.max(total_pump_stats.data_bytes);

    finalize_rates(&mut report, &mut latencies_ms);
    clear_channel_registry().await;
    report
}

#[derive(Debug, Clone, Copy)]
enum HtlBaselineMode {
    FloodPayload,
    InvWant,
}

pub async fn run_mesh_pubsub_htl_flood_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(config, htl, HtlBaselineMode::FloodPayload).await
}

pub async fn run_mesh_pubsub_htl_inv_want_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(config, htl, HtlBaselineMode::InvWant).await
}

async fn run_mesh_pubsub_htl_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
    mode: HtlBaselineMode,
) -> MeshPubsubWorkloadReport {
    if config.node_count == 0 || config.publish_rounds == 0 {
        return MeshPubsubWorkloadReport {
            seed: config.seed,
            node_count: config.node_count,
            author_count: config.author_count,
            publish_rounds: config.publish_rounds,
            spam_author_count: config.spam_author_count,
            ..Default::default()
        };
    }

    let _mock_registry = crate::mock_registry::lock_mock_channel_registry().await;
    clear_channel_registry().await;

    let mut rng = StdRng::seed_from_u64(config.seed);
    let node_ids = (0..config.node_count)
        .map(|index| format!("node-{index:04}"))
        .collect::<Vec<_>>();
    let nodes = make_workload_nodes(&config).await;
    let graph = peer_graph(&nodes).await;

    let _provider_draws = node_ids
        .iter()
        .filter(|_| rng.gen::<f64>() < config.reciprocal_provider_fraction.clamp(0.0, 1.0))
        .count();

    let stream_count = config.author_count.saturating_add(config.spam_author_count);
    let publisher_ids = choose_publishers(&mut rng, &node_ids, stream_count.max(1));
    let useful_publishers = publisher_ids
        .iter()
        .take(config.author_count)
        .cloned()
        .collect::<Vec<_>>();
    let spam_publishers = publisher_ids
        .iter()
        .skip(config.author_count)
        .cloned()
        .collect::<Vec<_>>();

    let mut subscriptions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut report = MeshPubsubWorkloadReport {
        seed: config.seed,
        node_count: config.node_count,
        active_nodes: config.node_count,
        author_count: config.author_count,
        publish_rounds: config.publish_rounds,
        spam_author_count: config.spam_author_count,
        ..Default::default()
    };
    record_topology_report(&nodes, &mut report).await;

    for (author_index, publisher_id) in useful_publishers.iter().enumerate() {
        let stream_id = author_stream(author_index);
        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher_id)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        for subscriber_id in candidates.into_iter().take(
            config
                .subscribers_per_author
                .min(config.node_count.saturating_sub(1)),
        ) {
            report.subscriber_attempts = report.subscriber_attempts.saturating_add(1);
            insert_subscription(&mut subscriptions, &subscriber_id, &stream_id);
        }
    }

    for (spam_index, publisher_id) in spam_publishers.iter().enumerate() {
        let stream_id = spam_stream(spam_index);
        let mut candidates = node_ids
            .iter()
            .filter(|node_id| *node_id != publisher_id)
            .cloned()
            .collect::<Vec<_>>();
        candidates.shuffle(&mut rng);
        for subscriber_id in candidates.into_iter().take(
            config
                .spam_subscribers_per_author
                .min(config.node_count.saturating_sub(1)),
        ) {
            insert_subscription(&mut subscriptions, &subscriber_id, &stream_id);
        }
    }

    let mut latencies_ms = Vec::new();
    for round in 0..config.publish_rounds {
        let churn_actions = apply_subscription_churn_plan(&mut rng, &subscriptions, &config);
        for (stream_id, subscriber_id, active) in churn_actions {
            if active {
                if insert_subscription(&mut subscriptions, &subscriber_id, &stream_id) {
                    report.churn_rejoins = report.churn_rejoins.saturating_add(1);
                }
            } else if remove_subscription(&mut subscriptions, &subscriber_id, &stream_id) {
                report.churn_unsubscribes = report.churn_unsubscribes.saturating_add(1);
            }
        }

        for spam_round in 0..config.spam_publish_rounds_per_round {
            let _seq = (round * config.spam_publish_rounds_per_round + spam_round + 1) as u64;
            for (spam_index, publisher_id) in spam_publishers.iter().enumerate() {
                let stream_id = spam_stream(spam_index);
                let subscribers = subscriptions.get(&stream_id).cloned().unwrap_or_default();
                if !subscribers.is_empty() {
                    report.spam_delivery_opportunities = report
                        .spam_delivery_opportunities
                        .saturating_add(subscribers.len() as u64);
                }
                report.spam_publish_events = report.spam_publish_events.saturating_add(1);
                let delivered_hops = match mode {
                    HtlBaselineMode::FloodPayload => {
                        htl_flood_publish(
                            &graph,
                            publisher_id,
                            config.payload_bytes as u64,
                            htl,
                            &mut report,
                        )
                        .delivered_hops
                    }
                    HtlBaselineMode::InvWant => htl_inv_want_publish(
                        &graph,
                        publisher_id,
                        &subscribers,
                        config.payload_bytes as u64,
                        htl,
                        &mut report,
                    ),
                };
                record_htl_spam_delivery_round(
                    &subscriptions,
                    &stream_id,
                    &delivered_hops,
                    &mut report,
                );
            }
        }

        for (author_index, publisher_id) in useful_publishers.iter().enumerate() {
            let stream_id = author_stream(author_index);
            let subscribers = subscriptions.get(&stream_id).cloned().unwrap_or_default();
            if !subscribers.is_empty() {
                report.delivery_opportunities = report
                    .delivery_opportunities
                    .saturating_add(subscribers.len() as u64);
            }
            let delivered_hops = match mode {
                HtlBaselineMode::FloodPayload => {
                    htl_flood_publish(
                        &graph,
                        publisher_id,
                        config.payload_bytes as u64,
                        htl,
                        &mut report,
                    )
                    .delivered_hops
                }
                HtlBaselineMode::InvWant => htl_inv_want_publish(
                    &graph,
                    publisher_id,
                    &subscribers,
                    config.payload_bytes as u64,
                    htl,
                    &mut report,
                ),
            };
            record_htl_delivery_round(
                &subscriptions,
                &stream_id,
                &delivered_hops,
                &config,
                &mut report,
                &mut latencies_ms,
            );
        }
    }

    report.active_subscriptions = subscriptions
        .values()
        .map(|subscribers| subscribers.len() as u64)
        .sum();
    report.wire_bytes_received = report.forwarded_bytes_sent;

    finalize_rates(&mut report, &mut latencies_ms);
    clear_channel_registry().await;
    report
}

pub async fn run_mesh_pubsub_sweep(
    configs: &[MeshPubsubWorkloadConfig],
) -> Vec<MeshPubsubSweepResult> {
    let mut results = Vec::with_capacity(configs.len());
    for config in configs {
        let report = run_mesh_pubsub_workload(config.clone()).await;
        results.push(MeshPubsubSweepResult {
            config: config.clone(),
            report,
        });
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_network::PubsubSchedulingPolicy;

    #[tokio::test]
    async fn production_mesh_pubsub_workload_uses_real_core_delivery() {
        let mut config = MeshPubsubWorkloadConfig {
            seed: 7,
            node_count: 10,
            author_count: 2,
            subscribers_per_author: 3,
            publish_rounds: 2,
            payload_bytes: 128,
            spam_author_count: 1,
            spam_publish_rounds_per_round: 2,
            pump_steps_after_setup: 128,
            pump_steps_per_publish_round: 96,
            ..Default::default()
        };
        config.pubsub_scheduler.fanout = 16;
        let report = run_mesh_pubsub_workload(config).await;

        assert_eq!(report.delivery_opportunities, 12);
        assert!(
            report.delivered_events > 0,
            "workload should deliver via MeshStoreCore pubsub"
        );
        assert!(report.delivered_events <= report.delivery_opportunities);
        assert_eq!(report.spam_publish_events, 4);
        assert!(report.forwarded_bytes_sent > 0);
        assert!(report.useful_bytes_received > 0);
    }

    #[tokio::test]
    async fn production_mesh_pubsub_sweep_compares_scheduler_configs() {
        let mut fair = MeshPubsubWorkloadConfig {
            seed: 11,
            node_count: 8,
            author_count: 1,
            subscribers_per_author: 4,
            publish_rounds: 1,
            payload_bytes: 64,
            pump_steps_after_setup: 40,
            pump_steps_per_publish_round: 24,
            ..Default::default()
        };
        fair.pubsub_scheduler.policy = PubsubSchedulingPolicy::Fair;
        fair.pubsub_scheduler.fanout = 2;

        let mut reciprocal = fair.clone();
        reciprocal.pubsub_scheduler.policy = PubsubSchedulingPolicy::Reciprocal;

        let results = run_mesh_pubsub_sweep(&[fair, reciprocal]).await;
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|result| result.report.delivery_opportunities > 0));
    }

    #[tokio::test]
    async fn htl_flood_baseline_uses_same_workload_shape() {
        let config = MeshPubsubWorkloadConfig {
            seed: 13,
            node_count: 12,
            author_count: 2,
            subscribers_per_author: 4,
            publish_rounds: 2,
            payload_bytes: 1024,
            spam_author_count: 1,
            spam_subscribers_per_author: 3,
            spam_publish_rounds_per_round: 1,
            pump_steps_after_setup: 48,
            ..Default::default()
        };

        let report =
            run_mesh_pubsub_htl_flood_baseline(config.clone(), MESH_EVENT_POLICY.max_htl).await;
        let inv_want =
            run_mesh_pubsub_htl_inv_want_baseline(config.clone(), MESH_EVENT_POLICY.max_htl).await;

        assert_eq!(
            report.delivery_opportunities,
            (config.author_count * config.subscribers_per_author * config.publish_rounds) as u64
        );
        assert!(report.delivered_events > 0);
        assert!(report.forwarded_bytes_sent > 0);
        assert!(report.average_peer_count > 0.0);
        assert_eq!(
            inv_want.delivery_opportunities,
            report.delivery_opportunities
        );
        assert!(inv_want.delivered_events > 0);
        assert!(
            inv_want.forwarded_bytes_sent < report.forwarded_bytes_sent,
            "inv/want should spend less bandwidth than full-payload HTL flood"
        );
    }
}
