use hashtree_network::{
    PubsubDeliveryMode, PubsubSchedulerConfig, PubsubSchedulingPolicy, MESH_EVENT_POLICY,
};
use hashtree_sim::{
    compute_workload_peer_graph, run_mesh_pubsub_htl_baseline_on_graph, run_mesh_pubsub_sweep,
    HtlBaselineMode, MeshPubsubWorkloadConfig, MeshPubsubWorkloadReport, PoolConfig,
};
use std::collections::BTreeSet;
use std::env;
use std::future::Future;
use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Variant {
    label: &'static str,
    delivery_mode: PubsubDeliveryMode,
    policy: PubsubSchedulingPolicy,
    fanout: usize,
}

#[derive(Debug, Clone)]
struct RunOptions {
    node_counts: Vec<usize>,
    subscribers_per_author: Option<usize>,
    spam_subscribers_per_author: Option<usize>,
    progress_interval_secs: Option<u64>,
    only_labels: BTreeSet<String>,
    broken_edge_fraction: f64,
}

#[derive(Debug, Clone, Copy)]
struct RunProgress {
    index: usize,
    total: usize,
}

fn scaled_count(node_count: usize, fraction: usize, minimum: usize) -> usize {
    (node_count / fraction)
        .max(minimum)
        .min(node_count.saturating_sub(1))
}

fn pump_steps_after_setup(node_count: usize) -> usize {
    if node_count >= 1000 {
        240
    } else if node_count >= 100 {
        160
    } else {
        80
    }
}

fn pump_steps_per_publish_round(node_count: usize) -> usize {
    if node_count >= 1000 {
        128
    } else if node_count >= 100 {
        96
    } else {
        48
    }
}

fn workload(
    seed: u64,
    node_count: usize,
    subscribers_per_author: usize,
    spam_subscribers_per_author: usize,
    broken_edge_fraction: f64,
    variant: Variant,
) -> MeshPubsubWorkloadConfig {
    MeshPubsubWorkloadConfig {
        seed,
        node_count,
        author_count: 3,
        subscribers_per_author,
        publish_rounds: 2,
        payload_bytes: 1200,
        pool: PoolConfig {
            max_connections: 14,
            satisfied_connections: 8,
        },
        pubsub_scheduler: PubsubSchedulerConfig {
            policy: variant.policy,
            fanout: variant.fanout,
            anonymous_free_credit_bytes: 4 * 1024,
            reciprocal_credit_multiplier: 1.0,
            aging_credit_bytes: 2 * 1024,
        },
        pubsub_delivery_mode: variant.delivery_mode,
        reciprocal_provider_fraction: 0.65,
        reciprocal_credit_bytes: 192 * 1024,
        subscription_churn_rate: 0.05,
        allow_rejoin: true,
        spam_author_count: 3,
        spam_subscribers_per_author,
        spam_publish_rounds_per_round: 2,
        pump_steps_after_setup: pump_steps_after_setup(node_count),
        pump_steps_per_publish_round: pump_steps_per_publish_round(node_count),
        latency_per_pump_step_ms: 10,
        broken_edge_fraction,
    }
}

fn print_report(label: &str, report: &MeshPubsubWorkloadReport, elapsed_secs: f64) {
    println!(
        "{label:18} delivery={:6.2}% loss={:6.2}% p50={:4}ms p95={:4}ms bytes/event={:8.1} useful_credit={} spam_delivery={:6.2}% dupes={} peers={:4.1}/{}/{} iso={} runtime={elapsed_secs:6.1}s",
        report.delivery_rate * 100.0,
        report.loss_rate * 100.0,
        report.delivery_latency_p50_ms,
        report.delivery_latency_p95_ms,
        report.bytes_sent_per_delivered_event,
        report.useful_bytes_received,
        report.spam_delivery_rate * 100.0,
        report.duplicate_deliveries,
        report.average_peer_count,
        report.min_peer_count,
        report.max_peer_count,
        report.isolated_nodes,
    );
}

async fn run_with_progress<T>(
    label: &str,
    node_count: usize,
    progress: RunProgress,
    progress_interval_secs: Option<u64>,
    future: impl Future<Output = T>,
) -> (T, f64) {
    let started = Instant::now();
    let progress_thread = progress_interval_secs
        .filter(|seconds| *seconds > 0)
        .map(|seconds| {
            let label = label.to_string();
            let (stop_tx, stop_rx) = mpsc::channel();
            let handle = thread::spawn(move || loop {
                match stop_rx.recv_timeout(Duration::from_secs(seconds)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let completed = progress.index.saturating_sub(1);
                        let remaining = progress.total.saturating_sub(completed);
                        let percent = if progress.total == 0 {
                            100.0
                        } else {
                            completed as f64 / progress.total as f64 * 100.0
                        };
                        eprintln!(
                            "[progress] run={}/{} completed={completed}/{} done={percent:5.1}% remaining_runs={} current={label:18} nodes={node_count} elapsed={:6.1}s",
                            progress.index,
                            progress.total,
                            progress.total,
                            remaining,
                            started.elapsed().as_secs_f64()
                        );
                    }
                }
            });
            (stop_tx, handle)
        });

    let result = future.await;
    let elapsed_secs = started.elapsed().as_secs_f64();
    if let Some((stop_tx, handle)) = progress_thread {
        let _ = stop_tx.send(());
        let _ = handle.join();
    }
    (result, elapsed_secs)
}

fn run_options_from_args() -> RunOptions {
    let mut options = RunOptions {
        node_counts: Vec::new(),
        subscribers_per_author: None,
        spam_subscribers_per_author: None,
        progress_interval_secs: Some(10),
        only_labels: BTreeSet::new(),
        broken_edge_fraction: 0.0,
    };

    for arg in env::args().skip(1) {
        if arg == "--no-progress" {
            options.progress_interval_secs = None;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--progress=") {
            options.progress_interval_secs = value.parse::<u64>().ok();
            continue;
        }
        if let Some(value) = arg.strip_prefix("--subs=") {
            options.subscribers_per_author = value.parse::<usize>().ok();
            continue;
        }
        if let Some(value) = arg.strip_prefix("--spam-subs=") {
            options.spam_subscribers_per_author = value.parse::<usize>().ok();
            continue;
        }
        if let Some(value) = arg.strip_prefix("--broken=") {
            options.broken_edge_fraction = value.parse::<f64>().unwrap_or(0.0).clamp(0.0, 1.0);
            continue;
        }
        if let Some(value) = arg.strip_prefix("--only=") {
            options.only_labels.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string),
            );
            continue;
        }
        options.node_counts.extend(
            arg.split(',')
                .filter_map(|part| part.parse::<usize>().ok())
                .filter(|count| *count > 0),
        );
    }

    if options.node_counts.is_empty() {
        options.node_counts.extend_from_slice(&[100, 1000]);
    }
    options
}

fn includes_label(options: &RunOptions, label: &str) -> bool {
    options.only_labels.is_empty() || options.only_labels.contains(label)
}

#[tokio::main]
async fn main() {
    let variants = [
        Variant {
            label: "push-fair-f2",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 2,
        },
        Variant {
            label: "push-rand-f2",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::Random,
            fanout: 2,
        },
        Variant {
            label: "push-recip-f2",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::Reciprocal,
            fanout: 2,
        },
        Variant {
            label: "push-aging-f2",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::AgingReciprocal,
            fanout: 2,
        },
        Variant {
            label: "push-recip-f4",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::Reciprocal,
            fanout: 4,
        },
        Variant {
            label: "push-fair-f4",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 4,
        },
        Variant {
            label: "push-fair-f8",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 8,
        },
        Variant {
            label: "push-fair-f14",
            delivery_mode: PubsubDeliveryMode::InterestPush,
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 14,
        },
        Variant {
            label: "prod-invwant",
            delivery_mode: PubsubDeliveryMode::HtlInvWant,
            policy: PubsubSchedulingPolicy::Reciprocal,
            fanout: 8,
        },
    ];

    let options = run_options_from_args();
    let baseline_labels = [
        "htl-flood-h4",
        "htl-invwant-h4",
        "htl-plumtree-h4",
        "htl-plumtree-h4-t1",
        "htl-plumtree-h4-t0",
        "htl-gossipsub-d6-h4",
        "htl-gossipsub-d6-h4-t0",
        "htl-gossipsub-d6-v11-h4",
    ];
    let selected_baselines = baseline_labels
        .iter()
        .filter(|label| includes_label(&options, label))
        .count();
    let selected_variants = variants
        .iter()
        .filter(|variant| includes_label(&options, variant.label))
        .count();
    let run_total = options.node_counts.len() * (selected_baselines + selected_variants);
    let mut run_index = 0usize;
    for node_count in options.node_counts.iter().copied() {
        let subscribers = options
            .subscribers_per_author
            .unwrap_or_else(|| scaled_count(node_count, 4, 8))
            .min(node_count.saturating_sub(1));
        let spam_subscribers = options
            .spam_subscribers_per_author
            .unwrap_or_else(|| scaled_count(node_count, 8, 6))
            .min(node_count.saturating_sub(1));

        println!(
            "production MeshStoreCore pubsub workload: {node_count} nodes, 3 useful authors x {subscribers} subscribers, 3 spam authors x {spam_subscribers} subscribers, churn=5%, payload=1200B"
        );
        // Collect all selected HTL graph baselines. They share a precomputed
        // peer graph (formed by the same MeshRouter signaling regardless of
        // pubsub_scheduler config), so we pay the heavy mesh-formation cost
        // once and run all variants in parallel via spawn_blocking.
        let htl_modes: &[(&str, HtlBaselineMode)] = &[
            ("htl-flood-h4", HtlBaselineMode::FloodPayload),
            ("htl-invwant-h4", HtlBaselineMode::InvWant),
            (
                "htl-plumtree-h4",
                HtlBaselineMode::EagerLazy {
                    target_degree: None,
                    ihave_timeout_hops: None,
                    peer_scoring: false,
                    prune_backoff_rounds: 0,
                },
            ),
            (
                "htl-plumtree-h4-t1",
                HtlBaselineMode::EagerLazy {
                    target_degree: None,
                    ihave_timeout_hops: Some(1),
                    peer_scoring: false,
                    prune_backoff_rounds: 0,
                },
            ),
            (
                "htl-plumtree-h4-t0",
                HtlBaselineMode::EagerLazy {
                    target_degree: None,
                    ihave_timeout_hops: Some(0),
                    peer_scoring: false,
                    prune_backoff_rounds: 0,
                },
            ),
            (
                "htl-gossipsub-d6-h4",
                HtlBaselineMode::EagerLazy {
                    target_degree: Some(6),
                    ihave_timeout_hops: Some(1),
                    peer_scoring: false,
                    prune_backoff_rounds: 0,
                },
            ),
            (
                "htl-gossipsub-d6-h4-t0",
                HtlBaselineMode::EagerLazy {
                    target_degree: Some(6),
                    ihave_timeout_hops: Some(0),
                    peer_scoring: false,
                    prune_backoff_rounds: 0,
                },
            ),
            (
                "htl-gossipsub-d6-v11-h4",
                HtlBaselineMode::EagerLazy {
                    target_degree: Some(6),
                    ihave_timeout_hops: Some(1),
                    peer_scoring: true,
                    prune_backoff_rounds: 4,
                },
            ),
        ];
        let selected_htl: Vec<(&str, HtlBaselineMode)> = htl_modes
            .iter()
            .copied()
            .filter(|(label, _)| includes_label(&options, label))
            .collect();
        if !selected_htl.is_empty() {
            let setup_config = workload(
                17,
                node_count,
                subscribers,
                spam_subscribers,
                options.broken_edge_fraction,
                Variant {
                    label: "htl-graph-setup",
                    delivery_mode: PubsubDeliveryMode::InterestPush,
                    policy: PubsubSchedulingPolicy::Fair,
                    fanout: 4,
                },
            );
            let setup_started = Instant::now();
            let (graph, topology) = compute_workload_peer_graph(&setup_config).await;
            let setup_secs = setup_started.elapsed().as_secs_f64();
            eprintln!(
                "[setup] node_count={node_count} graph_setup_elapsed={setup_secs:6.1}s baselines={}",
                selected_htl.len()
            );

            let graph = Arc::new(graph);
            let topology = Arc::new(topology);
            let mut handles = Vec::with_capacity(selected_htl.len());
            for (label, mode) in &selected_htl {
                run_index += 1;
                let label = (*label).to_string();
                let mode = *mode;
                let graph = graph.clone();
                let topology = topology.clone();
                let cfg = workload(
                    17,
                    node_count,
                    subscribers,
                    spam_subscribers,
                    options.broken_edge_fraction,
                    Variant {
                        label: "htl-on-graph",
                        delivery_mode: PubsubDeliveryMode::InterestPush,
                        policy: PubsubSchedulingPolicy::Fair,
                        fanout: 4,
                    },
                );
                let handle = tokio::task::spawn_blocking(move || {
                    let started = Instant::now();
                    let report = run_mesh_pubsub_htl_baseline_on_graph(
                        graph.as_ref(),
                        topology.as_ref(),
                        &cfg,
                        MESH_EVENT_POLICY.max_htl,
                        mode,
                    );
                    let elapsed = started.elapsed().as_secs_f64();
                    (label, report, elapsed)
                });
                handles.push(handle);
            }
            let _ = run_index; // silence unused-mut if all htl-* paths skip
            let mut results = Vec::with_capacity(handles.len());
            for handle in handles {
                results.push(handle.await.expect("htl baseline task panicked"));
            }
            // Print in declared order so output stays stable across runs.
            results.sort_by_key(|(label, _, _)| {
                htl_modes
                    .iter()
                    .position(|(name, _)| *name == label.as_str())
                    .unwrap_or(usize::MAX)
            });
            for (label, report, elapsed_secs) in results {
                print_report(&label, &report, elapsed_secs);
                io::stdout().flush().expect("flush stdout");
            }
        }

        for variant in variants {
            if !includes_label(&options, variant.label) {
                continue;
            }
            let config = workload(
                17,
                node_count,
                subscribers,
                spam_subscribers,
                options.broken_edge_fraction,
                variant,
            );
            run_index += 1;
            let (result, elapsed_secs) = run_with_progress(
                variant.label,
                node_count,
                RunProgress {
                    index: run_index,
                    total: run_total,
                },
                options.progress_interval_secs,
                run_mesh_pubsub_sweep(&[config]),
            )
            .await;
            print_report(variant.label, &result[0].report, elapsed_secs);
            io::stdout().flush().expect("flush stdout");
        }
    }
}
