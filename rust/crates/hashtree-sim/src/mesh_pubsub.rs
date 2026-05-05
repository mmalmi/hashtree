//! Production pubsub workload driver using the shared mesh store core.
//!
//! This module intentionally contains only workload orchestration and reporting.
//! Pubsub routing, dedupe, useful-byte credit, and fanout scheduling all run
//! through `hashtree_network::MeshStoreCore`.

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use hashtree_core::MemoryStore;
use hashtree_network::{
    clear_channel_registry, MeshRouter, MeshRoutingConfig, MeshStoreCore, MockConnectionFactory,
    MockLatencyMode, MockRelay, MockRelayTransport, PoolConfig, PoolSettings,
    PubsubSchedulerConfig, SignalingTransport, SimMeshStore,
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

fn choose_publishers(rng: &mut StdRng, node_ids: &[String], stream_count: usize) -> Vec<String> {
    let mut pool = node_ids.to_vec();
    pool.shuffle(rng);
    (0..stream_count)
        .map(|index| pool[index % pool.len()].clone())
        .collect()
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
    let inserted = subscriptions
        .entry(stream_id.to_string())
        .or_default()
        .insert(subscriber_id.to_string());
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
    let removed = subscriptions
        .get_mut(stream_id)
        .map(|subscribers| subscribers.remove(subscriber_id))
        .unwrap_or(false);
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

    clear_channel_registry().await;

    let mut rng = StdRng::seed_from_u64(config.seed);
    let relay = MockRelay::new();
    let node_ids = (0..config.node_count)
        .map(|index| format!("node-{index:04}"))
        .collect::<Vec<_>>();
    let mut nodes = Vec::with_capacity(config.node_count);
    for node_id in &node_ids {
        nodes.push(make_node(relay.clone(), node_id.clone(), &config).await);
    }
    pump_mesh(&nodes, config.pump_steps_after_setup).await;

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
            subscribe_stream(&nodes_by_id, &mut subscriptions, &subscriber_id, &stream_id).await;
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
            subscribe_stream(&nodes_by_id, &mut subscriptions, &subscriber_id, &stream_id).await;
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
}
