use hashtree_network::{PubsubSchedulerConfig, PubsubSchedulingPolicy};
use hashtree_sim::{
    run_mesh_pubsub_sweep, MeshPubsubWorkloadConfig, MeshPubsubWorkloadReport, PoolConfig,
};

#[derive(Debug, Clone, Copy)]
struct Variant {
    label: &'static str,
    policy: PubsubSchedulingPolicy,
    fanout: usize,
}

fn workload(seed: u64, variant: Variant) -> MeshPubsubWorkloadConfig {
    MeshPubsubWorkloadConfig {
        seed,
        node_count: 24,
        author_count: 3,
        subscribers_per_author: 8,
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
        spam_subscribers_per_author: 6,
        spam_publish_rounds_per_round: 2,
        pump_steps_after_setup: 80,
        pump_steps_per_publish_round: 48,
        latency_per_pump_step_ms: 10,
    }
}

fn print_report(label: &str, report: &MeshPubsubWorkloadReport) {
    println!(
        "{label:18} delivery={:6.2}% loss={:6.2}% p50={:4}ms p95={:4}ms bytes/event={:8.1} useful_credit={} spam_delivery={:6.2}% dupes={}",
        report.delivery_rate * 100.0,
        report.loss_rate * 100.0,
        report.delivery_latency_p50_ms,
        report.delivery_latency_p95_ms,
        report.bytes_sent_per_delivered_event,
        report.useful_bytes_received,
        report.spam_delivery_rate * 100.0,
        report.duplicate_deliveries,
    );
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
    ];
    let configs = variants
        .iter()
        .copied()
        .map(|variant| workload(17, variant))
        .collect::<Vec<_>>();

    println!("production MeshStoreCore pubsub workload: 24 nodes, 3 useful authors, 3 spam authors, churn=5%, payload=1200B");
    let results = run_mesh_pubsub_sweep(&configs).await;
    for (variant, result) in variants.iter().zip(results.iter()) {
        print_report(variant.label, &result.report);
    }
}
