//! Mesh simulation using the shared routed mesh core.
//!
//! This module runs the same router and store core as the production mesh
//! wrapper, but swaps in mock signaling/data transports.

use crate::cashu_test_mint::{MintError, MintStats};
use crate::mint_client::{LocalMintClient, MintClient};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use hashtree_core::{HashTree, HashTreeConfig, MemoryStore, Store};
use hashtree_network::{
    MeshRouter, MeshRoutingConfig, MeshStoreCore, MockConnectionFactory, MockLatencyMode,
    MockRelay, MockRelayTransport, PoolConfig, PoolSettings, RequestDispatchConfig,
    ResponseBehaviorConfig, SelectionStrategy, SignalingTransport, SimMeshStore,
};

/// Simulation configuration
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Number of nodes to spawn
    pub node_count: usize,
    /// Total simulation duration
    pub duration: Duration,
    /// Random seed for reproducibility
    pub seed: u64,
    /// Peer pool configuration
    pub pool: PoolConfig,
    /// How often nodes check for new peers (ms)
    pub discovery_interval_ms: u64,
    /// How often nodes re-broadcast hello for discovery refresh (ms).
    pub hello_reannounce_interval_ms: u64,
    /// Churn rate: probability a node leaves per interval (0.0 - 1.0)
    pub churn_rate: f64,
    /// Whether departed nodes can rejoin
    pub allow_rejoin: bool,
    /// Mean network latency per hop (ms)
    pub network_latency_ms: u64,
    /// Number of retrieval probes to run after topology formation.
    pub retrieval_probe_count: usize,
    /// Payload size for each retrieval probe.
    pub retrieval_payload_bytes: usize,
    /// Timeout for each retrieval probe (ms).
    pub retrieval_timeout_ms: u64,
    /// Maximum number of simulation events retained in memory.
    pub max_events_retained: usize,
    /// Retrieval timeout mode. Virtual steps makes simulation independent of wall-clock sleeps.
    pub retrieval_timing_mode: RetrievalTimingMode,
    /// Simulated poll interval (ms) used when `retrieval_timing_mode` is `VirtualSteps`.
    pub retrieval_poll_interval_ms: u64,
    /// Optional per-node strategy mix (if empty, `pool` is used for all nodes).
    pub strategy_mix: Vec<NodeStrategyProfile>,
    /// Strategy name to track as reference in reports.
    pub reference_strategy: Option<String>,
    /// Optional test Cashu incentives (local mint + payment-priority boost).
    pub cashu_incentives: Option<CashuIncentiveConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalTimingMode {
    /// Use wall clock (`tokio::time::timeout`) like production paths.
    WallClock,
    /// Use simulated step budget derived from timeout/poll interval (faster, deterministic).
    VirtualSteps,
}

#[derive(Debug, Clone, Copy)]
pub struct CashuIncentiveConfig {
    /// Whether payment incentives are enabled.
    pub enabled: bool,
    /// Initial channel capacity per payer->payee pair (sat).
    pub channel_capacity_sat: u64,
    /// Amount paid after each successful retrieval probe (sat).
    pub payment_per_probe_sat: u64,
    /// Blend weight for payment priority in selector ranking.
    pub selection_bonus_weight: f64,
    /// Refuse future service after this many unpaid successful deliveries.
    pub payment_default_block_threshold: u64,
}

impl Default for CashuIncentiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_capacity_sat: 0,
            payment_per_probe_sat: 0,
            selection_bonus_weight: 0.0,
            payment_default_block_threshold: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeStrategyProfile {
    pub name: String,
    pub weight: u32,
    pub pool: PoolConfig,
    pub selection_strategy: SelectionStrategy,
    pub fairness_enabled: bool,
    pub dispatch: RequestDispatchConfig,
    pub response_behavior: ResponseBehaviorConfig,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            node_count: 100,
            duration: Duration::from_secs(60),
            seed: 42,
            pool: PoolConfig::default(),
            discovery_interval_ms: 500,
            hello_reannounce_interval_ms: 1000,
            churn_rate: 0.01,
            allow_rejoin: true,
            network_latency_ms: 50,
            retrieval_probe_count: 0,
            retrieval_payload_bytes: 1024,
            retrieval_timeout_ms: 1500,
            max_events_retained: 20_000,
            retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
            retrieval_poll_interval_ms: 5,
            strategy_mix: Vec::new(),
            reference_strategy: None,
            cashu_incentives: None,
        }
    }
}

/// Simulation event for logging/analysis
#[derive(Debug, Clone)]
pub enum SimEvent {
    NodeJoined {
        node_id: String,
        time_ms: u64,
    },
    NodeLeft {
        node_id: String,
        time_ms: u64,
    },
    ConnectionFormed {
        from: String,
        to: String,
        time_ms: u64,
    },
    ConnectionLost {
        from: String,
        to: String,
        time_ms: u64,
    },
}

/// A running node in the simulation
struct RunningNode {
    /// HashTree for content operations (stored for future use)
    #[allow(dead_code)]
    tree: Arc<HashTree<SimMeshStore<MemoryStore>>>,
    /// The underlying store for P2P operations
    store: Arc<SimMeshStore<MemoryStore>>,
    /// Relay transport for this node
    transport: Arc<MockRelayTransport>,
    /// Strategy profile used by this node
    strategy: String,
    #[allow(dead_code)]
    joined_at_ms: u64,
}

struct RetrievalAttempt {
    data: Option<Vec<u8>>,
    latency_ms: u64,
}

/// Topology analysis results
#[derive(Debug, Clone)]
pub struct TopologyStats {
    pub node_count: usize,
    pub connection_count: usize,
    pub avg_degree: f64,
    pub min_degree: usize,
    pub max_degree: usize,
    pub isolated_nodes: usize,
    pub is_connected: bool,
    pub component_count: usize,
    pub largest_component: usize,
    pub clustering_coefficient: f64,
    pub degree_distribution: HashMap<usize, usize>,
}

/// Simulation statistics over time
#[derive(Debug, Clone, Default)]
pub struct SimStats {
    pub total_joins: usize,
    pub total_leaves: usize,
    pub total_connections_formed: usize,
    pub total_connections_lost: usize,
    pub data_messages_processed: usize,
    pub data_request_messages: usize,
    pub data_response_messages: usize,
    pub data_bytes_processed: u64,
    pub retrieval: RetrievalStats,
    pub strategy_joins: HashMap<String, usize>,
    pub strategy_retrieval: HashMap<String, RetrievalStats>,
    pub local_resources: LocalResourceStats,
    pub cashu: CashuStats,
    pub topology_snapshots: Vec<(u64, TopologyStats)>,
    pub events: VecDeque<SimEvent>,
}

#[derive(Debug, Clone, Default)]
pub struct CashuStats {
    pub channels_opened: u64,
    pub payments_sent: u64,
    pub payments_failed: u64,
    pub volume_sat: u64,
    pub settlements_finalized: u64,
    pub priority_credits_applied: u64,
    pub priority_volume_sat: u64,
    pub payment_defaults_recorded: u64,
    pub quote_requests_sent: u64,
    pub quote_responses_received: u64,
    pub quoted_retrieval_attempts: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LocalResourceStats {
    pub peak_active_nodes: usize,
    pub peak_connection_pairs: usize,
    pub peak_event_log_entries: usize,
    pub run_wall_ms: u64,
    pub tick_p50_us: u64,
    pub tick_p95_us: u64,
    pub tick_max_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RetrievalStats {
    pub probes: usize,
    pub successes: usize,
    pub failures: usize,
    pub payload_bytes: u64,
    pub data_plane_bytes: u64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub max_latency_ms: u64,
    pub success_rate: f64,
}

impl RetrievalStats {
    fn percentile(sorted_latencies: &[u64], percentile: f64) -> u64 {
        if sorted_latencies.is_empty() {
            return 0;
        }
        let p = percentile.clamp(0.0, 1.0);
        let idx = ((sorted_latencies.len() as f64 - 1.0) * p).round() as usize;
        sorted_latencies[idx]
    }

    fn finalize(&mut self, latencies_ms: &[u64]) {
        let mut sorted = latencies_ms.to_vec();
        sorted.sort_unstable();
        self.p50_latency_ms = Self::percentile(&sorted, 0.50);
        self.p95_latency_ms = Self::percentile(&sorted, 0.95);
        self.max_latency_ms = sorted.last().copied().unwrap_or(0);
        self.success_rate = if self.probes == 0 {
            0.0
        } else {
            self.successes as f64 / self.probes as f64
        };
    }
}

impl SimStats {
    fn record_event(&mut self, max_events_retained: usize, event: SimEvent) {
        if max_events_retained == 0 {
            return;
        }
        if self.events.len() >= max_events_retained {
            let _ = self.events.pop_front();
        }
        self.events.push_back(event);
    }
}

impl LocalResourceStats {
    fn percentile(sorted_samples: &[u64], percentile: f64) -> u64 {
        if sorted_samples.is_empty() {
            return 0;
        }
        let p = percentile.clamp(0.0, 1.0);
        let idx = ((sorted_samples.len() as f64 - 1.0) * p).round() as usize;
        sorted_samples[idx]
    }

    fn finalize_tick_samples(&mut self, tick_durations_us: &[u64]) {
        if tick_durations_us.is_empty() {
            return;
        }
        let mut sorted = tick_durations_us.to_vec();
        sorted.sort_unstable();
        self.tick_p50_us = Self::percentile(&sorted, 0.50);
        self.tick_p95_us = Self::percentile(&sorted, 0.95);
        self.tick_max_us = sorted.last().copied().unwrap_or(0);
    }
}

/// Network simulation using `MeshStoreCore` with mock transports.
///
/// Uses the exact same routed core as production `MeshStore`, just with mocks.
pub struct Simulation {
    config: SimConfig,
    strategy_mix: Vec<NodeStrategyProfile>,
    relay: Arc<MockRelay>,
    nodes: RwLock<HashMap<String, RunningNode>>,
    known_connections: RwLock<HashSet<(String, String)>>,
    rng: RwLock<StdRng>,
    stats: RwLock<SimStats>,
    next_node_id: RwLock<usize>,
    cashu_mint: Option<Arc<dyn MintClient>>,
}

struct VirtualTimeGuard {
    paused: bool,
}

impl VirtualTimeGuard {
    fn maybe_pause(mode: RetrievalTimingMode) -> Self {
        if mode != RetrievalTimingMode::VirtualSteps {
            return Self { paused: false };
        }

        if std::panic::catch_unwind(tokio::time::pause).is_err() {
            panic!(
                "hashtree-sim VirtualSteps requires Tokio time to pause successfully; \
                 real-time sleep in the simulation path is forbidden"
            );
        }

        Self { paused: true }
    }

    fn is_paused(&self) -> bool {
        self.paused
    }
}

impl Drop for VirtualTimeGuard {
    fn drop(&mut self) {
        if self.paused {
            tokio::time::resume();
        }
    }
}

impl Simulation {
    fn virtual_sleep_divisor(&self) -> u64 {
        self.config.retrieval_poll_interval_ms.max(1)
    }

    fn virtual_scaled_latency_ms(&self) -> u64 {
        if self.config.network_latency_ms == 0 {
            return 0;
        }
        let divisor = self.virtual_sleep_divisor();
        (self.config.network_latency_ms / divisor).max(1)
    }

    async fn wait_for_virtual_step(&self, virtual_time_paused: bool, step_delay: Duration) {
        if step_delay.is_zero() {
            tokio::task::yield_now().await;
            return;
        }

        assert!(
            virtual_time_paused,
            "VirtualSteps must use paused Tokio time instead of real-time sleeping"
        );
        tokio::time::advance(step_delay).await;
        tokio::task::yield_now().await;
    }

    pub fn new(config: SimConfig) -> Self {
        let cashu_mint = config
            .cashu_incentives
            .as_ref()
            .filter(|c| c.enabled)
            .map(|_| Arc::new(LocalMintClient::new()) as Arc<dyn MintClient>);
        Self::new_with_optional_mint_client(config, cashu_mint)
    }

    pub fn new_with_mint_client(config: SimConfig, cashu_mint: Arc<dyn MintClient>) -> Self {
        let cashu_mint = config
            .cashu_incentives
            .as_ref()
            .filter(|c| c.enabled)
            .map(|_| cashu_mint);
        Self::new_with_optional_mint_client(config, cashu_mint)
    }

    fn new_with_optional_mint_client(
        config: SimConfig,
        cashu_mint: Option<Arc<dyn MintClient>>,
    ) -> Self {
        let strategy_mix = if config.strategy_mix.is_empty() {
            vec![NodeStrategyProfile {
                name: "default".to_string(),
                weight: 1,
                pool: config.pool.clone(),
                selection_strategy: SelectionStrategy::Weighted,
                fairness_enabled: true,
                dispatch: RequestDispatchConfig::default(),
                response_behavior: ResponseBehaviorConfig::default(),
            }]
        } else {
            let sanitized: Vec<NodeStrategyProfile> = config
                .strategy_mix
                .iter()
                .filter(|s| s.weight > 0 && !s.name.is_empty())
                .cloned()
                .collect();
            if sanitized.is_empty() {
                vec![NodeStrategyProfile {
                    name: "default".to_string(),
                    weight: 1,
                    pool: config.pool.clone(),
                    selection_strategy: SelectionStrategy::Weighted,
                    fairness_enabled: true,
                    dispatch: RequestDispatchConfig::default(),
                    response_behavior: ResponseBehaviorConfig::default(),
                }]
            } else {
                sanitized
            }
        };

        Self {
            rng: RwLock::new(StdRng::seed_from_u64(config.seed)),
            relay: MockRelay::new(),
            nodes: RwLock::new(HashMap::new()),
            known_connections: RwLock::new(HashSet::new()),
            stats: RwLock::new(SimStats::default()),
            next_node_id: RwLock::new(0),
            strategy_mix,
            cashu_mint,
            config,
        }
    }

    /// Run the simulation
    pub async fn run(&self) {
        // Mock negotiated channels share a global registry; each simulation run must
        // start from a clean slate or later runs inherit stale links.
        hashtree_network::clear_channel_registry().await;
        let virtual_time = VirtualTimeGuard::maybe_pause(self.config.retrieval_timing_mode);
        let run_started = Instant::now();
        let total_ms = self.config.duration.as_millis() as u64;
        let tick_ms = self.config.discovery_interval_ms;
        let total_ticks = total_ms / tick_ms;
        let mut tick_durations_us = Vec::with_capacity(total_ticks as usize + 10);

        // Generate initial spawn times for all nodes
        let spawn_ticks: Vec<u64> = {
            let mut rng = self.rng.write().await;
            (0..self.config.node_count)
                .map(|_| rng.gen_range(0..total_ticks))
                .collect()
        };

        let mut spawn_schedule: Vec<(u64, usize)> = spawn_ticks
            .into_iter()
            .enumerate()
            .map(|(i, t)| (t, i))
            .collect();
        spawn_schedule.sort_by_key(|(t, _)| *t);

        let mut next_spawn_idx = 0;
        let snapshot_interval_ticks = 5000 / tick_ms;
        let hello_interval_ticks = self
            .config
            .hello_reannounce_interval_ms
            .max(tick_ms)
            .div_ceil(tick_ms);

        // Tick-based simulation loop
        for tick in 0..total_ticks {
            let tick_started = Instant::now();
            let elapsed_ms = tick * tick_ms;

            // Spawn nodes scheduled for this tick
            while next_spawn_idx < spawn_schedule.len() && spawn_schedule[next_spawn_idx].0 <= tick
            {
                self.spawn_node(elapsed_ms).await;
                next_spawn_idx += 1;
            }

            if hello_interval_ticks > 0 && tick % hello_interval_ticks == 0 {
                self.broadcast_hellos().await;
            }

            // Process messages, apply churn
            self.process_all_messages(elapsed_ms).await;
            self.apply_churn(elapsed_ms).await;

            // Periodic topology snapshot
            if tick > 0 && tick % snapshot_interval_ticks == 0 {
                let stats = self.analyze_topology().await;
                self.stats
                    .write()
                    .await
                    .topology_snapshots
                    .push((elapsed_ms, stats));
            }

            self.update_resource_peaks().await;
            tick_durations_us.push(tick_started.elapsed().as_micros() as u64);
        }

        // Final processing
        for _ in 0..10 {
            self.process_all_messages(total_ms).await;
        }

        self.run_retrieval_probes(total_ms, virtual_time.is_paused())
            .await;
        self.finalize_cashu_stats().await;
        self.update_resource_peaks().await;

        let final_stats = self.analyze_topology().await;
        let mut stats = self.stats.write().await;
        stats.topology_snapshots.push((total_ms, final_stats));
        stats.local_resources.run_wall_ms = run_started.elapsed().as_millis() as u64;
        stats
            .local_resources
            .finalize_tick_samples(&tick_durations_us);
        drop(stats);
        hashtree_network::clear_channel_registry().await;
    }

    async fn update_resource_peaks(&self) {
        let active_nodes = self.nodes.read().await.len();
        let connection_pairs = self.known_connections.read().await.len();
        let mut stats = self.stats.write().await;
        stats.local_resources.peak_active_nodes =
            stats.local_resources.peak_active_nodes.max(active_nodes);
        stats.local_resources.peak_connection_pairs = stats
            .local_resources
            .peak_connection_pairs
            .max(connection_pairs);
        stats.local_resources.peak_event_log_entries = stats
            .local_resources
            .peak_event_log_entries
            .max(stats.events.len());
    }

    async fn spawn_node(&self, time_ms: u64) {
        let node_id = {
            let mut id = self.next_node_id.write().await;
            let current = *id;
            *id += 1;
            current.to_string()
        };

        // Create transport connected to shared relay
        let transport = Arc::new(self.relay.create_transport(node_id.clone()));

        // Virtual timing should avoid wall-clock link sleeps. Ordering still comes from
        // the stepped processing loop and retrieval poll cadence.
        let (latency_ms, latency_mode) = match self.config.retrieval_timing_mode {
            RetrievalTimingMode::WallClock => {
                (self.config.network_latency_ms, MockLatencyMode::RealSleep)
            }
            RetrievalTimingMode::VirtualSteps => {
                (self.virtual_scaled_latency_ms(), MockLatencyMode::YieldOnly)
            }
        };
        let conn_factory = Arc::new(MockConnectionFactory::new_with_latency_mode(
            node_id.clone(),
            latency_ms,
            latency_mode,
        ));

        let selected_strategy = {
            let total_weight: u32 = self.strategy_mix.iter().map(|s| s.weight).sum();
            let mut rng = self.rng.write().await;
            let mut pick = rng.gen_range(0..total_weight);
            let mut chosen = self
                .strategy_mix
                .last()
                .expect("strategy mix must not be empty")
                .clone();
            for strategy in &self.strategy_mix {
                if pick < strategy.weight {
                    chosen = strategy.clone();
                    break;
                }
                pick -= strategy.weight;
            }
            chosen
        };

        // Create pool settings (simulation only uses "other" pool)
        let pools = PoolSettings {
            follows: PoolConfig {
                max_connections: 0,
                satisfied_connections: 0,
            },
            other: selected_strategy.pool.clone(),
        };

        // Create mesh router
        let signaling = Arc::new(MeshRouter::new(
            node_id.clone(),
            transport.clone(),
            conn_factory,
            pools,
            false, // debug
        ));

        // Create local storage
        let local_store = Arc::new(MemoryStore::new());

        // Create the shared routed mesh store core.
        let store = Arc::new(MeshStoreCore::new_with_routing(
            local_store,
            signaling,
            Duration::from_secs(1),
            false,
            MeshRoutingConfig {
                selection_strategy: selected_strategy.selection_strategy,
                fairness_enabled: selected_strategy.fairness_enabled,
                cashu_payment_weight: self
                    .config
                    .cashu_incentives
                    .as_ref()
                    .map(|c| c.selection_bonus_weight)
                    .unwrap_or(0.0),
                cashu_payment_default_block_threshold: self
                    .config
                    .cashu_incentives
                    .as_ref()
                    .map(|c| c.payment_default_block_threshold)
                    .unwrap_or(0),
                cashu_accepted_mints: Vec::new(),
                cashu_default_mint: None,
                cashu_peer_suggested_mint_base_cap_sat: 0,
                cashu_peer_suggested_mint_success_step_sat: 0,
                cashu_peer_suggested_mint_receipt_step_sat: 0,
                cashu_peer_suggested_mint_max_cap_sat: 0,
                dispatch: selected_strategy.dispatch,
                response_behavior: selected_strategy.response_behavior,
            },
        ));

        // Connect transport and start
        transport.connect(&[]).await.ok();
        store.start().await.ok();

        // Wrap in HashTree
        let tree_config = HashTreeConfig::new(store.clone());
        let tree = Arc::new(HashTree::new(tree_config));

        // Record event
        {
            let mut stats = self.stats.write().await;
            stats.total_joins += 1;
            *stats
                .strategy_joins
                .entry(selected_strategy.name.clone())
                .or_insert(0) += 1;
            stats.record_event(
                self.config.max_events_retained,
                SimEvent::NodeJoined {
                    node_id: node_id.clone(),
                    time_ms,
                },
            );
        }

        self.nodes.write().await.insert(
            node_id,
            RunningNode {
                tree,
                store,
                transport,
                strategy: selected_strategy.name,
                joined_at_ms: time_ms,
            },
        );
    }

    async fn process_all_messages(&self, time_ms: u64) {
        // Sort node IDs for deterministic order
        let mut node_ids: Vec<String> = self.nodes.read().await.keys().cloned().collect();
        node_ids.sort();

        // Process incoming signaling messages for each node
        for node_id in &node_ids {
            let nodes = self.nodes.read().await;
            if let Some(running) = nodes.get(node_id) {
                // Process all pending messages
                while let Some(msg) = running.transport.try_recv() {
                    running.store.process_signaling(msg).await.ok();
                }
            }
        }

        self.process_all_data_messages().await;
        self.record_new_connections(time_ms).await;
    }

    async fn broadcast_hellos(&self) {
        let stores: Vec<Arc<SimMeshStore<MemoryStore>>> = {
            let nodes = self.nodes.read().await;
            nodes
                .values()
                .map(|running| running.store.clone())
                .collect()
        };
        for store in stores {
            let _ = store.send_hello().await;
        }
    }

    fn canonical_connection_pair(a: &str, b: &str) -> Option<(String, String)> {
        if a == b {
            return None;
        }
        if a < b {
            Some((a.to_string(), b.to_string()))
        } else {
            Some((b.to_string(), a.to_string()))
        }
    }

    async fn collect_active_connections(&self) -> HashSet<(String, String)> {
        let nodes = self.nodes.read().await;
        let active_ids: HashSet<String> = nodes.keys().cloned().collect();
        let mut connections = HashSet::new();

        for (node_id, running) in nodes.iter() {
            let peers = running.store.signaling().peer_ids().await;
            for peer_id in peers {
                if !active_ids.contains(&peer_id) {
                    continue;
                }
                if let Some(pair) = Self::canonical_connection_pair(node_id, &peer_id) {
                    connections.insert(pair);
                }
            }
        }

        connections
    }

    async fn record_new_connections(&self, time_ms: u64) {
        let current = self.collect_active_connections().await;
        let formed: Vec<(String, String)> = {
            let known = self.known_connections.read().await;
            current.difference(&*known).cloned().collect()
        };

        if !formed.is_empty() {
            let mut stats = self.stats.write().await;
            for (from, to) in &formed {
                stats.total_connections_formed += 1;
                stats.record_event(
                    self.config.max_events_retained,
                    SimEvent::ConnectionFormed {
                        from: from.clone(),
                        to: to.clone(),
                        time_ms,
                    },
                );
            }
        }

        *self.known_connections.write().await = current;
    }

    async fn process_all_data_messages(&self) {
        let mut node_ids: Vec<String> = self.nodes.read().await.keys().cloned().collect();
        node_ids.sort();

        let mut aggregate = hashtree_network::DataPumpStats::default();

        for node_id in node_ids {
            let store = {
                let nodes = self.nodes.read().await;
                nodes.get(&node_id).map(|running| running.store.clone())
            };
            let Some(store) = store else {
                continue;
            };

            let stats = store.drain_available_data_messages().await;
            aggregate.processed += stats.processed;
            aggregate.request_messages += stats.request_messages;
            aggregate.response_messages += stats.response_messages;
            aggregate.quote_request_messages += stats.quote_request_messages;
            aggregate.quote_response_messages += stats.quote_response_messages;
            aggregate.processed_bytes += stats.processed_bytes;
        }

        if aggregate.processed > 0 {
            let mut stats = self.stats.write().await;
            stats.data_messages_processed += aggregate.processed;
            stats.data_request_messages += aggregate.request_messages;
            stats.data_response_messages += aggregate.response_messages;
            stats.data_bytes_processed += aggregate.processed_bytes;
            stats.cashu.quote_requests_sent += aggregate.quote_request_messages;
            stats.cashu.quote_responses_received += aggregate.quote_response_messages;
        }
    }

    fn active_cashu_config(&self) -> Option<CashuIncentiveConfig> {
        self.config
            .cashu_incentives
            .filter(|c| c.enabled && c.channel_capacity_sat > 0 && c.payment_per_probe_sat > 0)
    }

    async fn settle_cashu_delivery_payment(
        &self,
        payer_id: &str,
        payee_id: &str,
        payer_store: Arc<SimMeshStore<MemoryStore>>,
        payee_store: Arc<SimMeshStore<MemoryStore>>,
    ) {
        if payer_id == payee_id {
            return;
        }
        let Some(config) = self.active_cashu_config() else {
            return;
        };
        let Some(mint) = &self.cashu_mint else {
            return;
        };

        match mint
            .open_channel(payer_id, payee_id, config.channel_capacity_sat)
            .await
        {
            Ok(()) | Err(MintError::ChannelAlreadyExists) => {}
            Err(_) => return,
        }

        if mint
            .transfer(payer_id, payee_id, config.payment_per_probe_sat)
            .await
            .is_err()
        {
            payee_store
                .record_cashu_payment_default_from_peer(payer_id)
                .await;
            let mut stats = self.stats.write().await;
            stats.cashu.payment_defaults_recorded += 1;
            return;
        }

        payer_store
            .record_cashu_payment_for_peer(payee_id, config.payment_per_probe_sat)
            .await;
        payee_store
            .record_cashu_receipt_from_peer(payer_id, config.payment_per_probe_sat)
            .await;
        let mut stats = self.stats.write().await;
        stats.cashu.priority_credits_applied += 1;
        stats.cashu.priority_volume_sat += config.payment_per_probe_sat;
    }

    async fn finalize_cashu_stats(&self) {
        let Some(mint) = &self.cashu_mint else {
            return;
        };
        let _ = mint.settle_all().await;
        let MintStats {
            channels_opened,
            payments_sent,
            payments_failed,
            volume_sat,
            settlements_finalized,
        } = mint.stats().await.unwrap_or_default();

        let mut stats = self.stats.write().await;
        stats.cashu.channels_opened = channels_opened;
        stats.cashu.payments_sent = payments_sent;
        stats.cashu.payments_failed = payments_failed;
        stats.cashu.volume_sat = volume_sat;
        stats.cashu.settlements_finalized = settlements_finalized;
    }

    async fn run_retrieval_probes(&self, time_ms: u64, virtual_time_paused: bool) {
        if self.config.retrieval_probe_count == 0 {
            return;
        }

        let connected_pairs: Vec<(String, String)> = self
            .collect_active_connections()
            .await
            .into_iter()
            .collect();
        if connected_pairs.is_empty() {
            return;
        }

        let mut latencies_ms = Vec::with_capacity(self.config.retrieval_probe_count);
        let mut strategy_latencies: HashMap<String, Vec<u64>> = HashMap::new();
        for probe_idx in 0..self.config.retrieval_probe_count {
            let (source_id, target_id, payload) = {
                let mut rng = self.rng.write().await;
                let pair_idx = rng.gen_range(0..connected_pairs.len());
                let (a, b) = connected_pairs[pair_idx].clone();
                let use_ab = rng.gen::<bool>();
                let (source, target) = if use_ab { (a, b) } else { (b, a) };
                let payload = (0..self.config.retrieval_payload_bytes)
                    .map(|_| rng.gen::<u8>())
                    .collect::<Vec<u8>>();
                (source, target, payload)
            };

            let source_store = {
                let nodes = self.nodes.read().await;
                nodes
                    .get(&source_id)
                    .map(|running| running.store.clone())
                    .expect("source node must exist")
            };
            let (target_store, target_strategy) = {
                let nodes = self.nodes.read().await;
                let target = nodes.get(&target_id).expect("target node must exist");
                (target.store.clone(), target.strategy.clone())
            };

            let hash = hashtree_core::sha256(&payload);
            let _ = source_store.put(hash, payload.clone()).await;

            let bytes_before = self.stats.read().await.data_bytes_processed;
            let attempt = self
                .retrieve_with_processing(
                    target_store.clone(),
                    hash,
                    Duration::from_millis(self.config.retrieval_timeout_ms),
                    time_ms + probe_idx as u64,
                    virtual_time_paused,
                )
                .await;
            let latency_ms = attempt.latency_ms;
            let result = attempt.data;
            let bytes_after = self.stats.read().await.data_bytes_processed;
            let success = matches!(result.as_ref(), Some(data) if data == &payload);
            let transfer_bytes = bytes_after.saturating_sub(bytes_before);

            let mut stats = self.stats.write().await;
            stats.retrieval.probes += 1;
            stats.retrieval.data_plane_bytes += transfer_bytes;

            if success {
                stats.retrieval.successes += 1;
                stats.retrieval.payload_bytes += payload.len() as u64;
                latencies_ms.push(latency_ms);
            } else {
                stats.retrieval.failures += 1;
            }

            {
                let strategy_stats = stats
                    .strategy_retrieval
                    .entry(target_strategy.clone())
                    .or_default();
                strategy_stats.probes += 1;
                strategy_stats.data_plane_bytes += transfer_bytes;
                if success {
                    strategy_stats.successes += 1;
                    strategy_stats.payload_bytes += payload.len() as u64;
                    strategy_latencies
                        .entry(target_strategy)
                        .or_default()
                        .push(latency_ms);
                } else {
                    strategy_stats.failures += 1;
                }
            }

            drop(stats);
            if success {
                self.settle_cashu_delivery_payment(
                    &target_id,
                    &source_id,
                    target_store.clone(),
                    source_store.clone(),
                )
                .await;
            }
        }

        let mut stats = self.stats.write().await;
        stats.retrieval.finalize(&latencies_ms);
        for (strategy, latencies) in strategy_latencies {
            if let Some(strategy_stats) = stats.strategy_retrieval.get_mut(&strategy) {
                strategy_stats.finalize(&latencies);
            }
        }
        for strategy_stats in stats.strategy_retrieval.values_mut() {
            if strategy_stats.probes > 0 && strategy_stats.success_rate == 0.0 {
                strategy_stats.finalize(&[]);
            }
        }
    }

    async fn retrieve_with_processing(
        &self,
        store: Arc<SimMeshStore<MemoryStore>>,
        hash: hashtree_core::Hash,
        timeout: Duration,
        time_ms: u64,
        virtual_time_paused: bool,
    ) -> RetrievalAttempt {
        let quote_terms = self.active_cashu_config().map(|cashu| {
            (
                cashu.payment_per_probe_sat,
                Duration::from_millis(timeout.as_millis().max(1) as u64),
            )
        });
        if quote_terms.is_some() {
            self.stats.write().await.cashu.quoted_retrieval_attempts += 1;
        }
        let get_task = tokio::spawn(async move {
            if let Some((payment_sat, quote_ttl)) = quote_terms {
                store
                    .get_with_quote(&hash, payment_sat, quote_ttl)
                    .await
                    .ok()
                    .flatten()
            } else {
                store.get(&hash).await.ok().flatten()
            }
        });
        match self.config.retrieval_timing_mode {
            RetrievalTimingMode::WallClock => {
                let started = Instant::now();
                loop {
                    if get_task.is_finished() {
                        return RetrievalAttempt {
                            data: get_task.await.ok().flatten(),
                            latency_ms: started.elapsed().as_millis() as u64,
                        };
                    }

                    if started.elapsed() >= timeout {
                        get_task.abort();
                        return RetrievalAttempt {
                            data: None,
                            latency_ms: timeout.as_millis() as u64,
                        };
                    }

                    self.process_all_messages(time_ms + started.elapsed().as_millis() as u64)
                        .await;
                    tokio::task::yield_now().await;
                }
            }
            RetrievalTimingMode::VirtualSteps => {
                let poll_interval_ms = self.config.retrieval_poll_interval_ms.max(1);
                let timeout_ms = timeout.as_millis() as u64;
                let max_polls = ((timeout_ms + poll_interval_ms - 1) / poll_interval_ms).max(1);
                let step_sleep_ms = self.virtual_scaled_latency_ms();

                for poll in 0..max_polls {
                    if get_task.is_finished() {
                        return RetrievalAttempt {
                            data: get_task.await.ok().flatten(),
                            latency_ms: poll.saturating_mul(poll_interval_ms).min(timeout_ms),
                        };
                    }

                    let simulated_now =
                        time_ms.saturating_add(poll.saturating_mul(poll_interval_ms));
                    self.process_all_messages(simulated_now).await;
                    self.wait_for_virtual_step(
                        virtual_time_paused,
                        Duration::from_millis(step_sleep_ms),
                    )
                    .await;

                    if get_task.is_finished() {
                        return RetrievalAttempt {
                            data: get_task.await.ok().flatten(),
                            latency_ms: (poll.saturating_add(1))
                                .saturating_mul(poll_interval_ms)
                                .min(timeout_ms),
                        };
                    }
                }

                if get_task.is_finished() {
                    return RetrievalAttempt {
                        data: get_task.await.ok().flatten(),
                        latency_ms: timeout_ms,
                    };
                }

                get_task.abort();
                RetrievalAttempt {
                    data: None,
                    latency_ms: timeout_ms,
                }
            }
        }
    }

    async fn apply_churn(&self, time_ms: u64) {
        if self.config.churn_rate <= 0.0 {
            return;
        }

        let node_ids: Vec<String> = self.nodes.read().await.keys().cloned().collect();
        let mut to_remove = Vec::new();

        {
            let mut rng = self.rng.write().await;
            for node_id in &node_ids {
                if rng.gen::<f64>() < self.config.churn_rate {
                    to_remove.push(node_id.clone());
                }
            }
        }

        for node_id in to_remove {
            self.remove_node(&node_id, time_ms).await;
            if self.config.allow_rejoin {
                self.spawn_node(time_ms).await;
            }
        }
    }

    async fn remove_node(&self, node_id: &str, time_ms: u64) {
        let removed = self.nodes.write().await.remove(node_id);

        if let Some(running) = removed {
            running.store.stop().await;

            let peer_ids = running.store.signaling().peer_ids().await;
            let mut stats = self.stats.write().await;

            for peer_id in peer_ids {
                stats.total_connections_lost += 1;
                stats.record_event(
                    self.config.max_events_retained,
                    SimEvent::ConnectionLost {
                        from: node_id.to_string(),
                        to: peer_id.clone(),
                        time_ms,
                    },
                );
            }

            stats.total_leaves += 1;
            stats.record_event(
                self.config.max_events_retained,
                SimEvent::NodeLeft {
                    node_id: node_id.to_string(),
                    time_ms,
                },
            );
        }
    }

    /// Analyze the current network topology
    pub async fn analyze_topology(&self) -> TopologyStats {
        let nodes = self.nodes.read().await;

        // Build adjacency map
        let mut adjacency: HashMap<String, HashSet<String>> = HashMap::new();
        for (node_id, running) in nodes.iter() {
            let peers = running.store.signaling().peer_ids().await;
            let peer_set: HashSet<String> = peers.into_iter().collect();
            adjacency.insert(node_id.clone(), peer_set);
        }

        let node_count = adjacency.len();
        if node_count == 0 {
            return TopologyStats {
                node_count: 0,
                connection_count: 0,
                avg_degree: 0.0,
                min_degree: 0,
                max_degree: 0,
                isolated_nodes: 0,
                is_connected: true,
                component_count: 0,
                largest_component: 0,
                clustering_coefficient: 0.0,
                degree_distribution: HashMap::new(),
            };
        }

        // Filter to only active nodes
        let active_nodes: HashSet<String> = adjacency.keys().cloned().collect();
        let active_adjacency: HashMap<String, HashSet<String>> = adjacency
            .iter()
            .map(|(k, v)| {
                let active_peers: HashSet<String> = v
                    .iter()
                    .filter(|p| active_nodes.contains(*p))
                    .cloned()
                    .collect();
                (k.clone(), active_peers)
            })
            .collect();

        // Calculate degrees
        let degrees: Vec<usize> = active_adjacency.values().map(|peers| peers.len()).collect();
        let mut degree_distribution: HashMap<usize, usize> = HashMap::new();
        for &d in &degrees {
            *degree_distribution.entry(d).or_insert(0) += 1;
        }

        let total_degree: usize = degrees.iter().sum();
        let connection_count = total_degree / 2;
        let avg_degree = total_degree as f64 / node_count as f64;
        let min_degree = *degrees.iter().min().unwrap_or(&0);
        let max_degree = *degrees.iter().max().unwrap_or(&0);
        let isolated_nodes = degrees.iter().filter(|&&d| d == 0).count();

        // Find connected components
        let mut visited: HashSet<String> = HashSet::new();
        let mut components: Vec<usize> = Vec::new();

        for node_id in active_adjacency.keys() {
            if visited.contains(node_id) {
                continue;
            }

            let mut queue = vec![node_id.clone()];
            let mut component_size = 0;

            while let Some(current) = queue.pop() {
                if visited.contains(&current) {
                    continue;
                }
                visited.insert(current.clone());
                component_size += 1;

                if let Some(peers) = active_adjacency.get(&current) {
                    for peer in peers {
                        if !visited.contains(peer) {
                            queue.push(peer.clone());
                        }
                    }
                }
            }

            if component_size > 0 {
                components.push(component_size);
            }
        }

        let component_count = components.len();
        let largest_component = *components.iter().max().unwrap_or(&0);
        let is_connected = component_count <= 1;

        // Calculate clustering coefficient
        let mut total_clustering = 0.0;
        let mut nodes_with_neighbors = 0;

        for (_, peers) in &active_adjacency {
            let k = peers.len();
            if k < 2 {
                continue;
            }

            let mut neighbor_edges = 0;
            let peer_list: Vec<&String> = peers.iter().collect();
            for i in 0..peer_list.len() {
                for j in (i + 1)..peer_list.len() {
                    if let Some(peer_neighbors) = active_adjacency.get(peer_list[i]) {
                        if peer_neighbors.contains(peer_list[j]) {
                            neighbor_edges += 1;
                        }
                    }
                }
            }

            let max_edges = k * (k - 1) / 2;
            if max_edges > 0 {
                total_clustering += neighbor_edges as f64 / max_edges as f64;
                nodes_with_neighbors += 1;
            }
        }

        let clustering_coefficient = if nodes_with_neighbors > 0 {
            total_clustering / nodes_with_neighbors as f64
        } else {
            0.0
        };

        TopologyStats {
            node_count,
            connection_count,
            avg_degree,
            min_degree,
            max_degree,
            isolated_nodes,
            is_connected,
            component_count,
            largest_component,
            clustering_coefficient,
            degree_distribution,
        }
    }

    /// Get simulation statistics
    pub async fn get_stats(&self) -> SimStats {
        self.stats.read().await.clone()
    }

    /// Build a JSON report suitable for sweep comparison and regression tracking.
    pub async fn report_json(&self) -> serde_json::Value {
        let stats = self.get_stats().await;
        let (snapshot_time_ms, final_topology) =
            if let Some((ts, topo)) = stats.topology_snapshots.last().cloned() {
                (ts, topo)
            } else {
                (0, self.analyze_topology().await)
            };
        let strategy_retrieval_json: serde_json::Map<String, serde_json::Value> = stats
            .strategy_retrieval
            .iter()
            .map(|(strategy, retrieval)| {
                (
                    strategy.clone(),
                    serde_json::json!({
                        "probes": retrieval.probes,
                        "successes": retrieval.successes,
                        "failures": retrieval.failures,
                        "success_rate": retrieval.success_rate,
                        "payload_bytes": retrieval.payload_bytes,
                        "data_plane_bytes": retrieval.data_plane_bytes,
                        "p50_latency_ms": retrieval.p50_latency_ms,
                        "p95_latency_ms": retrieval.p95_latency_ms,
                        "max_latency_ms": retrieval.max_latency_ms
                    }),
                )
            })
            .collect();
        let reference_retrieval = self
            .config
            .reference_strategy
            .as_ref()
            .and_then(|name| stats.strategy_retrieval.get(name));
        let reference_success_rate = reference_retrieval
            .map(|r| r.success_rate)
            .unwrap_or(stats.retrieval.success_rate);
        let reference_p95_latency_ms = reference_retrieval
            .map(|r| r.p95_latency_ms)
            .unwrap_or(stats.retrieval.p95_latency_ms);
        let reference_failure_rate = reference_retrieval
            .map(|r| {
                if r.probes == 0 {
                    1.0
                } else {
                    r.failures as f64 / r.probes as f64
                }
            })
            .unwrap_or_else(|| 1.0 - stats.retrieval.success_rate);

        serde_json::json!({
            "config": {
                "node_count": self.config.node_count,
                "duration_ms": self.config.duration.as_millis() as u64,
                "seed": self.config.seed,
                "discovery_interval_ms": self.config.discovery_interval_ms,
                "hello_reannounce_interval_ms": self.config.hello_reannounce_interval_ms,
                "churn_rate": self.config.churn_rate,
                "allow_rejoin": self.config.allow_rejoin,
                "network_latency_ms": self.config.network_latency_ms,
                "retrieval_probe_count": self.config.retrieval_probe_count,
                "retrieval_payload_bytes": self.config.retrieval_payload_bytes,
                "retrieval_timeout_ms": self.config.retrieval_timeout_ms,
                "retrieval_timing_mode": match self.config.retrieval_timing_mode {
                    RetrievalTimingMode::WallClock => "wall_clock",
                    RetrievalTimingMode::VirtualSteps => "virtual_steps",
                },
                "retrieval_poll_interval_ms": self.config.retrieval_poll_interval_ms,
                "max_events_retained": self.config.max_events_retained,
                "reference_strategy": self.config.reference_strategy,
                "cashu_incentives": self.config.cashu_incentives.map(|cashu| {
                    serde_json::json!({
                        "enabled": cashu.enabled,
                        "channel_capacity_sat": cashu.channel_capacity_sat,
                        "payment_per_probe_sat": cashu.payment_per_probe_sat,
                        "selection_bonus_weight": cashu.selection_bonus_weight,
                        "payment_default_block_threshold": cashu.payment_default_block_threshold,
                    })
                }),
                "strategy_mix": self.strategy_mix.iter().map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "weight": s.weight,
                        "selection_strategy": format!("{:?}", s.selection_strategy),
                        "fairness_enabled": s.fairness_enabled,
                        "dispatch": {
                            "initial_fanout": s.dispatch.initial_fanout,
                            "hedge_fanout": s.dispatch.hedge_fanout,
                            "max_fanout": s.dispatch.max_fanout,
                            "hedge_interval_ms": s.dispatch.hedge_interval_ms,
                        },
                        "response_behavior": {
                            "drop_response_prob": s.response_behavior.drop_response_prob,
                            "corrupt_response_prob": s.response_behavior.corrupt_response_prob,
                            "extra_delay_ms": s.response_behavior.extra_delay_ms,
                            "first_byte_delay_ms": s.response_behavior.first_byte_delay_ms,
                            "bytes_per_second": s.response_behavior.bytes_per_second,
                            "stall_response_prob": s.response_behavior.stall_response_prob,
                            "stall_delay_ms": s.response_behavior.stall_delay_ms,
                        },
                        "pool": {
                            "max_connections": s.pool.max_connections,
                            "satisfied_connections": s.pool.satisfied_connections
                        }
                    })
                }).collect::<Vec<_>>(),
                "pool": {
                    "max_connections": self.config.pool.max_connections,
                    "satisfied_connections": self.config.pool.satisfied_connections,
                }
            },
            "stats": {
                "joins": stats.total_joins,
                "leaves": stats.total_leaves,
                "connections_formed": stats.total_connections_formed,
                "connections_lost": stats.total_connections_lost,
                "data_messages_processed": stats.data_messages_processed,
                "data_request_messages": stats.data_request_messages,
                "data_response_messages": stats.data_response_messages,
                "data_bytes_processed": stats.data_bytes_processed,
                "strategy_joins": stats.strategy_joins,
                "strategy_retrieval": strategy_retrieval_json,
                "local_resources": {
                    "peak_active_nodes": stats.local_resources.peak_active_nodes,
                    "peak_connection_pairs": stats.local_resources.peak_connection_pairs,
                    "peak_event_log_entries": stats.local_resources.peak_event_log_entries,
                    "run_wall_ms": stats.local_resources.run_wall_ms,
                    "tick_p50_us": stats.local_resources.tick_p50_us,
                    "tick_p95_us": stats.local_resources.tick_p95_us,
                    "tick_max_us": stats.local_resources.tick_max_us
                },
                "cashu": {
                    "channels_opened": stats.cashu.channels_opened,
                    "payments_sent": stats.cashu.payments_sent,
                    "payments_failed": stats.cashu.payments_failed,
                    "volume_sat": stats.cashu.volume_sat,
                    "settlements_finalized": stats.cashu.settlements_finalized,
                    "priority_credits_applied": stats.cashu.priority_credits_applied,
                    "priority_volume_sat": stats.cashu.priority_volume_sat,
                    "payment_defaults_recorded": stats.cashu.payment_defaults_recorded,
                    "quote_requests_sent": stats.cashu.quote_requests_sent,
                    "quote_responses_received": stats.cashu.quote_responses_received,
                    "quoted_retrieval_attempts": stats.cashu.quoted_retrieval_attempts
                },
                "retrieval": {
                    "probes": stats.retrieval.probes,
                    "successes": stats.retrieval.successes,
                    "failures": stats.retrieval.failures,
                    "success_rate": stats.retrieval.success_rate,
                    "payload_bytes": stats.retrieval.payload_bytes,
                    "data_plane_bytes": stats.retrieval.data_plane_bytes,
                    "p50_latency_ms": stats.retrieval.p50_latency_ms,
                    "p95_latency_ms": stats.retrieval.p95_latency_ms,
                    "max_latency_ms": stats.retrieval.max_latency_ms
                }
            },
            "topology": {
                "snapshot_time_ms": snapshot_time_ms,
                "node_count": final_topology.node_count,
                "connection_count": final_topology.connection_count,
                "avg_degree": final_topology.avg_degree,
                "min_degree": final_topology.min_degree,
                "max_degree": final_topology.max_degree,
                "isolated_nodes": final_topology.isolated_nodes,
                "is_connected": final_topology.is_connected,
                "component_count": final_topology.component_count,
                "largest_component": final_topology.largest_component,
                "clustering_coefficient": final_topology.clustering_coefficient
            },
            "objectives": {
                "retrieval_success_rate": stats.retrieval.success_rate,
                "retrieval_p95_latency_ms": stats.retrieval.p95_latency_ms,
                "overhead_ratio_data_to_payload": if stats.retrieval.payload_bytes == 0 {
                    0.0
                } else {
                    stats.retrieval.data_plane_bytes as f64 / stats.retrieval.payload_bytes as f64
                },
                "decentralization_component_count": final_topology.component_count,
                "decentralization_largest_component_share": if final_topology.node_count == 0 {
                    0.0
                } else {
                    final_topology.largest_component as f64 / final_topology.node_count as f64
                },
                "local_cpu_tick_p95_us": stats.local_resources.tick_p95_us,
                "local_cpu_run_wall_ms": stats.local_resources.run_wall_ms,
                "local_mem_peak_event_log_entries": stats.local_resources.peak_event_log_entries,
                "local_mem_peak_connection_pairs": stats.local_resources.peak_connection_pairs,
                "reference_success_rate": reference_success_rate,
                "reference_p95_latency_ms": reference_p95_latency_ms,
                "reference_failure_rate": reference_failure_rate,
                "cashu_priority_credits_applied": stats.cashu.priority_credits_applied,
                "cashu_priority_volume_sat": stats.cashu.priority_volume_sat,
                "cashu_payment_defaults_recorded": stats.cashu.payment_defaults_recorded,
                "cashu_quote_requests_sent": stats.cashu.quote_requests_sent,
                "cashu_quote_responses_received": stats.cashu.quote_responses_received,
                "cashu_quoted_retrieval_attempts": stats.cashu.quoted_retrieval_attempts
            }
        })
    }

    /// Get number of currently active nodes
    pub async fn active_node_count(&self) -> usize {
        self.nodes.read().await.len()
    }

    /// Print topology summary
    pub fn print_topology_stats(stats: &TopologyStats) {
        println!("=== Topology Analysis ===");
        println!("Nodes: {}", stats.node_count);
        println!("Connections: {}", stats.connection_count);
        println!("Avg degree: {:.2}", stats.avg_degree);
        println!(
            "Min/Max degree: {} / {}",
            stats.min_degree, stats.max_degree
        );
        println!("Isolated nodes: {}", stats.isolated_nodes);
        println!("Connected: {}", stats.is_connected);
        println!(
            "Components: {} (largest: {})",
            stats.component_count, stats.largest_component
        );
        println!(
            "Clustering coefficient: {:.4}",
            stats.clustering_coefficient
        );
        println!("Degree distribution: {:?}", stats.degree_distribution);
    }

    /// Print simulation summary
    pub fn print_sim_stats(stats: &SimStats) {
        println!("=== Simulation Stats ===");
        println!("Total joins: {}", stats.total_joins);
        println!("Total leaves: {}", stats.total_leaves);
        println!("Connections formed: {}", stats.total_connections_formed);
        println!("Connections lost: {}", stats.total_connections_lost);
        println!(
            "Data messages: {} (req: {}, res: {})",
            stats.data_messages_processed,
            stats.data_request_messages,
            stats.data_response_messages
        );
        println!("Data bytes: {}", stats.data_bytes_processed);
        println!(
            "Local resources: peak_nodes={}, peak_links={}, peak_events={}, tick p50/p95/max (us)={}/{}/{}, wall_ms={}",
            stats.local_resources.peak_active_nodes,
            stats.local_resources.peak_connection_pairs,
            stats.local_resources.peak_event_log_entries,
            stats.local_resources.tick_p50_us,
            stats.local_resources.tick_p95_us,
            stats.local_resources.tick_max_us,
            stats.local_resources.run_wall_ms
        );
        if stats.cashu.payments_sent > 0 || stats.cashu.channels_opened > 0 {
            println!(
                "Cashu incentives: channels={} payments_sent={} payments_failed={} volume_sat={} settlements={} priority_credits={} priority_volume_sat={} payment_defaults={} quote_requests={} quote_responses={} quoted_retrievals={}",
                stats.cashu.channels_opened,
                stats.cashu.payments_sent,
                stats.cashu.payments_failed,
                stats.cashu.volume_sat,
                stats.cashu.settlements_finalized,
                stats.cashu.priority_credits_applied,
                stats.cashu.priority_volume_sat,
                stats.cashu.payment_defaults_recorded,
                stats.cashu.quote_requests_sent,
                stats.cashu.quote_responses_received,
                stats.cashu.quoted_retrieval_attempts
            );
        }
        if !stats.strategy_retrieval.is_empty() {
            println!("Strategy retrieval:");
            let mut keys: Vec<_> = stats.strategy_retrieval.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if let Some(s) = stats.strategy_retrieval.get(&key) {
                    println!(
                        "  {} -> probes={} success_rate={:.2}% p95={}ms",
                        key,
                        s.probes,
                        s.success_rate * 100.0,
                        s.p95_latency_ms
                    );
                }
            }
        }
        if stats.retrieval.probes > 0 {
            println!(
                "Retrieval probes: {} (success: {}, failure: {}, success_rate: {:.2}%)",
                stats.retrieval.probes,
                stats.retrieval.successes,
                stats.retrieval.failures,
                stats.retrieval.success_rate * 100.0
            );
            println!(
                "Retrieval latency p50/p95/max (ms): {}/{}/{}",
                stats.retrieval.p50_latency_ms,
                stats.retrieval.p95_latency_ms,
                stats.retrieval.max_latency_ms
            );
        }
        println!("Topology snapshots: {}", stats.topology_snapshots.len());
    }
}

#[derive(Debug, Clone)]
pub struct SweepResult {
    pub config: SimConfig,
    pub stats: SimStats,
    pub final_topology: TopologyStats,
}

/// Run a deterministic parameter sweep and collect comparable results.
pub async fn run_parameter_sweep(configs: &[SimConfig]) -> Vec<SweepResult> {
    let mut results = Vec::with_capacity(configs.len());
    for config in configs {
        let sim = Simulation::new(config.clone());
        sim.run().await;
        results.push(SweepResult {
            config: config.clone(),
            stats: sim.get_stats().await,
            final_topology: sim.analyze_topology().await,
        });
    }
    results
}

#[cfg(test)]
mod tests;
