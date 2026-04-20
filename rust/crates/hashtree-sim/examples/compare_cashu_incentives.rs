use hashtree_sim::{
    run_parameter_sweep, CashuIncentiveConfig, NodeStrategyProfile, PoolConfig,
    RequestDispatchConfig, ResponseBehaviorConfig, SelectionStrategy, SimConfig, SweepResult,
};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
struct Variant {
    label: &'static str,
    cashu: Option<CashuIncentiveConfig>,
}

#[derive(Debug, Default)]
struct Aggregate {
    runs: usize,
    success_rate_sum: f64,
    p95_ms_sum: f64,
    overhead_ratio_sum: f64,
    component_count_sum: f64,
    largest_component_share_sum: f64,
    tick_p95_us_sum: f64,
    peak_links_sum: f64,
    cashu_payments_sum: u64,
    cashu_payment_failures_sum: u64,
    cashu_priority_credits_sum: u64,
}

impl Aggregate {
    fn record(&mut self, result: &SweepResult) {
        self.runs += 1;
        self.success_rate_sum += result.stats.retrieval.success_rate;
        self.p95_ms_sum += result.stats.retrieval.p95_latency_ms as f64;
        let overhead_ratio = if result.stats.retrieval.payload_bytes == 0 {
            0.0
        } else {
            result.stats.retrieval.data_plane_bytes as f64
                / result.stats.retrieval.payload_bytes as f64
        };
        self.overhead_ratio_sum += overhead_ratio;
        self.component_count_sum += result.final_topology.component_count as f64;
        self.largest_component_share_sum += if result.final_topology.node_count == 0 {
            0.0
        } else {
            result.final_topology.largest_component as f64 / result.final_topology.node_count as f64
        };
        self.tick_p95_us_sum += result.stats.local_resources.tick_p95_us as f64;
        self.peak_links_sum += result.stats.local_resources.peak_connection_pairs as f64;
        self.cashu_payments_sum += result.stats.cashu.payments_sent;
        self.cashu_payment_failures_sum += result.stats.cashu.payments_failed;
        self.cashu_priority_credits_sum += result.stats.cashu.priority_credits_applied;
    }

    fn avg(&self, value: f64) -> f64 {
        if self.runs == 0 {
            0.0
        } else {
            value / self.runs as f64
        }
    }
}

fn mixed_bad_actor_config(seed: u64, cashu: Option<CashuIncentiveConfig>) -> SimConfig {
    let reference_dispatch = RequestDispatchConfig {
        initial_fanout: 1,
        hedge_fanout: 1,
        max_fanout: 4,
        hedge_interval_ms: 8,
    };
    SimConfig {
        node_count: 120,
        duration: Duration::from_secs(5),
        seed,
        pool: PoolConfig {
            max_connections: 16,
            satisfied_connections: 8,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 400,
        churn_rate: 0.02,
        allow_rejoin: true,
        network_latency_ms: 30,
        retrieval_probe_count: 36,
        retrieval_payload_bytes: 1024,
        retrieval_timeout_ms: 700,
        max_events_retained: 20_000,
        retrieval_timing_mode: hashtree_sim::RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: vec![
            NodeStrategyProfile {
                name: "reference".to_string(),
                weight: 60,
                pool: PoolConfig {
                    max_connections: 18,
                    satisfied_connections: 9,
                },
                selection_strategy: SelectionStrategy::Weighted,
                fairness_enabled: true,
                dispatch: reference_dispatch,
                response_behavior: ResponseBehaviorConfig::default(),
            },
            NodeStrategyProfile {
                name: "goofball".to_string(),
                weight: 25,
                pool: PoolConfig {
                    max_connections: 12,
                    satisfied_connections: 6,
                },
                selection_strategy: SelectionStrategy::RoundRobin,
                fairness_enabled: true,
                dispatch: RequestDispatchConfig::default(),
                response_behavior: ResponseBehaviorConfig {
                    drop_response_prob: 0.25,
                    corrupt_response_prob: 0.05,
                    extra_delay_ms: 15,
                    first_byte_delay_ms: 25,
                    bytes_per_second: 18_000,
                    stall_response_prob: 0.15,
                    stall_delay_ms: 60,
                },
            },
            NodeStrategyProfile {
                name: "adversarial".to_string(),
                weight: 15,
                pool: PoolConfig {
                    max_connections: 20,
                    satisfied_connections: 10,
                },
                selection_strategy: SelectionStrategy::Random,
                fairness_enabled: true,
                dispatch: RequestDispatchConfig::default(),
                response_behavior: ResponseBehaviorConfig {
                    drop_response_prob: 0.55,
                    corrupt_response_prob: 0.35,
                    extra_delay_ms: 5,
                    first_byte_delay_ms: 10,
                    bytes_per_second: 9_000,
                    stall_response_prob: 0.45,
                    stall_delay_ms: 120,
                },
            },
        ],
        reference_strategy: Some("reference".to_string()),
        cashu_incentives: cashu,
    }
}

fn print_variant_summary(label: &str, agg: &Aggregate) {
    println!(
        "{label:>14} | runs={:>2} | success={:>6.2}% | p95={:>6.1}ms | overhead={:>6.3} | components={:>5.2} | largest_share={:>5.3} | tick_p95={:>7.1}us | peak_links={:>6.1} | payments={:>4} | pay_fail={:>4} | priority_credits={:>4}",
        agg.runs,
        agg.avg(agg.success_rate_sum) * 100.0,
        agg.avg(agg.p95_ms_sum),
        agg.avg(agg.overhead_ratio_sum),
        agg.avg(agg.component_count_sum),
        agg.avg(agg.largest_component_share_sum),
        agg.avg(agg.tick_p95_us_sum),
        agg.avg(agg.peak_links_sum),
        agg.cashu_payments_sum / agg.runs.max(1) as u64,
        agg.cashu_payment_failures_sum / agg.runs.max(1) as u64,
        agg.cashu_priority_credits_sum / agg.runs.max(1) as u64
    );
}

fn print_delta(label: &str, baseline: &Aggregate, other: &Aggregate) {
    let base_success = baseline.avg(baseline.success_rate_sum);
    let base_p95 = baseline.avg(baseline.p95_ms_sum);
    let base_overhead = baseline.avg(baseline.overhead_ratio_sum);

    let other_success = other.avg(other.success_rate_sum);
    let other_p95 = other.avg(other.p95_ms_sum);
    let other_overhead = other.avg(other.overhead_ratio_sum);

    let success_delta_pp = (other_success - base_success) * 100.0;
    let p95_delta_pct = if base_p95 > 0.0 {
        (other_p95 - base_p95) / base_p95 * 100.0
    } else {
        0.0
    };
    let overhead_delta_pct = if base_overhead > 0.0 {
        (other_overhead - base_overhead) / base_overhead * 100.0
    } else {
        0.0
    };

    println!(
        "{label:>14} vs baseline: success_delta={:+.2}pp, p95_delta={:+.2}%, overhead_delta={:+.2}%",
        success_delta_pp, p95_delta_pct, overhead_delta_pct
    );
}

#[tokio::main]
async fn main() {
    let seeds = [11_u64, 22, 33, 44];
    let variants = [
        Variant {
            label: "baseline",
            cashu: None,
        },
        Variant {
            label: "cashu_light",
            cashu: Some(CashuIncentiveConfig {
                enabled: true,
                channel_capacity_sat: 128,
                payment_per_probe_sat: 1,
                selection_bonus_weight: 0.35,
                payment_default_block_threshold: 0,
            }),
        },
        Variant {
            label: "cashu_strong",
            cashu: Some(CashuIncentiveConfig {
                enabled: true,
                channel_capacity_sat: 256,
                payment_per_probe_sat: 2,
                selection_bonus_weight: 0.8,
                payment_default_block_threshold: 0,
            }),
        },
    ];

    let mut configs = Vec::new();
    let mut config_variant_idx = Vec::new();
    for (variant_idx, variant) in variants.iter().enumerate() {
        for seed in seeds {
            configs.push(mixed_bad_actor_config(seed, variant.cashu));
            config_variant_idx.push(variant_idx);
        }
    }

    println!(
        "Running {} simulations ({} variants x {} seeds)...",
        configs.len(),
        variants.len(),
        seeds.len()
    );
    let results = run_parameter_sweep(&configs).await;

    let mut aggregates: Vec<Aggregate> =
        (0..variants.len()).map(|_| Aggregate::default()).collect();
    for (idx, result) in results.iter().enumerate() {
        let variant_idx = config_variant_idx[idx];
        aggregates[variant_idx].record(result);
    }

    println!("\nVariant summary (averages over seeds):");
    for (variant, agg) in variants.iter().zip(aggregates.iter()) {
        print_variant_summary(variant.label, agg);
    }

    println!("\nDelta vs baseline:");
    let baseline = &aggregates[0];
    print_delta("cashu_light", baseline, &aggregates[1]);
    print_delta("cashu_strong", baseline, &aggregates[2]);
}
