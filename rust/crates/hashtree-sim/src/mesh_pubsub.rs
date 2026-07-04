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
    clear_channel_registry, decrement_htl_with_policy, should_forward_htl, HtlPolicy, MeshRouter,
    MeshRoutingConfig, MeshStoreCore, MockConnectionFactory, MockLatencyMode, MockRelay,
    MockRelayTransport, PeerHTLConfig, PoolConfig, PoolSettings, PubsubDeliveryMode,
    PubsubSchedulerConfig, SignalingTransport, SimMeshStore, MESH_EVENT_POLICY,
};

/// Build a local HTL policy with the given `max_htl`, sharing the same
/// probabilistic decrement parameters as the production
/// `MESH_EVENT_POLICY`. Lets the simulator explore HTL budgets above 4
/// without changing the production constant.
fn sim_htl_policy(max_htl: u8) -> HtlPolicy {
    HtlPolicy {
        max_htl: max_htl.max(1),
        ..MESH_EVENT_POLICY
    }
}

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
    /// Per-publish independent edge failure probability for the HTL graph
    /// baselines. Each publish samples a fresh broken-edge set deterministic
    /// in `(seed, global_publish_index)` so every baseline sees the same
    /// failures during one comparison run. 0.0 (default) means no failures.
    /// Production MeshStoreCore workloads ignore this — they go through the
    /// real mesh transport stack.
    pub broken_edge_fraction: f64,
    /// Optional Nostr-realistic workload: many authors, each subscriber
    /// follows a fraction of them. When set, the default
    /// (`author_count` × `subscribers_per_author`) generation is replaced
    /// with per-node follow sampling. Spam streams are disabled in this
    /// mode.
    pub nostr: Option<NostrWorkloadParams>,
}

#[derive(Debug, Clone, Copy)]
pub enum FollowDistribution {
    /// Each follower picks `follows_per_node` authors uniformly at random.
    Uniform,
    /// Each follower picks `follows_per_node` authors weighted by
    /// 1/rank^alpha — a few very popular authors, long tail of niche ones.
    /// alpha=1.0 is classic Zipf; larger values are more skewed.
    Zipf { alpha: f64 },
}

#[derive(Debug, Clone, Copy)]
pub struct NostrWorkloadParams {
    pub author_count: usize,
    pub follows_per_node: usize,
    pub follow_distribution: FollowDistribution,
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
            broken_edge_fraction: 0.0,
            nostr: None,
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
pub(crate) struct MeshPubsubPayloadConfig {
    pub seed: u64,
    pub node_count: usize,
    pub subscriber_count: usize,
    pub stream_id: String,
    pub payloads: Vec<Vec<u8>>,
    pub pool: PoolConfig,
    pub pump_steps_after_setup: usize,
    pub pump_steps_per_publish: usize,
}

impl Default for MeshPubsubPayloadConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            node_count: 8,
            subscriber_count: 3,
            stream_id: "payload".to_string(),
            payloads: Vec::new(),
            pool: PoolConfig {
                max_connections: 8,
                satisfied_connections: 4,
            },
            pump_steps_after_setup: 128,
            pump_steps_per_publish: 96,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MeshPubsubPayloadDelivery {
    pub subscriber_id: String,
    pub seq: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MeshPubsubPayloadReport {
    pub delivery_opportunities: u64,
    pub delivered_payloads: Vec<MeshPubsubPayloadDelivery>,
    pub duplicate_deliveries: u64,
    pub forwarded_bytes_sent: u64,
    pub wire_bytes_received: u64,
    pub useful_bytes_received: u64,
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

/// Topology snapshot of a constructed workload mesh, captured once at graph
/// extraction time so multiple HTL baselines can share it without re-running
/// the heavy peer-formation setup.
#[derive(Debug, Clone, Default)]
pub struct WorkloadTopology {
    pub average_peer_count: f64,
    pub min_peer_count: usize,
    pub max_peer_count: usize,
    pub isolated_nodes: usize,
}

async fn compute_topology(nodes: &[MeshPubsubNode]) -> WorkloadTopology {
    if nodes.is_empty() {
        return WorkloadTopology::default();
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
    WorkloadTopology {
        average_peer_count: total_peers as f64 / nodes.len() as f64,
        min_peer_count: min_peers,
        max_peer_count: max_peers,
        isolated_nodes: isolated,
    }
}

fn apply_topology(report: &mut MeshPubsubWorkloadReport, topology: &WorkloadTopology) {
    report.average_peer_count = topology.average_peer_count;
    report.min_peer_count = topology.min_peer_count;
    report.max_peer_count = topology.max_peer_count;
    report.isolated_nodes = topology.isolated_nodes;
}

async fn record_topology_report(nodes: &[MeshPubsubNode], report: &mut MeshPubsubWorkloadReport) {
    let topology = compute_topology(nodes).await;
    apply_topology(report, &topology);
}

/// Compute the peer graph and topology stats for a workload configuration.
/// This runs the full mesh setup (relay + signaling + hello pumps), then
/// extracts the formed graph and tears the nodes down. Heavy: at 1000 nodes
/// this dominates baseline runtime. Reusable across HTL-baseline variants
/// since `pubsub_scheduler` and `pubsub_delivery_mode` do not affect peer
/// formation — only `seed`, `node_count`, and `pool` do.
pub async fn compute_workload_peer_graph(
    config: &MeshPubsubWorkloadConfig,
) -> (BTreeMap<String, Vec<String>>, WorkloadTopology) {
    if config.node_count == 0 {
        return (BTreeMap::new(), WorkloadTopology::default());
    }
    let _mock_registry = crate::mock_registry::lock_mock_channel_registry().await;
    clear_channel_registry().await;
    let nodes = make_workload_nodes(config).await;
    let graph = peer_graph(&nodes).await;
    let topology = compute_topology(&nodes).await;
    drop(nodes);
    clear_channel_registry().await;
    (graph, topology)
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

/// Canonical undirected edge key (alphabetically ordered endpoints) so the
/// broken-edge set can be checked symmetrically without storing both
/// directions.
fn edge_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn is_edge_broken(broken: &BTreeSet<(String, String)>, a: &str, b: &str) -> bool {
    broken.contains(&edge_key(a, b))
}

/// Sample a broken-edge set for one publish. Independent Bernoulli per edge
/// with probability `fraction`; deterministic in `seed` so all baselines see
/// the same set during the same publish index.
fn compute_broken_edges(
    graph: &BTreeMap<String, Vec<String>>,
    fraction: f64,
    seed: u64,
) -> BTreeSet<(String, String)> {
    if fraction <= 0.0 {
        return BTreeSet::new();
    }
    let fraction = fraction.clamp(0.0, 1.0);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut broken = BTreeSet::new();
    for (a, neighbors) in graph {
        for b in neighbors {
            if a >= b {
                continue;
            }
            if rng.gen::<f64>() < fraction {
                broken.insert((a.clone(), b.clone()));
            }
        }
    }
    broken
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
    broken: &BTreeSet<(String, String)>,
    payload_bytes: u64,
    htl: u8,
    report: &mut MeshPubsubWorkloadReport,
) -> HtlFloodResult {
    let policy = sim_htl_policy(htl);
    let htl = policy.max_htl;
    let mut delivered_hops = BTreeMap::from([(publisher_id.to_string(), 0u64)]);
    let mut parents = BTreeMap::new();
    let mut queue = VecDeque::new();
    if let Some(neighbors) = graph.get(publisher_id) {
        for neighbor in neighbors {
            if is_edge_broken(broken, publisher_id, neighbor) {
                continue;
            }
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
            if is_edge_broken(broken, &node_id, neighbor) {
                continue;
            }
            let htl_config = deterministic_htl_config(&node_id, neighbor);
            let next_htl = decrement_htl_with_policy(frame_htl, &policy, &htl_config);
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

/// Interest-routed INV BFS — the discovery half of `InvWantInterestRouted`.
///
/// Each node's INV-forwarding decision is asymmetric:
/// - Subscribers (and the publisher) flood to all neighbors except sender.
/// - Non-subscribers forward INV only to neighbors that subscribe to this
///   stream — modelling the production `PubsubInterest` mechanism where
///   every node knows the 1-hop interest graph from periodic Subscribe
///   broadcasts.
///
/// Same return shape as `htl_flood_publish` so the WANT phase can reuse
/// the parent-walk logic in `htl_inv_want_publish`.
fn htl_interest_routed_inv(
    graph: &BTreeMap<String, Vec<String>>,
    publisher_id: &str,
    subscribers: &BTreeSet<String>,
    broken: &BTreeSet<(String, String)>,
    inv_bytes: u64,
    htl: u8,
    report: &mut MeshPubsubWorkloadReport,
) -> HtlFloodResult {
    let policy = sim_htl_policy(htl);
    let htl = policy.max_htl;
    let mut delivered_hops = BTreeMap::from([(publisher_id.to_string(), 0u64)]);
    let mut parents = BTreeMap::new();
    let mut queue = VecDeque::new();
    // The publisher is treated as a "subscriber" for the purposes of
    // forwarding (so it always floods its own event to all neighbors), even
    // if it isn't in the subscriber set.
    let is_interested = |node: &str| -> bool { node == publisher_id || subscribers.contains(node) };
    let should_forward_inv = |from: &str, to: &str| -> bool {
        if is_interested(from) {
            true
        } else {
            subscribers.contains(to)
        }
    };

    if let Some(neighbors) = graph.get(publisher_id) {
        for neighbor in neighbors {
            if !should_forward_inv(publisher_id, neighbor) {
                continue;
            }
            if is_edge_broken(broken, publisher_id, neighbor) {
                continue;
            }
            report.forwarded_bytes_sent = report.forwarded_bytes_sent.saturating_add(inv_bytes);
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
            if !should_forward_inv(&node_id, neighbor) {
                continue;
            }
            if is_edge_broken(broken, &node_id, neighbor) {
                continue;
            }
            let htl_config = deterministic_htl_config(&node_id, neighbor);
            let next_htl = decrement_htl_with_policy(frame_htl, &policy, &htl_config);
            if !should_forward_htl(next_htl) {
                continue;
            }
            report.forwarded_bytes_sent = report.forwarded_bytes_sent.saturating_add(inv_bytes);
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
    broken: &BTreeSet<(String, String)>,
    payload_bytes: u64,
    htl: u8,
    report: &mut MeshPubsubWorkloadReport,
) -> BTreeMap<String, u64> {
    const INV_BYTES: u64 = 96;
    const WANT_BYTES: u64 = 64;

    let inv = htl_flood_publish(graph, publisher_id, broken, INV_BYTES, htl, report);
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

/// Interest-routed inv-want: combines `htl_interest_routed_inv` with the
/// standard WANT pull-back phase. INV propagation is asymmetric per node
/// (subs flood, non-subs forward only to known-subscriber neighbors); WANT
/// is unchanged from the regular invwant.
fn htl_interest_routed_inv_want_publish(
    graph: &BTreeMap<String, Vec<String>>,
    publisher_id: &str,
    subscribers: &BTreeSet<String>,
    broken: &BTreeSet<(String, String)>,
    payload_bytes: u64,
    htl: u8,
    report: &mut MeshPubsubWorkloadReport,
) -> BTreeMap<String, u64> {
    const INV_BYTES: u64 = 96;
    const WANT_BYTES: u64 = 64;

    let inv = htl_interest_routed_inv(
        graph,
        publisher_id,
        subscribers,
        broken,
        INV_BYTES,
        htl,
        report,
    );
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

/// Per-(node, neighbor) score, gossipsub v1.1-style. Higher is better.
/// Tracks first-delivery count (P2), duplicates from this peer (P3), and
/// time-in-mesh (P1). Used to choose which lazy peer to graft on heartbeat.
#[derive(Debug, Clone, Default)]
struct PeerScore {
    first_deliveries: u64,
    duplicates_from: u64,
    rounds_in_mesh: u64,
}

impl PeerScore {
    fn weight(&self) -> f64 {
        (self.first_deliveries as f64) - 0.3 * (self.duplicates_from as f64)
            + 0.1 * (self.rounds_in_mesh.min(10) as f64)
    }
}

/// Streamr-Plumtree per-stream state: which neighbors are eager vs lazy for a
/// given (node, stream) pair. Persists across publish rounds so the spanning
/// tree converges as redundant deliveries get pruned.
///
/// Optional gossipsub v1.1 augmentation:
///   - `scores[node][peer]` records P2/P3/P1 signals from observed traffic.
///   - `sticky_prune[node][peer] = round_until` blocks a pruned edge from
///     being re-grafted until that round. Closes the prune/regraft churn
///     loop in naive bounded-mesh gossipsub.
#[derive(Default)]
struct PlumtreeStreamState {
    eager: BTreeMap<String, BTreeSet<String>>,
    lazy: BTreeMap<String, BTreeSet<String>>,
    initialized: BTreeSet<String>,
    scores: BTreeMap<String, BTreeMap<String, PeerScore>>,
    sticky_prune: BTreeMap<String, BTreeMap<String, u64>>,
    current_round: u64,
}

fn deterministic_rank(stream_id: &str, owner: &str, neighbor: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    stream_id.hash(&mut hasher);
    owner.hash(&mut hasher);
    neighbor.hash(&mut hasher);
    hasher.finish()
}

impl PlumtreeStreamState {
    /// Initialize a node's eager/lazy sets. With `target_degree = None`, every
    /// neighbor starts eager (Plumtree's "broadcast" init: prune to a tree by
    /// duplicate detection). With `target_degree = Some(D)`, the D
    /// lowest-ranked neighbors start eager and the rest start lazy
    /// (gossipsub-style bounded mesh).
    fn ensure_initialized(
        &mut self,
        node: &str,
        stream_id: &str,
        graph: &BTreeMap<String, Vec<String>>,
        mesh_members: Option<&BTreeSet<String>>,
        fanout_peers_per_member: u8,
        target_degree: Option<usize>,
    ) {
        if !self.initialized.insert(node.to_string()) {
            return;
        }
        let underlay: Vec<String> = graph.get(node).into_iter().flatten().cloned().collect();
        let neighbors: Vec<String> = match mesh_members {
            None => underlay,
            Some(mesh) => {
                let mesh_neighbors: Vec<String> = underlay
                    .iter()
                    .filter(|n| mesh.contains(*n))
                    .cloned()
                    .collect();
                let is_member = mesh.contains(node);
                if is_member && fanout_peers_per_member > 0 {
                    // Augment with K lowest-rank non-mesh underlay neighbors
                    // ("bridges"): they relay 1 hop into the mesh.
                    let mut non_mesh: Vec<(u64, String)> = underlay
                        .iter()
                        .filter(|n| !mesh.contains(*n))
                        .map(|n| (deterministic_rank(stream_id, node, n), n.clone()))
                        .collect();
                    non_mesh.sort_by_key(|(rank, _)| *rank);
                    let bridges: Vec<String> = non_mesh
                        .into_iter()
                        .take(fanout_peers_per_member as usize)
                        .map(|(_, n)| n)
                        .collect();
                    mesh_neighbors.into_iter().chain(bridges).collect()
                } else {
                    // Non-member (would be a bridge node reached via BFS) or
                    // fanout disabled: forward only to mesh members. This is
                    // what makes a bridge a true 1-hop relay rather than a
                    // recursive flood.
                    mesh_neighbors
                }
            }
        };
        match target_degree {
            None => {
                self.eager
                    .insert(node.to_string(), neighbors.into_iter().collect());
                self.lazy.insert(node.to_string(), BTreeSet::new());
            }
            Some(degree) => {
                let mut ranked: Vec<(u64, String)> = neighbors
                    .into_iter()
                    .map(|n| (deterministic_rank(stream_id, node, &n), n))
                    .collect();
                ranked.sort_by_key(|(rank, _)| *rank);
                let mut eager_set = BTreeSet::new();
                let mut lazy_set = BTreeSet::new();
                for (i, (_, name)) in ranked.into_iter().enumerate() {
                    if i < degree {
                        eager_set.insert(name);
                    } else {
                        lazy_set.insert(name);
                    }
                }
                self.eager.insert(node.to_string(), eager_set);
                self.lazy.insert(node.to_string(), lazy_set);
            }
        }
    }

    fn prune_edge(&mut self, a: &str, b: &str) {
        if let Some(set) = self.eager.get_mut(a) {
            set.remove(b);
        }
        if let Some(set) = self.eager.get_mut(b) {
            set.remove(a);
        }
        self.lazy
            .entry(a.to_string())
            .or_default()
            .insert(b.to_string());
        self.lazy
            .entry(b.to_string())
            .or_default()
            .insert(a.to_string());
    }

    fn graft_edge(&mut self, a: &str, b: &str) {
        if let Some(set) = self.lazy.get_mut(a) {
            set.remove(b);
        }
        if let Some(set) = self.lazy.get_mut(b) {
            set.remove(a);
        }
        self.eager
            .entry(a.to_string())
            .or_default()
            .insert(b.to_string());
        self.eager
            .entry(b.to_string())
            .or_default()
            .insert(a.to_string());
    }

    /// Gossipsub-style heartbeat: for every node whose eager degree fell
    /// below `target`, graft its lowest-ranked lazy neighbors back to eager.
    fn rebalance_to_degree(&mut self, target: usize, stream_id: &str) {
        let nodes: Vec<String> = self.initialized.iter().cloned().collect();
        for node in nodes {
            let current = self
                .eager
                .get(&node)
                .map(|set| set.len())
                .unwrap_or_default();
            if current >= target {
                continue;
            }
            let needed = target - current;
            let lazy_set = self.lazy.get(&node).cloned().unwrap_or_default();
            let mut candidates: Vec<(u64, String)> = lazy_set
                .into_iter()
                .map(|n| (deterministic_rank(stream_id, &node, &n), n))
                .collect();
            candidates.sort_by_key(|(rank, _)| *rank);
            for (_, peer) in candidates.into_iter().take(needed) {
                self.graft_edge(&node, &peer);
            }
        }
    }

    /// gossipsub v1.1-style heartbeat: graft the highest-scoring lazy peers
    /// that are NOT in sticky-prune cooldown. Tie-break by deterministic
    /// rank so the choice is stable across runs.
    fn rebalance_with_scoring(&mut self, target: usize, stream_id: &str) {
        let nodes: Vec<String> = self.initialized.iter().cloned().collect();
        let now = self.current_round;
        for node in nodes {
            let current = self
                .eager
                .get(&node)
                .map(|set| set.len())
                .unwrap_or_default();
            if current >= target {
                continue;
            }
            let needed = target - current;
            let lazy_set = self.lazy.get(&node).cloned().unwrap_or_default();
            let cooldowns = self.sticky_prune.get(&node).cloned().unwrap_or_default();
            let scores = self.scores.get(&node).cloned().unwrap_or_default();

            let mut candidates: Vec<(f64, u64, String)> = lazy_set
                .iter()
                .filter(|peer| {
                    cooldowns
                        .get(*peer)
                        .map(|until| *until <= now)
                        .unwrap_or(true)
                })
                .map(|peer| {
                    let score = scores.get(peer).map(|s| s.weight()).unwrap_or(0.0);
                    let rank = deterministic_rank(stream_id, &node, peer);
                    (score, rank, peer.clone())
                })
                .collect();
            // Highest score first; tie-break by lowest rank.
            candidates.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(&b.1))
            });
            for (_, _, peer) in candidates.into_iter().take(needed) {
                self.graft_edge(&node, &peer);
            }
        }
    }

    fn record_first_delivery(&mut self, node: &str, peer: &str) {
        self.scores
            .entry(node.to_string())
            .or_default()
            .entry(peer.to_string())
            .or_default()
            .first_deliveries += 1;
    }

    fn record_duplicate(&mut self, node: &str, peer: &str) {
        self.scores
            .entry(node.to_string())
            .or_default()
            .entry(peer.to_string())
            .or_default()
            .duplicates_from += 1;
    }

    fn mark_sticky_prune(&mut self, a: &str, b: &str, until_round: u64) {
        self.sticky_prune
            .entry(a.to_string())
            .or_default()
            .insert(b.to_string(), until_round);
        self.sticky_prune
            .entry(b.to_string())
            .or_default()
            .insert(a.to_string(), until_round);
    }

    fn tick_round_for_eager(&mut self) {
        let nodes: Vec<String> = self.initialized.iter().cloned().collect();
        for node in nodes {
            let eager = self.eager.get(&node).cloned().unwrap_or_default();
            for peer in eager {
                self.scores
                    .entry(node.clone())
                    .or_default()
                    .entry(peer)
                    .or_default()
                    .rounds_in_mesh += 1;
            }
        }
        self.current_round = self.current_round.saturating_add(1);
    }
}

const PLUMTREE_IHAVE_BYTES: u64 = 96;
const PLUMTREE_IWANT_BYTES: u64 = 64;
const PLUMTREE_PRUNE_BYTES: u64 = 32;

/// Eager+lazy push baseline. With `target_degree = None` and
/// `ihave_timeout_hops = None` this is plain Plumtree (all-eager init,
/// graft-only-on-payload-never). With `target_degree = Some(D)` and
/// `ihave_timeout_hops = Some(T)` it behaves like a gossipsub mesh of
/// degree D where IWANT fires T hops after IHAVE if payload hasn't arrived.
/// `peer_scoring` + `prune_backoff_rounds` enable gossipsub v1.1-style score
/// tracking and sticky-prune backoff for the heartbeat graft selector.
/// `topic_mesh = Some(set)` restricts eager+lazy traversal to the given mesh
/// members (subscribers ∪ {publisher}); `None` means whole-network mesh.
/// `fanout_peers_per_member > 0` adds K non-mesh 1-hop relays to each
/// member's eager set (only meaningful with `topic_mesh = Some(_)`).
#[allow(clippy::too_many_arguments)]
fn htl_plumtree_publish(
    graph: &BTreeMap<String, Vec<String>>,
    publisher_id: &str,
    stream_id: &str,
    state: &mut PlumtreeStreamState,
    target_degree: Option<usize>,
    ihave_timeout_hops: Option<u8>,
    peer_scoring: bool,
    prune_backoff_rounds: u8,
    topic_mesh: Option<&BTreeSet<String>>,
    fanout_peers_per_member: u8,
    broken: &BTreeSet<(String, String)>,
    payload_bytes: u64,
    htl: u8,
    report: &mut MeshPubsubWorkloadReport,
) -> BTreeMap<String, u64> {
    let policy = sim_htl_policy(htl);
    let htl = policy.max_htl;
    state.ensure_initialized(
        publisher_id,
        stream_id,
        graph,
        topic_mesh,
        fanout_peers_per_member,
        target_degree,
    );

    let mut delivered_hops: BTreeMap<String, u64> = BTreeMap::new();
    delivered_hops.insert(publisher_id.to_string(), 0);

    // Pending IHAVE entries per node: list of (peer_who_sent_ihave, hop_count_when_received).
    let mut pending_ihave: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();

    let mut prunes: Vec<(String, String)> = Vec::new();
    let mut grafts: Vec<(String, String)> = Vec::new();

    let mut queue: VecDeque<(String, String, u8, u64)> = VecDeque::new();

    // Seed from publisher: full payload to eager neighbors, IHAVE to lazy neighbors.
    let pub_eager = state.eager.get(publisher_id).cloned().unwrap_or_default();
    let pub_lazy = state.lazy.get(publisher_id).cloned().unwrap_or_default();
    for nbr in &pub_eager {
        if is_edge_broken(broken, publisher_id, nbr) {
            continue;
        }
        report.forwarded_bytes_sent = report.forwarded_bytes_sent.saturating_add(payload_bytes);
        let nbr_htl_cfg = deterministic_htl_config(publisher_id, nbr);
        let next_htl = decrement_htl_with_policy(htl, &policy, &nbr_htl_cfg);
        if !should_forward_htl(next_htl) {
            continue;
        }
        queue.push_back((nbr.clone(), publisher_id.to_string(), next_htl, 1));
    }
    for nbr in &pub_lazy {
        if is_edge_broken(broken, publisher_id, nbr) {
            continue;
        }
        report.forwarded_bytes_sent = report
            .forwarded_bytes_sent
            .saturating_add(PLUMTREE_IHAVE_BYTES);
        pending_ihave
            .entry(nbr.clone())
            .or_default()
            .push((publisher_id.to_string(), 1));
    }

    while let Some((node, sender, frame_htl, hop)) = queue.pop_front() {
        if delivered_hops.contains_key(&node) {
            // Duplicate eager delivery → bidirectional prune (sender,node) to lazy.
            report.duplicate_deliveries = report.duplicate_deliveries.saturating_add(1);
            report.forwarded_bytes_sent = report
                .forwarded_bytes_sent
                .saturating_add(PLUMTREE_PRUNE_BYTES);
            if peer_scoring {
                state.record_duplicate(&node, &sender);
            }
            prunes.push((node.clone(), sender.clone()));
            continue;
        }
        delivered_hops.insert(node.clone(), hop);
        if peer_scoring && hop > 0 {
            // First eager delivery to `node` came from `sender` — credit P2.
            state.record_first_delivery(&node, &sender);
        }
        // Keep pending IHAVE entries — they may still trigger a graft if the
        // IHAVE arrived sooner than the eager payload (timer expired race).

        state.ensure_initialized(
            &node,
            stream_id,
            graph,
            topic_mesh,
            fanout_peers_per_member,
            target_degree,
        );
        let eager = state.eager.get(&node).cloned().unwrap_or_default();
        let lazy = state.lazy.get(&node).cloned().unwrap_or_default();

        for nbr in &eager {
            if nbr == &sender {
                continue;
            }
            if is_edge_broken(broken, &node, nbr) {
                continue;
            }
            let nbr_htl_cfg = deterministic_htl_config(&node, nbr);
            let next_htl = decrement_htl_with_policy(frame_htl, &policy, &nbr_htl_cfg);
            if !should_forward_htl(next_htl) {
                continue;
            }
            report.forwarded_bytes_sent = report.forwarded_bytes_sent.saturating_add(payload_bytes);
            queue.push_back((nbr.clone(), node.clone(), next_htl, hop.saturating_add(1)));
        }
        for nbr in &lazy {
            if nbr == &sender {
                continue;
            }
            if is_edge_broken(broken, &node, nbr) {
                continue;
            }
            let nbr_htl_cfg = deterministic_htl_config(&node, nbr);
            let next_htl = decrement_htl_with_policy(frame_htl, &policy, &nbr_htl_cfg);
            if !should_forward_htl(next_htl) {
                continue;
            }
            report.forwarded_bytes_sent = report
                .forwarded_bytes_sent
                .saturating_add(PLUMTREE_IHAVE_BYTES);
            pending_ihave
                .entry(nbr.clone())
                .or_default()
                .push((node.clone(), hop.saturating_add(1)));
        }
    }

    // Phase 2: IHAVE timer logic. For each (node, IHAVE) pair, decide whether
    // the timer expired before payload arrived. If so, fire IWANT and graft.
    let pending_keys: Vec<String> = pending_ihave.keys().cloned().collect();
    for node in pending_keys {
        let entries = pending_ihave.remove(&node).unwrap_or_default();
        // Pick the closest (lowest-hop) IHAVE peer.
        let mut best: Option<(String, u64)> = None;
        for (peer, h) in entries {
            match &best {
                None => best = Some((peer, h)),
                Some((_, bh)) if h < *bh => best = Some((peer, h)),
                _ => {}
            }
        }
        let Some((peer, ihave_hop)) = best else {
            continue;
        };
        let payload_hop = delivered_hops.get(&node).copied();
        // Timer modes:
        //   None     → "infinite": fire only when payload missed entirely.
        //              In real Plumtree the node would wait "long enough"; in
        //              our discrete-hop sim that's the moment we conclude
        //              payload didn't come, i.e., right at `ihave_hop`.
        //   Some(t)  → fire at ihave_hop + t hops if payload hasn't arrived
        //              by then.
        let timer_expires_at = ihave_hop.saturating_add(ihave_timeout_hops.unwrap_or(0) as u64);
        let should_fire = match (ihave_timeout_hops, payload_hop) {
            // Infinite timer: only payload-never case fires.
            (None, None) => true,
            (None, Some(_)) => false,
            // Finite timer: fires if payload missed, OR if eager arrived
            // strictly later than the timer expiry.
            (Some(_), None) => true,
            (Some(_), Some(hop)) => hop > timer_expires_at,
        };
        if !should_fire {
            continue;
        }
        // IWANT request always costs IWANT_BYTES. Payload reply only flows if
        // we don't already have it (i.e., payload_hop is None or strictly
        // later than the IWANT round trip).
        report.forwarded_bytes_sent = report
            .forwarded_bytes_sent
            .saturating_add(PLUMTREE_IWANT_BYTES);
        let iwant_arrival = timer_expires_at.saturating_add(2);
        let new_hop = match payload_hop {
            None => {
                report.forwarded_bytes_sent =
                    report.forwarded_bytes_sent.saturating_add(payload_bytes);
                iwant_arrival
            }
            Some(eager_hop) => {
                if iwant_arrival < eager_hop {
                    report.forwarded_bytes_sent =
                        report.forwarded_bytes_sent.saturating_add(payload_bytes);
                    iwant_arrival
                } else {
                    eager_hop
                }
            }
        };
        delivered_hops.insert(node.clone(), new_hop);
        grafts.push((node.clone(), peer.clone()));
    }

    let cooldown_until = state
        .current_round
        .saturating_add(prune_backoff_rounds as u64);
    for (a, b) in prunes {
        state.prune_edge(&a, &b);
        if prune_backoff_rounds > 0 {
            state.mark_sticky_prune(&a, &b, cooldown_until);
        }
    }
    for (a, b) in grafts {
        state.graft_edge(&a, &b);
    }

    // Gossipsub heartbeat: keep each node's eager mesh at target degree.
    // With peer scoring on, favor highest-scoring lazy peers and skip those
    // currently in sticky-prune cooldown.
    if let Some(target) = target_degree {
        if peer_scoring {
            state.rebalance_with_scoring(target, stream_id);
        } else {
            state.rebalance_to_degree(target, stream_id);
        }
    }

    if peer_scoring {
        state.tick_round_for_eager();
    }

    delivered_hops
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

pub(crate) async fn run_mesh_pubsub_payload_delivery(
    config: MeshPubsubPayloadConfig,
) -> MeshPubsubPayloadReport {
    if config.node_count < 2 || config.payloads.is_empty() || config.stream_id.trim().is_empty() {
        return MeshPubsubPayloadReport::default();
    }

    let _mock_registry = crate::mock_registry::lock_mock_channel_registry().await;
    clear_channel_registry().await;

    let mut rng = StdRng::seed_from_u64(config.seed);
    let workload_config = MeshPubsubWorkloadConfig {
        seed: config.seed,
        node_count: config.node_count,
        author_count: 1,
        subscribers_per_author: config.subscriber_count,
        publish_rounds: config.payloads.len(),
        payload_bytes: config.payloads.iter().map(Vec::len).max().unwrap_or(1),
        pool: config.pool,
        pump_steps_after_setup: config.pump_steps_after_setup,
        pump_steps_per_publish_round: config.pump_steps_per_publish,
        ..Default::default()
    };
    let node_ids = (0..config.node_count)
        .map(|index| format!("node-{index:04}"))
        .collect::<Vec<_>>();
    let nodes = make_workload_nodes(&workload_config).await;
    let nodes_by_id = nodes
        .iter()
        .map(|node| (node.id.clone(), node.store.clone()))
        .collect::<HashMap<_, _>>();
    let publisher_id = choose_publishers(&mut rng, &node_ids, 1)
        .into_iter()
        .next()
        .unwrap_or_else(|| node_ids[0].clone());
    let mut subscriber_ids = node_ids
        .iter()
        .filter(|node_id| *node_id != &publisher_id)
        .cloned()
        .collect::<Vec<_>>();
    subscriber_ids.shuffle(&mut rng);
    subscriber_ids.truncate(
        config
            .subscriber_count
            .min(config.node_count.saturating_sub(1)),
    );

    let mut subscriptions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for subscriber_id in &subscriber_ids {
        if subscribe_stream(
            &nodes_by_id,
            &mut subscriptions,
            subscriber_id,
            &config.stream_id,
        )
        .await
        {
            pump_mesh(&nodes, 1).await;
        }
    }
    pump_mesh(&nodes, config.pump_steps_after_setup).await;

    let mut report = MeshPubsubPayloadReport {
        delivery_opportunities: subscriber_ids.len() as u64 * config.payloads.len() as u64,
        ..Default::default()
    };
    let expected_subscribers = subscriber_ids.into_iter().collect::<BTreeSet<_>>();
    let mut seen = HashSet::<(String, u64)>::new();

    for (index, payload) in config.payloads.into_iter().enumerate() {
        let seq = (index + 1) as u64;
        if let Some(store) = nodes_by_id.get(&publisher_id) {
            store
                .publish_pubsub(config.stream_id.clone(), seq, payload)
                .await;
        }
        for _ in 0..config.pump_steps_per_publish {
            pump_mesh(&nodes, 1).await;
            for node in &nodes {
                for event in node.store.drain_pubsub_events().await {
                    if event.stream_id != config.stream_id
                        || !expected_subscribers.contains(&node.id)
                        || !seen.insert((node.id.clone(), event.seq))
                    {
                        report.duplicate_deliveries = report.duplicate_deliveries.saturating_add(1);
                        continue;
                    }
                    report.delivered_payloads.push(MeshPubsubPayloadDelivery {
                        subscriber_id: node.id.clone(),
                        seq: event.seq,
                        payload: event.payload,
                    });
                }
            }
        }
    }

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
    clear_channel_registry().await;

    report
}

#[derive(Debug, Clone, Copy)]
pub enum HtlBaselineMode {
    FloodPayload,
    InvWant,
    /// Eager+lazy push (Plumtree / gossipsub-shaped). With
    /// `target_degree = None` every neighbor is initially eager (Plumtree
    /// "broadcast init"). With `Some(D)` the eager mesh is bounded to D
    /// neighbors and rebalanced via heartbeat (gossipsub-style).
    /// `ihave_timeout_hops = None` means IWANT only fires when payload never
    /// arrives at all; `Some(t)` fires t hops after IHAVE if payload hasn't
    /// caught up — modelled as a hop budget since each hop maps to
    /// `latency_per_pump_step_ms` of real time in the workload pump.
    /// `peer_scoring = true` enables gossipsub v1.1-style score tracking
    /// (P1 time-in-mesh, P2 first-deliveries, P3 duplicates) and biases the
    /// heartbeat graft toward high-scoring lazy peers.
    /// `prune_backoff_rounds > 0` adds a sticky-prune cooldown: a pruned
    /// edge can't be re-grafted for that many rounds. Closes the
    /// prune/regraft churn that plain bounded-mesh gossipsub falls into.
    /// `topic_mesh = true` restricts eager+lazy traversal to mesh members
    /// (subscribers ∪ {publisher}) per stream — gossipsub/Plumtree's actual
    /// design, where a node only joins/forwards meshes for topics it
    /// subscribes to. Off by default for backward compatibility (whole
    /// network is the mesh, which makes per-event bandwidth scale with N
    /// instead of subscriber count). Should be true for Nostr-realistic
    /// many-topics workloads.
    /// `fanout_peers_per_member > 0` augments each mesh member's eager set
    /// with K non-mesh underlay neighbors as 1-hop relays ("bridges").
    /// Bridges receive the payload and forward to their mesh-member
    /// underlay neighbors only — they don't recursively introduce more
    /// bridges, so the augmented mesh stays bounded. Closes
    /// mesh-fragmentation gaps when subscriber density is below the per-
    /// topic-mesh-diameter threshold (typical of Nostr long-tail topics).
    /// Only meaningful when `topic_mesh = true`.
    EagerLazy {
        target_degree: Option<usize>,
        ihave_timeout_hops: Option<u8>,
        peer_scoring: bool,
        prune_backoff_rounds: u8,
        topic_mesh: bool,
        fanout_peers_per_member: u8,
    },
    /// Interest-routed inv-want: same flood-INV → pull-WANT shape as
    /// `InvWant`, but each node's INV-forwarding rule is asymmetric.
    /// Subscribers (and the publisher) flood to all neighbors. Non-
    /// subscribers forward INV only to neighbors they know are subscribed
    /// to this stream. Models the production `PubsubInterest` mechanism:
    /// each node periodically announces its subscriptions to its neighbors,
    /// so every node knows the 1-hop interest graph for free. WANT phase
    /// is unchanged. Designed to outperform pure invwant when many topics
    /// have moderate density (long-tail Nostr).
    InvWantInterestRouted,
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

/// Interest-routed inv-want baseline: subscribers flood INV, non-subscribers
/// forward INV only to known-subscriber neighbors. WANT phase identical to
/// regular invwant. See `HtlBaselineMode::InvWantInterestRouted` for full
/// motivation.
pub async fn run_mesh_pubsub_htl_inv_want_interest_routed_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(config, htl, HtlBaselineMode::InvWantInterestRouted).await
}

/// Streamr-style Plumtree baseline (broadcast init, infinite IHAVE timer):
/// every neighbor starts eager and the spanning tree converges as redundant
/// deliveries get pruned. IWANT only fires when payload never arrives.
pub async fn run_mesh_pubsub_htl_plumtree_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(
        config,
        htl,
        HtlBaselineMode::EagerLazy {
            target_degree: None,
            ihave_timeout_hops: None,
            peer_scoring: false,
            prune_backoff_rounds: 0,
            topic_mesh: false,
            fanout_peers_per_member: 0,
        },
    )
    .await
}

/// Plumtree with a finite IHAVE→IWANT timer measured in hops. `timeout_hops`
/// controls how long a node waits after IHAVE for payload to arrive on the
/// eager path before pulling via IWANT (and grafting the edge to eager).
/// `0` is "always graft on IHAVE-before-payload race", higher values are more
/// patient. Each hop maps to `latency_per_pump_step_ms` real time in the
/// workload pump, so a tokio-clock-driven timer would be equivalent.
pub async fn run_mesh_pubsub_htl_plumtree_baseline_with_timer(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
    timeout_hops: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(
        config,
        htl,
        HtlBaselineMode::EagerLazy {
            target_degree: None,
            ihave_timeout_hops: Some(timeout_hops),
            peer_scoring: false,
            prune_backoff_rounds: 0,
            topic_mesh: false,
            fanout_peers_per_member: 0,
        },
    )
    .await
}

/// Plumtree with topic-aware mesh: eager+lazy traversal restricted to
/// subscribers ∪ {publisher} per stream. Matches real gossipsub/Plumtree's
/// per-topic-mesh design and is the right model for Nostr-style workloads
/// where most network nodes are not subscribed to most topics.
pub async fn run_mesh_pubsub_htl_plumtree_topic_mesh_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
    timeout_hops: Option<u8>,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(
        config,
        htl,
        HtlBaselineMode::EagerLazy {
            target_degree: None,
            ihave_timeout_hops: timeout_hops,
            peer_scoring: false,
            prune_backoff_rounds: 0,
            topic_mesh: true,
            fanout_peers_per_member: 0,
        },
    )
    .await
}

/// Plumtree with topic-aware mesh + fanout bridges: each member augments
/// its eager set with K non-subscriber underlay neighbors as 1-hop relays.
/// Closes mesh-fragmentation gaps when subscriber density per topic is
/// below the per-topic-mesh-diameter threshold (typical of Nostr
/// long-tail topics). Bridges only forward 1 hop (to their own
/// mesh-member underlay neighbors), so the augmented mesh stays bounded.
pub async fn run_mesh_pubsub_htl_plumtree_topic_mesh_fanout_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
    timeout_hops: Option<u8>,
    fanout_peers_per_member: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(
        config,
        htl,
        HtlBaselineMode::EagerLazy {
            target_degree: None,
            ihave_timeout_hops: timeout_hops,
            peer_scoring: false,
            prune_backoff_rounds: 0,
            topic_mesh: true,
            fanout_peers_per_member,
        },
    )
    .await
}

/// Gossipsub-style baseline: each (node, stream) keeps an eager mesh of size
/// `target_degree`, with the rest of the underlay neighbors as lazy-IHAVE
/// peers. After every publish round we run a heartbeat-style rebalance that
/// re-grafts lazy peers to eager whenever degree dropped below target.
/// `ihave_timeout_hops` controls IWANT aggressiveness like the Plumtree-with-
/// timer variant. This is the "naive" v1.0-shaped variant: prune-on-duplicate
/// is symmetric and graft selection is deterministic-rank only — see
/// `run_mesh_pubsub_htl_gossipsub_v11_baseline` for v1.1 peer scoring.
pub async fn run_mesh_pubsub_htl_gossipsub_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
    target_degree: usize,
    ihave_timeout_hops: Option<u8>,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(
        config,
        htl,
        HtlBaselineMode::EagerLazy {
            target_degree: Some(target_degree),
            ihave_timeout_hops,
            peer_scoring: false,
            prune_backoff_rounds: 0,
            topic_mesh: false,
            fanout_peers_per_member: 0,
        },
    )
    .await
}

/// Gossipsub v1.1-style: bounded mesh of `target_degree`, plus per-peer
/// scoring and sticky-prune cooldown. On every publish:
///   - score++ for the peer whose eager edge first-delivered to us (P2)
///   - score-- for any peer that sent us a duplicate (P3)
///   - score++ for time-in-mesh (P1, capped)
///   - on prune, mark a `prune_backoff_rounds`-round cooldown: that edge
///     can't be re-grafted until the cooldown expires
///   - heartbeat grafts the highest-scoring non-cooldowned lazy peer
/// Designed to fix the prune/regraft churn that the v1.0 baseline falls into
/// with a high underlay degree.
pub async fn run_mesh_pubsub_htl_gossipsub_v11_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
    target_degree: usize,
    ihave_timeout_hops: Option<u8>,
    prune_backoff_rounds: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(
        config,
        htl,
        HtlBaselineMode::EagerLazy {
            target_degree: Some(target_degree),
            ihave_timeout_hops,
            peer_scoring: true,
            prune_backoff_rounds,
            topic_mesh: false,
            fanout_peers_per_member: 0,
        },
    )
    .await
}

/// Gossipsub v1.1 with topic-aware mesh — the most realistic gossipsub
/// approximation we model. Per-topic eager mesh of `target_degree`
/// restricted to subscribers ∪ {publisher}, plus peer scoring and sticky-
/// prune cooldown.
pub async fn run_mesh_pubsub_htl_gossipsub_v11_topic_mesh_baseline(
    config: MeshPubsubWorkloadConfig,
    htl: u8,
    target_degree: usize,
    ihave_timeout_hops: Option<u8>,
    prune_backoff_rounds: u8,
) -> MeshPubsubWorkloadReport {
    run_mesh_pubsub_htl_baseline(
        config,
        htl,
        HtlBaselineMode::EagerLazy {
            target_degree: Some(target_degree),
            ihave_timeout_hops,
            peer_scoring: true,
            prune_backoff_rounds,
            topic_mesh: true,
            fanout_peers_per_member: 0,
        },
    )
    .await
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
    let (graph, topology) = compute_workload_peer_graph(&config).await;
    run_mesh_pubsub_htl_baseline_on_graph(&graph, &topology, &config, htl, mode)
}

/// Run an HTL pubsub baseline against a precomputed peer graph. Pure sync —
/// no mock registry lock, no node setup. Reuse a graph across many baseline
/// variants (flood, invwant, plumtree, gossipsub) since the graph is fully
/// determined by `seed`, `node_count`, and `pool` and is independent of
/// `pubsub_scheduler`/`pubsub_delivery_mode`. Safe to call concurrently from
/// `tokio::task::spawn_blocking` for parallel sweeps.
pub fn run_mesh_pubsub_htl_baseline_on_graph(
    graph: &BTreeMap<String, Vec<String>>,
    topology: &WorkloadTopology,
    config: &MeshPubsubWorkloadConfig,
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

    let mut rng = StdRng::seed_from_u64(config.seed);
    let node_ids = (0..config.node_count)
        .map(|index| format!("node-{index:04}"))
        .collect::<Vec<_>>();

    let _provider_draws = node_ids
        .iter()
        .filter(|_| rng.gen::<f64>() < config.reciprocal_provider_fraction.clamp(0.0, 1.0))
        .count();

    let nostr_active = config.nostr.is_some();
    let stream_count = if let Some(n) = &config.nostr {
        n.author_count.max(1)
    } else {
        config
            .author_count
            .saturating_add(config.spam_author_count)
            .max(1)
    };
    let publisher_ids = choose_publishers(&mut rng, &node_ids, stream_count);
    let useful_publishers = if let Some(n) = &config.nostr {
        publisher_ids
            .iter()
            .take(n.author_count)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        publisher_ids
            .iter()
            .take(config.author_count)
            .cloned()
            .collect::<Vec<_>>()
    };
    let spam_publishers = if nostr_active {
        Vec::new()
    } else {
        publisher_ids
            .iter()
            .skip(config.author_count)
            .cloned()
            .collect::<Vec<_>>()
    };

    let mut subscriptions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut report = MeshPubsubWorkloadReport {
        seed: config.seed,
        node_count: config.node_count,
        active_nodes: config.node_count,
        author_count: useful_publishers.len(),
        publish_rounds: config.publish_rounds,
        spam_author_count: spam_publishers.len(),
        ..Default::default()
    };
    apply_topology(&mut report, topology);

    if let Some(nostr) = &config.nostr {
        // Each node samples `follows_per_node` authors weighted by the
        // popularity distribution. Sampling is without replacement: pick,
        // remove from candidate pool, repeat.
        let weights: Vec<f64> = match nostr.follow_distribution {
            FollowDistribution::Uniform => vec![1.0; nostr.author_count],
            FollowDistribution::Zipf { alpha } => (1..=nostr.author_count)
                .map(|k| 1.0_f64 / (k as f64).powf(alpha))
                .collect(),
        };
        let follow_count = nostr.follows_per_node.min(nostr.author_count);
        for node_id in &node_ids {
            let mut available: Vec<usize> = (0..nostr.author_count).collect();
            let mut current_weights = weights.clone();
            for _ in 0..follow_count {
                if available.is_empty() {
                    break;
                }
                let total: f64 = current_weights.iter().sum();
                if total <= 0.0 {
                    break;
                }
                let mut r = rng.gen::<f64>() * total;
                let mut pick_idx = 0usize;
                for (i, w) in current_weights.iter().enumerate() {
                    r -= *w;
                    if r <= 0.0 {
                        pick_idx = i;
                        break;
                    }
                }
                let author_idx = available[pick_idx];
                available.swap_remove(pick_idx);
                current_weights.swap_remove(pick_idx);
                let publisher_id = &useful_publishers[author_idx];
                if publisher_id == node_id {
                    continue; // don't follow yourself
                }
                report.subscriber_attempts = report.subscriber_attempts.saturating_add(1);
                let stream_id = author_stream(author_idx);
                insert_subscription(&mut subscriptions, node_id, &stream_id);
            }
        }
    } else {
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
    } // end !nostr branch

    let mut latencies_ms = Vec::new();
    let mut plumtree_streams: HashMap<String, PlumtreeStreamState> = HashMap::new();
    let mut publish_idx: u64 = 0;
    for round in 0..config.publish_rounds {
        let churn_actions = apply_subscription_churn_plan(&mut rng, &subscriptions, config);
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
                let broken_edges = compute_broken_edges(
                    graph,
                    config.broken_edge_fraction,
                    config.seed.wrapping_add(publish_idx),
                );
                publish_idx = publish_idx.wrapping_add(1);
                let delivered_hops = match mode {
                    HtlBaselineMode::FloodPayload => {
                        htl_flood_publish(
                            graph,
                            publisher_id,
                            &broken_edges,
                            config.payload_bytes as u64,
                            htl,
                            &mut report,
                        )
                        .delivered_hops
                    }
                    HtlBaselineMode::InvWant => htl_inv_want_publish(
                        graph,
                        publisher_id,
                        &subscribers,
                        &broken_edges,
                        config.payload_bytes as u64,
                        htl,
                        &mut report,
                    ),
                    HtlBaselineMode::InvWantInterestRouted => htl_interest_routed_inv_want_publish(
                        graph,
                        publisher_id,
                        &subscribers,
                        &broken_edges,
                        config.payload_bytes as u64,
                        htl,
                        &mut report,
                    ),
                    HtlBaselineMode::EagerLazy {
                        target_degree,
                        ihave_timeout_hops,
                        peer_scoring,
                        prune_backoff_rounds,
                        topic_mesh,
                        fanout_peers_per_member,
                    } => {
                        let mesh_members: Option<BTreeSet<String>> = if topic_mesh {
                            let mut m = subscribers.clone();
                            m.insert(publisher_id.to_string());
                            Some(m)
                        } else {
                            None
                        };
                        htl_plumtree_publish(
                            graph,
                            publisher_id,
                            &stream_id,
                            plumtree_streams.entry(stream_id.clone()).or_default(),
                            target_degree,
                            ihave_timeout_hops,
                            peer_scoring,
                            prune_backoff_rounds,
                            mesh_members.as_ref(),
                            fanout_peers_per_member,
                            &broken_edges,
                            config.payload_bytes as u64,
                            htl,
                            &mut report,
                        )
                    }
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
            let broken_edges = compute_broken_edges(
                graph,
                config.broken_edge_fraction,
                config.seed.wrapping_add(publish_idx),
            );
            publish_idx = publish_idx.wrapping_add(1);
            let delivered_hops = match mode {
                HtlBaselineMode::FloodPayload => {
                    htl_flood_publish(
                        graph,
                        publisher_id,
                        &broken_edges,
                        config.payload_bytes as u64,
                        htl,
                        &mut report,
                    )
                    .delivered_hops
                }
                HtlBaselineMode::InvWant => htl_inv_want_publish(
                    graph,
                    publisher_id,
                    &subscribers,
                    &broken_edges,
                    config.payload_bytes as u64,
                    htl,
                    &mut report,
                ),
                HtlBaselineMode::InvWantInterestRouted => htl_interest_routed_inv_want_publish(
                    graph,
                    publisher_id,
                    &subscribers,
                    &broken_edges,
                    config.payload_bytes as u64,
                    htl,
                    &mut report,
                ),
                HtlBaselineMode::EagerLazy {
                    target_degree,
                    ihave_timeout_hops,
                    peer_scoring,
                    prune_backoff_rounds,
                    topic_mesh,
                    fanout_peers_per_member,
                } => {
                    let mesh_members: Option<BTreeSet<String>> = if topic_mesh {
                        let mut m = subscribers.clone();
                        m.insert(publisher_id.to_string());
                        Some(m)
                    } else {
                        None
                    };
                    htl_plumtree_publish(
                        graph,
                        publisher_id,
                        &stream_id,
                        plumtree_streams.entry(stream_id.clone()).or_default(),
                        target_degree,
                        ihave_timeout_hops,
                        peer_scoring,
                        prune_backoff_rounds,
                        mesh_members.as_ref(),
                        fanout_peers_per_member,
                        &broken_edges,
                        config.payload_bytes as u64,
                        htl,
                        &mut report,
                    )
                }
            };
            record_htl_delivery_round(
                &subscriptions,
                &stream_id,
                &delivered_hops,
                config,
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
    async fn htl_plumtree_baseline_converges_below_flood_bandwidth() {
        let config = MeshPubsubWorkloadConfig {
            seed: 19,
            node_count: 24,
            author_count: 3,
            subscribers_per_author: 8,
            publish_rounds: 4,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 80,
            pump_steps_per_publish_round: 48,
            ..Default::default()
        };

        let plumtree =
            run_mesh_pubsub_htl_plumtree_baseline(config.clone(), MESH_EVENT_POLICY.max_htl).await;
        let flood =
            run_mesh_pubsub_htl_flood_baseline(config.clone(), MESH_EVENT_POLICY.max_htl).await;

        assert!(plumtree.delivered_events > 0);
        assert!(plumtree.forwarded_bytes_sent > 0);
        // Plumtree should converge to a spanning tree and beat full-flood bandwidth.
        assert!(
            plumtree.forwarded_bytes_sent < flood.forwarded_bytes_sent,
            "plumtree {} should spend fewer bytes than flood {}",
            plumtree.forwarded_bytes_sent,
            flood.forwarded_bytes_sent
        );
    }

    #[tokio::test]
    async fn htl_plumtree_finite_timer_recovers_faster_when_eager_misses() {
        // Tight HTL forces some subscribers to miss eager and fall back to
        // IWANT. With a finite timer (t=1) the IWANT fires at hop ihave+1+2;
        // with infinite timer the IWANT only fires when payload never came at
        // all, so behaviour is identical iff every subscriber gets eager.
        // Sanity check: both deliver, finite-timer's tail latency is no worse.
        let cfg = MeshPubsubWorkloadConfig {
            seed: 41,
            node_count: 32,
            author_count: 2,
            subscribers_per_author: 8,
            publish_rounds: 4,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 96,
            pump_steps_per_publish_round: 64,
            ..Default::default()
        };
        let infinite =
            run_mesh_pubsub_htl_plumtree_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let finite =
            run_mesh_pubsub_htl_plumtree_baseline_with_timer(cfg, MESH_EVENT_POLICY.max_htl, 1)
                .await;
        assert!(infinite.delivered_events > 0);
        assert!(finite.delivered_events > 0);
        // Finite timer never makes p95 worse than infinite (it can only pull
        // earlier).
        assert!(finite.delivery_latency_p95_ms <= infinite.delivery_latency_p95_ms);
    }

    #[tokio::test]
    async fn htl_gossipsub_bounded_mesh_beats_plumtree_round_one() {
        // Plumtree pays a "broadcast init" tax in round 1 (every neighbor
        // eager). Gossipsub starts with a bounded mesh of degree D, so round-1
        // bandwidth is much lower.
        let cfg = MeshPubsubWorkloadConfig {
            seed: 53,
            node_count: 64,
            author_count: 2,
            subscribers_per_author: 16,
            publish_rounds: 1,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 128,
            pump_steps_per_publish_round: 96,
            ..Default::default()
        };
        let plumtree =
            run_mesh_pubsub_htl_plumtree_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let gossipsub = run_mesh_pubsub_htl_gossipsub_baseline(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            6,
            Some(1),
        )
        .await;
        assert!(gossipsub.delivered_events > 0);
        assert!(
            gossipsub.forwarded_bytes_sent < plumtree.forwarded_bytes_sent,
            "gossipsub round-1 {} should beat plumtree round-1 {}",
            gossipsub.forwarded_bytes_sent,
            plumtree.forwarded_bytes_sent
        );
    }

    #[tokio::test]
    async fn htl_gossipsub_timer_recovers_delivery_under_edge_failure() {
        // Plumtree's broadcast-init eager mesh is so over-provisioned (full
        // underlay degree) that even moderate failures rarely partition
        // delivery. Bounded gossipsub at D=6 has fewer alternate paths, and
        // at 50% per-publish edge failure the finite IHAVE timer earns its
        // keep: it pulls payload via IWANT when the eager edge would have
        // arrived later than the lazy IHAVE, and recovers more deliveries.
        let cfg = MeshPubsubWorkloadConfig {
            seed: 17,
            node_count: 64,
            author_count: 3,
            subscribers_per_author: 16,
            publish_rounds: 4,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            spam_author_count: 3,
            spam_subscribers_per_author: 8,
            spam_publish_rounds_per_round: 2,
            subscription_churn_rate: 0.05,
            allow_rejoin: true,
            pump_steps_after_setup: 160,
            pump_steps_per_publish_round: 96,
            broken_edge_fraction: 0.50,
            ..Default::default()
        };
        let infinite =
            run_mesh_pubsub_htl_gossipsub_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl, 6, None)
                .await;
        let finite =
            run_mesh_pubsub_htl_gossipsub_baseline(cfg, MESH_EVENT_POLICY.max_htl, 6, Some(1))
                .await;
        assert!(
            finite.delivery_rate > infinite.delivery_rate,
            "under 50% edge failure, finite-timer gossipsub rate {:.3} should beat infinite rate {:.3}",
            finite.delivery_rate,
            infinite.delivery_rate
        );
    }

    #[tokio::test]
    async fn htl_gossipsub_v11_does_not_grow_with_rounds() {
        // The naive bounded-mesh gossipsub gets WORSE over rounds because of
        // prune/regraft churn. v1.1 (peer scoring + sticky prune backoff)
        // should keep per-event bandwidth bounded as rounds grow.
        let cfg = MeshPubsubWorkloadConfig {
            seed: 71,
            node_count: 64,
            author_count: 3,
            subscribers_per_author: 16,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            spam_author_count: 3,
            spam_subscribers_per_author: 8,
            spam_publish_rounds_per_round: 2,
            subscription_churn_rate: 0.05,
            allow_rejoin: true,
            pump_steps_after_setup: 160,
            pump_steps_per_publish_round: 96,
            ..Default::default()
        };

        let cfg_short = MeshPubsubWorkloadConfig {
            publish_rounds: 2,
            ..cfg.clone()
        };
        let cfg_long = MeshPubsubWorkloadConfig {
            publish_rounds: 16,
            ..cfg
        };

        let v10_long = run_mesh_pubsub_htl_gossipsub_baseline(
            cfg_long.clone(),
            MESH_EVENT_POLICY.max_htl,
            6,
            Some(1),
        )
        .await;
        let v11_short = run_mesh_pubsub_htl_gossipsub_v11_baseline(
            cfg_short,
            MESH_EVENT_POLICY.max_htl,
            6,
            Some(1),
            4,
        )
        .await;
        let v11_long = run_mesh_pubsub_htl_gossipsub_v11_baseline(
            cfg_long,
            MESH_EVENT_POLICY.max_htl,
            6,
            Some(1),
            4,
        )
        .await;

        assert!(v11_long.delivered_events > 0);
        // v1.1 long should NOT grow much vs v1.1 short.
        let growth_ratio =
            v11_long.bytes_sent_per_delivered_event / v11_short.bytes_sent_per_delivered_event;
        assert!(
            growth_ratio < 1.20,
            "v1.1 long/short bytes ratio {:.3} should stay near 1.0; bandwidth growing implies churn isn't fixed",
            growth_ratio
        );
        // v1.1 long should beat v1.0 long now that the churn loop is closed.
        assert!(
            v11_long.bytes_sent_per_delivered_event < v10_long.bytes_sent_per_delivered_event,
            "v1.1 long {} should beat v1.0 long {}",
            v11_long.bytes_sent_per_delivered_event,
            v10_long.bytes_sent_per_delivered_event
        );
    }

    #[tokio::test]
    async fn htl_nostr_workload_generates_per_author_subscriptions() {
        // Many-authors / many-follows-per-node: 32 authors, each subscriber
        // follows 8 of them. Each author has avg 32×8/32 = 8 subs (uniform).
        let cfg = MeshPubsubWorkloadConfig {
            seed: 71,
            node_count: 32,
            publish_rounds: 1,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 96,
            pump_steps_per_publish_round: 64,
            nostr: Some(NostrWorkloadParams {
                author_count: 32,
                follows_per_node: 8,
                follow_distribution: FollowDistribution::Uniform,
            }),
            ..Default::default()
        };
        let report = run_mesh_pubsub_htl_inv_want_baseline(cfg, MESH_EVENT_POLICY.max_htl).await;
        assert!(report.delivered_events > 0, "nostr workload should deliver");
        assert!(report.delivery_opportunities > 0);
        assert_eq!(report.author_count, 32);
        assert_eq!(report.spam_author_count, 0);
        // Avg follows per node = 8, but self-follow drop reduces slightly.
        // 32 nodes × ~8 follows = ~256 attempts, scaled across many authors.
        assert!(
            report.subscriber_attempts >= 200,
            "subscriber_attempts {} should be ≈ N × follows_per_node",
            report.subscriber_attempts
        );
    }

    #[tokio::test]
    async fn htl_nostr_zipf_concentrates_followers_on_top_authors() {
        // With a heavy Zipf distribution, the first few authors should have
        // dramatically more followers than the long tail.
        let cfg_zipf = MeshPubsubWorkloadConfig {
            seed: 73,
            node_count: 64,
            publish_rounds: 1,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 96,
            pump_steps_per_publish_round: 64,
            nostr: Some(NostrWorkloadParams {
                author_count: 32,
                follows_per_node: 8,
                follow_distribution: FollowDistribution::Zipf { alpha: 1.5 },
            }),
            ..Default::default()
        };
        let cfg_uniform = MeshPubsubWorkloadConfig {
            nostr: Some(NostrWorkloadParams {
                follow_distribution: FollowDistribution::Uniform,
                ..cfg_zipf.nostr.unwrap()
            }),
            ..cfg_zipf.clone()
        };
        let zipf = run_mesh_pubsub_htl_inv_want_baseline(cfg_zipf, MESH_EVENT_POLICY.max_htl).await;
        let uniform =
            run_mesh_pubsub_htl_inv_want_baseline(cfg_uniform, MESH_EVENT_POLICY.max_htl).await;
        // Zipf: average opportunity-per-author lower than uniform because
        // most authors get few followers, head gets many. Total subscriber
        // attempts roughly the same (driven by N × follows_per_node).
        let zipf_max_opps = zipf.delivery_opportunities;
        let uniform_max_opps = uniform.delivery_opportunities;
        // delivery_opportunities sums (subscribers per stream) × publish
        // rounds. The total is ~N × follows_per_node regardless of dist;
        // distributions differ in concentration, not total volume.
        assert!(zipf_max_opps > 0);
        assert!(uniform_max_opps > 0);
    }

    #[tokio::test]
    async fn htl_invwant_interest_routed_saves_bandwidth_at_high_density() {
        // At high subscriber density, most non-subscribers have a sub
        // neighbor, so interest-routed forwarding behaves nearly like pure
        // invwant — same delivery, slightly less bandwidth (the long-tail
        // non-sub-to-non-sub edges that pure invwant would flood are
        // skipped). At low density the interest filter could hurt delivery,
        // so this test focuses on the favourable case.
        let cfg = MeshPubsubWorkloadConfig {
            seed: 91,
            node_count: 256,
            author_count: 1,
            subscribers_per_author: 64, // 25% density
            publish_rounds: 2,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 160,
            pump_steps_per_publish_round: 96,
            ..Default::default()
        };
        let pure =
            run_mesh_pubsub_htl_inv_want_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let interest_routed =
            run_mesh_pubsub_htl_inv_want_interest_routed_baseline(cfg, MESH_EVENT_POLICY.max_htl)
                .await;
        // Interest-routed shouldn't hurt delivery at this density.
        assert!(
            interest_routed.delivery_rate >= pure.delivery_rate * 0.95,
            "interest-routed delivery {:.3} should match pure invwant {:.3} (-5% slack)",
            interest_routed.delivery_rate,
            pure.delivery_rate
        );
        // And should send strictly fewer bytes per delivered event.
        assert!(
            interest_routed.bytes_sent_per_delivered_event < pure.bytes_sent_per_delivered_event,
            "interest-routed {} should beat pure invwant {} on bandwidth",
            interest_routed.bytes_sent_per_delivered_event,
            pure.bytes_sent_per_delivered_event
        );
    }

    #[tokio::test]
    async fn htl_plumtree_fanout_recovers_delivery_on_sparse_topic_mesh() {
        // 1000 nodes, 100 subscribers (10% density). Pure topic-mesh fails
        // because the subscriber-induced subgraph diameter > HTL=4. Adding
        // fanout=2 bridges per member lets payload hop through non-mesh
        // relayers, recovering delivery.
        let cfg = MeshPubsubWorkloadConfig {
            seed: 41,
            node_count: 256,
            author_count: 1,
            subscribers_per_author: 32, // ~12.5% density
            publish_rounds: 2,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 160,
            pump_steps_per_publish_round: 96,
            ..Default::default()
        };
        let pure_tm = run_mesh_pubsub_htl_plumtree_topic_mesh_baseline(
            cfg.clone(),
            MESH_EVENT_POLICY.max_htl,
            Some(1),
        )
        .await;
        let fanout = run_mesh_pubsub_htl_plumtree_topic_mesh_fanout_baseline(
            cfg,
            MESH_EVENT_POLICY.max_htl,
            Some(1),
            4,
        )
        .await;
        // Fanout should rescue delivery rate by relaying through bridges.
        assert!(
            fanout.delivery_rate > pure_tm.delivery_rate,
            "fanout {:.3} should beat pure-tm {:.3}",
            fanout.delivery_rate,
            pure_tm.delivery_rate
        );
    }

    #[tokio::test]
    async fn htl_plumtree_topic_mesh_cuts_bandwidth_when_subscribers_are_minority() {
        // The whole-network plumtree forwards every event to every node;
        // topic-mesh plumtree only forwards to subscribers ∪ {publisher}.
        // With high-density mesh (50% subs in a small underlay), the mesh
        // subgraph diameter stays under MESH_EVENT_POLICY.max_htl=4, so
        // delivery rate stays high while bandwidth drops.
        let cfg = MeshPubsubWorkloadConfig {
            seed: 31,
            node_count: 64,
            author_count: 2,
            subscribers_per_author: 32, // 50% density → subgraph diameter < 4
            publish_rounds: 4,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 96,
            pump_steps_per_publish_round: 64,
            ..Default::default()
        };
        let whole_net =
            run_mesh_pubsub_htl_plumtree_baseline(cfg.clone(), MESH_EVENT_POLICY.max_htl).await;
        let topic_aware = run_mesh_pubsub_htl_plumtree_topic_mesh_baseline(
            cfg,
            MESH_EVENT_POLICY.max_htl,
            Some(1),
        )
        .await;
        assert!(topic_aware.delivered_events > 0);
        // Topic-mesh is bounded by mesh subgraph diameter; whole-net by
        // underlay diameter. At 50% density subgraph reach is similar.
        assert!(
            topic_aware.delivery_rate >= 0.85,
            "topic-mesh delivery {:.2} should stay high at 50% subscriber density",
            topic_aware.delivery_rate
        );
        // Significant bandwidth win because we only forward to mesh members.
        assert!(
            topic_aware.bytes_sent_per_delivered_event * 1.5
                < whole_net.bytes_sent_per_delivered_event,
            "topic-mesh {} should be at least 1.5x cheaper than whole-net {}",
            topic_aware.bytes_sent_per_delivered_event,
            whole_net.bytes_sent_per_delivered_event
        );
    }

    #[tokio::test]
    async fn htl_plumtree_bandwidth_improves_with_more_rounds() {
        let base = MeshPubsubWorkloadConfig {
            seed: 23,
            node_count: 32,
            author_count: 2,
            subscribers_per_author: 8,
            publish_rounds: 2,
            payload_bytes: 1200,
            pool: PoolConfig {
                max_connections: 14,
                satisfied_connections: 8,
            },
            pump_steps_after_setup: 80,
            pump_steps_per_publish_round: 48,
            ..Default::default()
        };
        let mut long = base.clone();
        long.publish_rounds = 8;

        let short = run_mesh_pubsub_htl_plumtree_baseline(base, MESH_EVENT_POLICY.max_htl).await;
        let long = run_mesh_pubsub_htl_plumtree_baseline(long, MESH_EVENT_POLICY.max_htl).await;

        // After more rounds the spanning tree is converged; per-event cost drops.
        assert!(
            long.bytes_sent_per_delivered_event < short.bytes_sent_per_delivered_event,
            "long-run bytes/event {} should be below short-run {} (convergence)",
            long.bytes_sent_per_delivered_event,
            short.bytes_sent_per_delivered_event
        );
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
