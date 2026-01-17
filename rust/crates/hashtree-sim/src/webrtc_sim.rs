//! WebRTC-based simulation using shared code with production
//!
//! This module uses the exact same signaling and data transfer code
//! as production WebRTCStore, just with mock transports.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;
use tokio::sync::RwLock;

use hashtree_core::{HashTree, HashTreeConfig, MemoryStore};
use hashtree_webrtc::{
    GenericStore, MockConnectionFactory, MockRelay, MockRelayTransport,
    PoolConfig, PoolSettings, RelayTransport, SignalingManager,
};

/// Simulation configuration
#[derive(Clone)]
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
    /// Churn rate: probability a node leaves per interval (0.0 - 1.0)
    pub churn_rate: f64,
    /// Whether departed nodes can rejoin
    pub allow_rejoin: bool,
    /// Mean network latency per hop (ms)
    pub network_latency_ms: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            node_count: 100,
            duration: Duration::from_secs(60),
            seed: 42,
            pool: PoolConfig::default(),
            discovery_interval_ms: 500,
            churn_rate: 0.01,
            allow_rejoin: true,
            network_latency_ms: 50,
        }
    }
}

/// Simulation event for logging/analysis
#[derive(Debug, Clone)]
pub enum SimEvent {
    NodeJoined { node_id: String, time_ms: u64 },
    NodeLeft { node_id: String, time_ms: u64 },
    ConnectionFormed { from: String, to: String, time_ms: u64 },
    ConnectionLost { from: String, to: String, time_ms: u64 },
}

/// Type alias for the store used in simulation
pub type SimStore = GenericStore<MemoryStore, MockRelayTransport, MockConnectionFactory>;

/// A running node in the simulation
struct RunningNode {
    /// HashTree for content operations (stored for future use)
    #[allow(dead_code)]
    tree: Arc<HashTree<SimStore>>,
    /// The underlying store for P2P operations
    store: Arc<SimStore>,
    /// Relay transport for this node
    transport: Arc<MockRelayTransport>,
    #[allow(dead_code)]
    joined_at_ms: u64,
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
    pub topology_snapshots: Vec<(u64, TopologyStats)>,
    pub events: Vec<SimEvent>,
}

/// Network simulation using GenericStore with mock transports
///
/// Uses the exact same code as production WebRTCStore, just with mocks.
pub struct Simulation {
    config: SimConfig,
    relay: Arc<MockRelay>,
    nodes: RwLock<HashMap<String, RunningNode>>,
    rng: RwLock<StdRng>,
    stats: RwLock<SimStats>,
    next_node_id: RwLock<usize>,
}

impl Simulation {
    pub fn new(config: SimConfig) -> Self {
        Self {
            rng: RwLock::new(StdRng::seed_from_u64(config.seed)),
            relay: MockRelay::new(),
            nodes: RwLock::new(HashMap::new()),
            stats: RwLock::new(SimStats::default()),
            next_node_id: RwLock::new(0),
            config,
        }
    }

    /// Run the simulation
    pub async fn run(&self) {
        let total_ms = self.config.duration.as_millis() as u64;
        let tick_ms = self.config.discovery_interval_ms;
        let total_ticks = total_ms / tick_ms;

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

        // Tick-based simulation loop
        for tick in 0..total_ticks {
            let elapsed_ms = tick * tick_ms;

            // Spawn nodes scheduled for this tick
            while next_spawn_idx < spawn_schedule.len() && spawn_schedule[next_spawn_idx].0 <= tick {
                self.spawn_node(elapsed_ms).await;
                next_spawn_idx += 1;
            }

            // Process messages, apply churn
            self.process_all_messages().await;
            self.apply_churn(elapsed_ms).await;

            // Periodic topology snapshot
            if tick > 0 && tick % snapshot_interval_ticks == 0 {
                let stats = self.analyze_topology().await;
                self.stats.write().await.topology_snapshots.push((elapsed_ms, stats));
            }
        }

        // Final processing
        for _ in 0..10 {
            self.process_all_messages().await;
        }

        let final_stats = self.analyze_topology().await;
        self.stats.write().await.topology_snapshots.push((total_ms, final_stats));
    }

    async fn spawn_node(&self, time_ms: u64) {
        let node_id = {
            let mut id = self.next_node_id.write().await;
            let current = *id;
            *id += 1;
            current.to_string()
        };

        // Create transport connected to shared relay
        let transport = Arc::new(self.relay.create_transport(node_id.clone(), node_id.clone()));

        // Create connection factory for this node
        let conn_factory = Arc::new(MockConnectionFactory::new(
            node_id.clone(),
            self.config.network_latency_ms,
        ));

        // Create pool settings (simulation only uses "other" pool)
        let pools = PoolSettings {
            follows: PoolConfig { max_connections: 0, satisfied_connections: 0 },
            other: self.config.pool.clone(),
        };

        // Create signaling manager
        let signaling = Arc::new(SignalingManager::new(
            node_id.clone(),
            node_id.clone(),
            transport.clone(),
            conn_factory,
            pools,
            false, // debug
        ));

        // Create local storage
        let local_store = Arc::new(MemoryStore::new());

        // Create GenericStore
        let store = Arc::new(GenericStore::new(
            local_store,
            signaling,
            Duration::from_secs(1),
            false,
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
            stats.events.push(SimEvent::NodeJoined {
                node_id: node_id.clone(),
                time_ms,
            });
        }

        self.nodes.write().await.insert(
            node_id,
            RunningNode {
                tree,
                store,
                transport,
                joined_at_ms: time_ms,
            },
        );
    }

    async fn process_all_messages(&self) {
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
                stats.events.push(SimEvent::ConnectionLost {
                    from: node_id.to_string(),
                    to: peer_id.clone(),
                    time_ms,
                });
            }

            stats.total_leaves += 1;
            stats.events.push(SimEvent::NodeLeft {
                node_id: node_id.to_string(),
                time_ms,
            });
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
                let active_peers: HashSet<String> = v.iter()
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
        println!("Min/Max degree: {} / {}", stats.min_degree, stats.max_degree);
        println!("Isolated nodes: {}", stats.isolated_nodes);
        println!("Connected: {}", stats.is_connected);
        println!("Components: {} (largest: {})", stats.component_count, stats.largest_component);
        println!("Clustering coefficient: {:.4}", stats.clustering_coefficient);
        println!("Degree distribution: {:?}", stats.degree_distribution);
    }

    /// Print simulation summary
    pub fn print_sim_stats(stats: &SimStats) {
        println!("=== Simulation Stats ===");
        println!("Total joins: {}", stats.total_joins);
        println!("Total leaves: {}", stats.total_leaves);
        println!("Connections formed: {}", stats.total_connections_formed);
        println!("Connections lost: {}", stats.total_connections_lost);
        println!("Topology snapshots: {}", stats.topology_snapshots.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_webrtc_sim_small() {
        let config = SimConfig {
            node_count: 10,
            duration: Duration::from_secs(2),
            seed: 42,
            pool: PoolConfig { max_connections: 5, satisfied_connections: 3 },
            discovery_interval_ms: 100,
            churn_rate: 0.0,
            allow_rejoin: false,
            network_latency_ms: 0,
        };

        let sim = Simulation::new(config);
        sim.run().await;

        let stats = sim.analyze_topology().await;
        println!("\nSmall simulation results:");
        Simulation::print_topology_stats(&stats);

        assert_eq!(stats.node_count, 10);
        assert!(stats.connection_count > 0, "Should have some connections");
    }

    #[tokio::test]
    async fn test_webrtc_sim_with_churn() {
        let config = SimConfig {
            node_count: 20,
            duration: Duration::from_secs(3),
            seed: 123,
            pool: PoolConfig { max_connections: 5, satisfied_connections: 3 },
            discovery_interval_ms: 100,
            churn_rate: 0.05,
            allow_rejoin: true,
            network_latency_ms: 0,
        };

        let sim = Simulation::new(config);
        sim.run().await;

        let stats = sim.analyze_topology().await;
        let sim_stats = sim.get_stats().await;

        println!("\nSimulation with churn:");
        Simulation::print_topology_stats(&stats);
        Simulation::print_sim_stats(&sim_stats);

        assert!(sim_stats.total_joins >= 20, "Should have at least initial joins");
    }

    #[tokio::test]
    async fn test_webrtc_sim_200_nodes_connectivity() {
        let config = SimConfig {
            node_count: 200,
            duration: Duration::from_secs(15),
            seed: 42,
            pool: PoolConfig { max_connections: 20, satisfied_connections: 10 },
            discovery_interval_ms: 100,
            churn_rate: 0.0,
            allow_rejoin: false,
            network_latency_ms: 0,
        };

        let sim = Simulation::new(config);
        sim.run().await;

        let stats = sim.analyze_topology().await;

        println!("\n=== 200 Node Connectivity Test (10/20 pool) ===");
        Simulation::print_topology_stats(&stats);

        assert_eq!(stats.node_count, 200, "Should have 200 nodes");
        assert!(stats.connection_count > 0, "Should have connections");
        // With 10/20 pool settings, should have 1 connected component
        assert_eq!(
            stats.component_count, 1,
            "Should be fully connected (1 component), got {}",
            stats.component_count
        );
    }
}
