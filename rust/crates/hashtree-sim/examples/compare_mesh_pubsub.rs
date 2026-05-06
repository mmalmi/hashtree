use hashtree_network::{PubsubSchedulerConfig, PubsubSchedulingPolicy, MESH_EVENT_POLICY};
use hashtree_sim::{
    run_mesh_pubsub_htl_flood_baseline, run_mesh_pubsub_htl_inv_want_baseline,
    run_mesh_pubsub_sweep, MeshPubsubWorkloadConfig, MeshPubsubWorkloadReport, PoolConfig,
};
use std::env;
use std::io::{self, Write};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
struct Variant {
    label: &'static str,
    policy: PubsubSchedulingPolicy,
    fanout: usize,
}

#[derive(Debug, Clone)]
struct RunOptions {
    node_counts: Vec<usize>,
    subscribers_per_author: Option<usize>,
    spam_subscribers_per_author: Option<usize>,
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

fn run_options_from_args() -> RunOptions {
    let mut options = RunOptions {
        node_counts: Vec::new(),
        subscribers_per_author: None,
        spam_subscribers_per_author: None,
    };

    for arg in env::args().skip(1) {
        if let Some(value) = arg.strip_prefix("--subs=") {
            options.subscribers_per_author = value.parse::<usize>().ok();
            continue;
        }
        if let Some(value) = arg.strip_prefix("--spam-subs=") {
            options.spam_subscribers_per_author = value.parse::<usize>().ok();
            continue;
        }
        options.node_counts.extend(
            arg.split(',')
                .filter_map(|part| part.parse::<usize>().ok())
                .filter(|count| *count > 0),
        );
    }

    if options.node_counts.is_empty() {
        options.node_counts.push(24);
    }
    options
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let variants = [
        Variant {
            label: "fair-f2",
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 2,
        },
        Variant {
            label: "random-f2",
            policy: PubsubSchedulingPolicy::Random,
            fanout: 2,
        },
        Variant {
            label: "reciprocal-f2",
            policy: PubsubSchedulingPolicy::Reciprocal,
            fanout: 2,
        },
        Variant {
            label: "aging-recip-f2",
            policy: PubsubSchedulingPolicy::AgingReciprocal,
            fanout: 2,
        },
        Variant {
            label: "reciprocal-f4",
            policy: PubsubSchedulingPolicy::Reciprocal,
            fanout: 4,
        },
        Variant {
            label: "fair-f4",
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 4,
        },
        Variant {
            label: "fair-f8",
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 8,
        },
        Variant {
            label: "fair-f14",
            policy: PubsubSchedulingPolicy::Fair,
            fanout: 14,
        },
    ];

    let options = run_options_from_args();
    for node_count in options.node_counts {
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
        let htl_config = workload(
            17,
            node_count,
            subscribers,
            spam_subscribers,
            Variant {
                label: "htl-flood-h4",
                policy: PubsubSchedulingPolicy::Fair,
                fanout: 4,
            },
        );
        let started = Instant::now();
        let htl_report =
            run_mesh_pubsub_htl_flood_baseline(htl_config, MESH_EVENT_POLICY.max_htl).await;
        print_report("htl-flood-h4", &htl_report, started.elapsed().as_secs_f64());
        io::stdout().flush().expect("flush stdout");

        let htl_config = workload(
            17,
            node_count,
            subscribers,
            spam_subscribers,
            Variant {
                label: "htl-invwant-h4",
                policy: PubsubSchedulingPolicy::Fair,
                fanout: 4,
            },
        );
        let started = Instant::now();
        let htl_report =
            run_mesh_pubsub_htl_inv_want_baseline(htl_config, MESH_EVENT_POLICY.max_htl).await;
        print_report(
            "htl-invwant-h4",
            &htl_report,
            started.elapsed().as_secs_f64(),
        );
        io::stdout().flush().expect("flush stdout");

        for variant in variants {
            let config = workload(17, node_count, subscribers, spam_subscribers, variant);
            let started = Instant::now();
            let result = run_mesh_pubsub_sweep(&[config]).await;
            print_report(
                variant.label,
                &result[0].report,
                started.elapsed().as_secs_f64(),
            );
            io::stdout().flush().expect("flush stdout");
        }
    }
}
