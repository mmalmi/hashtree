//! Nostr-realistic workload sweep: many authors, each subscriber follows a
//! fraction of them. Shows where each protocol sits on the
//! many-tiny-topics frontier — the actual Nostr design question.
//!
//! Usage:
//!   cargo run --release -p hashtree-sim --example nostr_workload_compare
//!   cargo run --release -p hashtree-sim --example nostr_workload_compare -- \
//!       --nodes=1000 --authors=500 --follows=100 --rounds=2 --zipf=1.0
//!
//! Default sweeps `(nodes, authors, follows, distribution)` to surface the
//! crossover point where bandwidth winner flips between invwant (cheap
//! global INV flood, selective payload routing) and topic-mesh
//! plumtree/gossipsub (per-topic mesh formation).

use hashtree_network::MESH_EVENT_POLICY;
use hashtree_sim::{
    compute_workload_peer_graph, run_mesh_pubsub_htl_baseline_on_graph, FollowDistribution,
    HtlBaselineMode, MeshPubsubWorkloadConfig, MeshPubsubWorkloadReport, NostrWorkloadParams,
    PoolConfig,
};
use std::env;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone)]
struct RunOptions {
    node_counts: Vec<usize>,
    authors: Vec<usize>,
    follows: Vec<usize>,
    rounds: usize,
    zipf_alphas: Vec<f64>, // empty = uniform
    seed: u64,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            node_counts: vec![256, 1000],
            authors: vec![64, 250],
            follows: vec![16, 64],
            rounds: 2,
            zipf_alphas: vec![0.0, 1.0], // 0 means Uniform; >0 means Zipf
            seed: 31,
        }
    }
}

fn parse_args() -> RunOptions {
    let mut opts = RunOptions::default();
    let mut explicit_nodes = false;
    let mut explicit_authors = false;
    let mut explicit_follows = false;
    let mut explicit_zipf = false;
    for arg in env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--nodes=") {
            opts.node_counts = v
                .split(',')
                .filter_map(|p| p.parse::<usize>().ok())
                .collect();
            explicit_nodes = true;
        } else if let Some(v) = arg.strip_prefix("--authors=") {
            opts.authors = v
                .split(',')
                .filter_map(|p| p.parse::<usize>().ok())
                .collect();
            explicit_authors = true;
        } else if let Some(v) = arg.strip_prefix("--follows=") {
            opts.follows = v
                .split(',')
                .filter_map(|p| p.parse::<usize>().ok())
                .collect();
            explicit_follows = true;
        } else if let Some(v) = arg.strip_prefix("--rounds=") {
            opts.rounds = v.parse::<usize>().unwrap_or(opts.rounds);
        } else if let Some(v) = arg.strip_prefix("--zipf=") {
            opts.zipf_alphas = v.split(',').filter_map(|p| p.parse::<f64>().ok()).collect();
            explicit_zipf = true;
        } else if let Some(v) = arg.strip_prefix("--seed=") {
            opts.seed = v.parse::<u64>().unwrap_or(opts.seed);
        }
    }
    let _ = (
        explicit_nodes,
        explicit_authors,
        explicit_follows,
        explicit_zipf,
    );
    opts
}

fn workload(
    seed: u64,
    node_count: usize,
    author_count: usize,
    follows_per_node: usize,
    distribution: FollowDistribution,
    publish_rounds: usize,
) -> MeshPubsubWorkloadConfig {
    MeshPubsubWorkloadConfig {
        seed,
        node_count,
        author_count: 0, // unused with nostr
        subscribers_per_author: 0,
        publish_rounds,
        payload_bytes: 1200,
        pool: PoolConfig {
            max_connections: 14,
            satisfied_connections: 8,
        },
        spam_author_count: 0,
        spam_subscribers_per_author: 0,
        spam_publish_rounds_per_round: 0,
        subscription_churn_rate: 0.0,
        allow_rejoin: false,
        pump_steps_after_setup: if node_count >= 1000 {
            240
        } else if node_count >= 100 {
            160
        } else {
            80
        },
        pump_steps_per_publish_round: if node_count >= 1000 { 128 } else { 64 },
        latency_per_pump_step_ms: 10,
        broken_edge_fraction: 0.0,
        nostr: Some(NostrWorkloadParams {
            author_count,
            follows_per_node,
            follow_distribution: distribution,
        }),
        ..Default::default()
    }
}

fn print_report(label: &str, r: &MeshPubsubWorkloadReport, elapsed_secs: f64) {
    println!(
        "{label:30} delivery={:6.2}% bytes/event={:9.1} dupes={:>6} p50={:>4}ms p95={:>5}ms runtime={:5.1}s",
        r.delivery_rate * 100.0,
        r.bytes_sent_per_delivered_event,
        r.duplicate_deliveries,
        r.delivery_latency_p50_ms,
        r.delivery_latency_p95_ms,
        elapsed_secs,
    );
}

fn distribution_label(alpha: f64) -> String {
    if alpha <= 0.0 {
        "uniform".to_string()
    } else {
        format!("zipf-α{alpha:.1}")
    }
}

#[tokio::main]
async fn main() {
    let opts = parse_args();
    let strategies: &[(&str, HtlBaselineMode)] = &[
        ("flood", HtlBaselineMode::FloodPayload),
        ("invwant", HtlBaselineMode::InvWant),
        ("invwant-ir", HtlBaselineMode::InvWantInterestRouted),
        (
            "plumtree-tm",
            HtlBaselineMode::EagerLazy {
                target_degree: None,
                ihave_timeout_hops: Some(1),
                peer_scoring: false,
                prune_backoff_rounds: 0,
                topic_mesh: true,
                fanout_peers_per_member: 0,
            },
        ),
        (
            "plumtree-tm-fan2",
            HtlBaselineMode::EagerLazy {
                target_degree: None,
                ihave_timeout_hops: Some(1),
                peer_scoring: false,
                prune_backoff_rounds: 0,
                topic_mesh: true,
                fanout_peers_per_member: 2,
            },
        ),
        (
            "plumtree-tm-fan6",
            HtlBaselineMode::EagerLazy {
                target_degree: None,
                ihave_timeout_hops: Some(1),
                peer_scoring: false,
                prune_backoff_rounds: 0,
                topic_mesh: true,
                fanout_peers_per_member: 6,
            },
        ),
        (
            "gossipsub-d6-v11-tm",
            HtlBaselineMode::EagerLazy {
                target_degree: Some(6),
                ihave_timeout_hops: Some(1),
                peer_scoring: true,
                prune_backoff_rounds: 4,
                topic_mesh: true,
                fanout_peers_per_member: 0,
            },
        ),
        (
            "gossipsub-d6-v11-tm-fan2",
            HtlBaselineMode::EagerLazy {
                target_degree: Some(6),
                ihave_timeout_hops: Some(1),
                peer_scoring: true,
                prune_backoff_rounds: 4,
                topic_mesh: true,
                fanout_peers_per_member: 2,
            },
        ),
        (
            "plumtree-whole-net",
            HtlBaselineMode::EagerLazy {
                target_degree: None,
                ihave_timeout_hops: Some(1),
                peer_scoring: false,
                prune_backoff_rounds: 0,
                topic_mesh: false,
                fanout_peers_per_member: 0,
            },
        ),
    ];

    for &node_count in &opts.node_counts {
        for &authors in &opts.authors {
            for &follows in &opts.follows {
                for &alpha in &opts.zipf_alphas {
                    let dist = if alpha <= 0.0 {
                        FollowDistribution::Uniform
                    } else {
                        FollowDistribution::Zipf { alpha }
                    };
                    let scenario_label = format!(
                        "nodes={node_count} authors={authors} follows={follows} dist={}",
                        distribution_label(alpha)
                    );
                    println!("\n=== {scenario_label} ===");

                    let setup_cfg =
                        workload(opts.seed, node_count, authors, follows, dist, opts.rounds);
                    let setup_started = Instant::now();
                    let (graph, topology) = compute_workload_peer_graph(&setup_cfg).await;
                    let setup_secs = setup_started.elapsed().as_secs_f64();
                    eprintln!("[setup] graph_setup_elapsed={setup_secs:5.1}s");

                    let graph = Arc::new(graph);
                    let topology = Arc::new(topology);
                    let mut handles = Vec::with_capacity(strategies.len());
                    for (label, mode) in strategies {
                        let label = (*label).to_string();
                        let mode = *mode;
                        let graph = graph.clone();
                        let topology = topology.clone();
                        let cfg =
                            workload(opts.seed, node_count, authors, follows, dist, opts.rounds);
                        let h = tokio::task::spawn_blocking(move || {
                            let started = Instant::now();
                            let r = run_mesh_pubsub_htl_baseline_on_graph(
                                graph.as_ref(),
                                topology.as_ref(),
                                &cfg,
                                MESH_EVENT_POLICY.max_htl,
                                mode,
                            );
                            (label, r, started.elapsed().as_secs_f64())
                        });
                        handles.push(h);
                    }
                    let mut results = Vec::with_capacity(handles.len());
                    for h in handles {
                        results.push(h.await.expect("nostr workload task panicked"));
                    }
                    results.sort_by_key(|(label, _, _)| {
                        strategies
                            .iter()
                            .position(|(name, _)| *name == label.as_str())
                            .unwrap_or(usize::MAX)
                    });
                    for (label, r, secs) in results {
                        print_report(&label, &r, secs);
                    }
                }
            }
        }
    }
}
