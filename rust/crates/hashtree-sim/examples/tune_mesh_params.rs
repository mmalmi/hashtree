use hashtree_sim::{
    NodeStrategyProfile, PoolConfig, RequestDispatchConfig, ResponseBehaviorConfig,
    RetrievalTimingMode, SelectionStrategy, SimConfig, Simulation, SweepResult,
};
use std::env;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExploreMode {
    Manual,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkloadProfile {
    Standard,
    HeavyMedia,
}

#[derive(Debug, Clone, Copy)]
struct WorkloadRuntime {
    node_count: usize,
    duration_secs: u64,
    retrieval_probe_count: usize,
    retrieval_payload_bytes: usize,
    retrieval_timeout_ms: u64,
    max_events_retained: usize,
}

#[derive(Debug, Clone)]
struct Candidate {
    label: String,
    reference_pool: PoolConfig,
    conservative_pool: PoolConfig,
    aggressive_pool: PoolConfig,
    weights: (u32, u32, u32),
    reference_strategy: StrategyRuntime,
    conservative_strategy: StrategyRuntime,
    aggressive_strategy: StrategyRuntime,
}

#[derive(Debug, Clone, Copy)]
struct StrategyRuntime {
    selection_strategy: SelectionStrategy,
    fairness_enabled: bool,
    dispatch: RequestDispatchConfig,
    response_behavior: ResponseBehaviorConfig,
}

#[derive(Debug)]
struct Summary {
    label: String,
    reference_pool: PoolConfig,
    conservative_pool: PoolConfig,
    aggressive_pool: PoolConfig,
    weights: (u32, u32, u32),
    runs: usize,
    avg_success_rate: f64,
    avg_p95_ms: f64,
    avg_overhead_ratio: f64,
    avg_component_count: f64,
    avg_largest_component_share: f64,
    avg_tick_p95_us: f64,
    avg_peak_connection_pairs: f64,
    run_success_rates: Vec<f64>,
    run_largest_component_shares: Vec<f64>,
    run_component_counts: Vec<f64>,
}

#[derive(Debug, Clone, Copy)]
struct GateProfile {
    name: &'static str,
    min_success_rate: f64,
    min_largest_component_share: f64,
    max_component_count: f64,
    max_failed_runs: usize,
}

#[derive(Debug)]
struct ScoredSummary {
    summary: Summary,
    passes_gates: bool,
    failed_runs: usize,
    gate_failures: Vec<&'static str>,
    score: f64,
}

fn parse_mode() -> ExploreMode {
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--auto" | "--mode=auto" => return ExploreMode::Auto,
            "--manual" | "--mode=manual" => return ExploreMode::Manual,
            _ => {}
        }
    }
    ExploreMode::Manual
}

fn parse_workload_profile() -> WorkloadProfile {
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--heavy-media" | "--workload=heavy-media" => return WorkloadProfile::HeavyMedia,
            "--standard" | "--workload=standard" => return WorkloadProfile::Standard,
            _ => {}
        }
    }
    WorkloadProfile::Standard
}

fn parse_candidate_limit() -> Option<usize> {
    for arg in env::args().skip(1) {
        if let Some(raw) = arg.strip_prefix("--candidate-limit=") {
            if let Ok(limit) = raw.parse::<usize>() {
                return Some(limit.max(1));
            }
        }
    }
    None
}

fn parse_candidate_offset() -> usize {
    for arg in env::args().skip(1) {
        if let Some(raw) = arg.strip_prefix("--candidate-offset=") {
            if let Ok(offset) = raw.parse::<usize>() {
                return offset;
            }
        }
    }
    0
}

fn parse_seed_limit() -> Option<usize> {
    for arg in env::args().skip(1) {
        if let Some(raw) = arg.strip_prefix("--seed-limit=") {
            if let Ok(limit) = raw.parse::<usize>() {
                return Some(limit.max(1));
            }
        }
    }
    None
}

fn parse_seeds_override() -> Option<Vec<u64>> {
    for arg in env::args().skip(1) {
        if let Some(raw) = arg.strip_prefix("--seeds=") {
            let mut seeds = Vec::new();
            for part in raw.split(',') {
                let trimmed = part.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(seed) = trimmed.parse::<u64>() {
                    seeds.push(seed);
                }
            }
            if !seeds.is_empty() {
                return Some(seeds);
            }
        }
    }
    None
}

fn parse_timeout_override_ms() -> Option<u64> {
    for arg in env::args().skip(1) {
        if let Some(raw) = arg.strip_prefix("--timeout-ms=") {
            if let Ok(timeout_ms) = raw.parse::<u64>() {
                return Some(timeout_ms.max(1));
            }
        }
    }
    None
}

fn workload_runtime(profile: WorkloadProfile) -> WorkloadRuntime {
    match profile {
        WorkloadProfile::Standard => WorkloadRuntime {
            node_count: 90,
            duration_secs: 5,
            retrieval_probe_count: 20,
            retrieval_payload_bytes: 2048,
            retrieval_timeout_ms: 700,
            max_events_retained: 10_000,
        },
        WorkloadProfile::HeavyMedia => WorkloadRuntime {
            node_count: 60,
            duration_secs: 3,
            retrieval_probe_count: 12,
            retrieval_payload_bytes: 16 * 1024,
            retrieval_timeout_ms: 1_800,
            max_events_retained: 12_000,
        },
    }
}

async fn run_parameter_sweep_with_progress(configs: &[SimConfig]) -> Vec<SweepResult> {
    let mut results = Vec::with_capacity(configs.len());
    let started = Instant::now();
    for (index, config) in configs.iter().enumerate() {
        let sim = Simulation::new(config.clone());
        sim.run().await;
        let stats = sim.get_stats().await;
        let final_topology = sim.analyze_topology().await;
        let elapsed = started.elapsed().as_secs_f32();
        eprintln!(
            "[{}/{}] seed={} success={:.2}% p95={}ms elapsed={:.1}s",
            index + 1,
            configs.len(),
            config.seed,
            stats.retrieval.success_rate * 100.0,
            stats.retrieval.p95_latency_ms,
            elapsed,
        );
        results.push(SweepResult {
            config: config.clone(),
            stats,
            final_topology,
        });
    }
    results
}

fn flood_runtime(selection_strategy: SelectionStrategy) -> StrategyRuntime {
    StrategyRuntime {
        selection_strategy,
        fairness_enabled: true,
        dispatch: RequestDispatchConfig::default(),
        response_behavior: ResponseBehaviorConfig::default(),
    }
}

fn hedged_runtime(
    selection_strategy: SelectionStrategy,
    initial_fanout: usize,
    hedge_fanout: usize,
    max_fanout: usize,
    hedge_interval_ms: u64,
) -> StrategyRuntime {
    StrategyRuntime {
        selection_strategy,
        fairness_enabled: true,
        dispatch: RequestDispatchConfig {
            initial_fanout,
            hedge_fanout,
            max_fanout,
            hedge_interval_ms,
        },
        response_behavior: ResponseBehaviorConfig::default(),
    }
}

fn goofball_behavior() -> ResponseBehaviorConfig {
    ResponseBehaviorConfig {
        drop_response_prob: 0.25,
        corrupt_response_prob: 0.05,
        extra_delay_ms: 15,
        first_byte_delay_ms: 25,
        bytes_per_second: 18_000,
        stall_response_prob: 0.15,
        stall_delay_ms: 60,
    }
}

fn adversarial_behavior() -> ResponseBehaviorConfig {
    ResponseBehaviorConfig {
        drop_response_prob: 0.55,
        corrupt_response_prob: 0.35,
        extra_delay_ms: 5,
        first_byte_delay_ms: 10,
        bytes_per_second: 9_000,
        stall_response_prob: 0.45,
        stall_delay_ms: 120,
    }
}

fn approach_profiles() -> Vec<(
    &'static str,
    StrategyRuntime,
    StrategyRuntime,
    StrategyRuntime,
)> {
    vec![
        (
            "flood_weighted",
            flood_runtime(SelectionStrategy::Weighted),
            flood_runtime(SelectionStrategy::HighestSuccessRate),
            flood_runtime(SelectionStrategy::Weighted),
        ),
        (
            "hedged_weighted",
            hedged_runtime(SelectionStrategy::Weighted, 2, 2, usize::MAX, 5),
            hedged_runtime(SelectionStrategy::HighestSuccessRate, 1, 1, usize::MAX, 8),
            hedged_runtime(SelectionStrategy::Weighted, 3, 2, usize::MAX, 5),
        ),
        (
            "hedged_utility",
            hedged_runtime(SelectionStrategy::UtilityUcb, 2, 2, usize::MAX, 5),
            hedged_runtime(SelectionStrategy::HighestSuccessRate, 1, 1, usize::MAX, 8),
            hedged_runtime(SelectionStrategy::Weighted, 3, 3, usize::MAX, 4),
        ),
        (
            "hedged_latency",
            hedged_runtime(SelectionStrategy::LowestLatency, 2, 2, usize::MAX, 5),
            hedged_runtime(SelectionStrategy::HighestSuccessRate, 1, 1, usize::MAX, 8),
            hedged_runtime(SelectionStrategy::Weighted, 3, 2, usize::MAX, 5),
        ),
    ]
}

fn build_manual_candidates() -> Vec<Candidate> {
    let refs = [(16_usize, 8_usize), (18, 9), (20, 10), (24, 12)];
    let weights = (35_u32, 35_u32, 30_u32);
    let approaches = approach_profiles();

    let mut out = Vec::new();
    for (max_connections, satisfied_connections) in refs {
        for (approach, reference_strategy, conservative_strategy, aggressive_strategy) in
            &approaches
        {
            out.push(Candidate {
                label: format!("manual:{max_connections}/{satisfied_connections}:{approach}"),
                reference_pool: PoolConfig {
                    max_connections,
                    satisfied_connections,
                },
                conservative_pool: PoolConfig {
                    max_connections: 12,
                    satisfied_connections: 6,
                },
                aggressive_pool: PoolConfig {
                    max_connections: 24,
                    satisfied_connections: 12,
                },
                weights,
                reference_strategy: *reference_strategy,
                conservative_strategy: *conservative_strategy,
                aggressive_strategy: *aggressive_strategy,
            });
        }
    }
    out
}

fn build_auto_candidates() -> Vec<Candidate> {
    let reference_pools = [(14_usize, 7_usize), (16, 8), (18, 9), (20, 10), (24, 12)];
    let weight_sets = [(20_u32, 40_u32, 40_u32), (30, 35, 35), (40, 30, 30)];
    let approaches = approach_profiles();

    let mut out = Vec::new();
    for (max_connections, satisfied_connections) in reference_pools {
        let conservative = PoolConfig {
            max_connections: max_connections.saturating_sub(4).max(6),
            satisfied_connections: satisfied_connections.saturating_sub(2).max(3),
        };
        let aggressive = PoolConfig {
            max_connections: max_connections + 4,
            satisfied_connections: satisfied_connections + 2,
        };

        for weights in weight_sets {
            for (approach, reference_strategy, conservative_strategy, aggressive_strategy) in
                approaches.iter().take(2)
            {
                out.push(Candidate {
                    label: format!(
                        "auto:{max_connections}/{satisfied_connections}:w{}-{}-{}:{approach}",
                        weights.0, weights.1, weights.2
                    ),
                    reference_pool: PoolConfig {
                        max_connections,
                        satisfied_connections,
                    },
                    conservative_pool: conservative,
                    aggressive_pool: aggressive,
                    weights,
                    reference_strategy: *reference_strategy,
                    conservative_strategy: *conservative_strategy,
                    aggressive_strategy: *aggressive_strategy,
                });
            }
        }
    }

    out
}

fn ratio_score(summary: &Summary, profile: GateProfile) -> (bool, usize, Vec<&'static str>, f64) {
    let mut failures = Vec::new();
    if summary.avg_success_rate < profile.min_success_rate {
        failures.push("avg_success");
    }
    if summary.avg_largest_component_share < profile.min_largest_component_share {
        failures.push("avg_largest_component");
    }
    if summary.avg_component_count > profile.max_component_count {
        failures.push("avg_components");
    }

    let mut failed_runs = 0usize;
    for idx in 0..summary.runs {
        if summary.run_success_rates[idx] < profile.min_success_rate
            || summary.run_largest_component_shares[idx] < profile.min_largest_component_share
            || summary.run_component_counts[idx] > profile.max_component_count
        {
            failed_runs += 1;
        }
    }
    if failed_runs > profile.max_failed_runs {
        failures.push("run_failures");
    }

    if !failures.is_empty() {
        return (false, failed_runs, failures, 0.0);
    }

    let good = summary.avg_success_rate.powf(3.0) * summary.avg_largest_component_share.powf(2.0);
    let bad = (1.0 + summary.avg_p95_ms / 50.0).ln()
        + 0.8 * (1.0 + summary.avg_overhead_ratio).ln()
        + 0.5 * (1.0 + summary.avg_tick_p95_us / 2000.0).ln()
        + 0.3 * (1.0 + summary.avg_peak_connection_pairs / 200.0).ln();
    let score = good / (1.0 + bad);
    (true, failed_runs, failures, score)
}

fn print_ranked_for_profile(profile: GateProfile, summaries: &[Summary]) {
    let mut scored: Vec<ScoredSummary> = summaries
        .iter()
        .map(|summary| {
            let (passes_gates, failed_runs, gate_failures, score) = ratio_score(summary, profile);
            ScoredSummary {
                summary: Summary {
                    label: summary.label.clone(),
                    reference_pool: summary.reference_pool,
                    conservative_pool: summary.conservative_pool,
                    aggressive_pool: summary.aggressive_pool,
                    weights: summary.weights,
                    runs: summary.runs,
                    avg_success_rate: summary.avg_success_rate,
                    avg_p95_ms: summary.avg_p95_ms,
                    avg_overhead_ratio: summary.avg_overhead_ratio,
                    avg_component_count: summary.avg_component_count,
                    avg_largest_component_share: summary.avg_largest_component_share,
                    avg_tick_p95_us: summary.avg_tick_p95_us,
                    avg_peak_connection_pairs: summary.avg_peak_connection_pairs,
                    run_success_rates: summary.run_success_rates.clone(),
                    run_largest_component_shares: summary.run_largest_component_shares.clone(),
                    run_component_counts: summary.run_component_counts.clone(),
                },
                passes_gates,
                failed_runs,
                gate_failures,
                score,
            }
        })
        .collect();

    scored.sort_by(|a, b| match (a.passes_gates, b.passes_gates) {
        (true, true) => b
            .score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => a.failed_runs.cmp(&b.failed_runs).then_with(|| {
            b.summary
                .avg_success_rate
                .partial_cmp(&a.summary.avg_success_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    });

    println!(
        "\nProfile: {} (accepted first, higher score is better)\nlabel                | ref  | cns  | agg  | w(r/c/a) | runs | success | p95_ms | overhead | components | largest_share | tick_p95_us | peak_links | fail_runs | gates | score",
        profile.name
    );
    for s in scored {
        let gate_str = if s.passes_gates {
            "pass".to_string()
        } else {
            format!("fail({})", s.gate_failures.join(","))
        };
        println!(
            "{:<20} | {:>2}/{:<2} | {:>2}/{:<2} | {:>2}/{:<2} | {:>2}/{:>2}/{:<2} | {:>4} | {:>6.2}% | {:>7.1} | {:>8.3} | {:>10.2} | {:>13.3} | {:>11.1} | {:>10.1} | {:>9} | {:>20} | {:>8.5}",
            s.summary.label,
            s.summary.reference_pool.max_connections,
            s.summary.reference_pool.satisfied_connections,
            s.summary.conservative_pool.max_connections,
            s.summary.conservative_pool.satisfied_connections,
            s.summary.aggressive_pool.max_connections,
            s.summary.aggressive_pool.satisfied_connections,
            s.summary.weights.0,
            s.summary.weights.1,
            s.summary.weights.2,
            s.summary.runs,
            s.summary.avg_success_rate * 100.0,
            s.summary.avg_p95_ms,
            s.summary.avg_overhead_ratio,
            s.summary.avg_component_count,
            s.summary.avg_largest_component_share,
            s.summary.avg_tick_p95_us,
            s.summary.avg_peak_connection_pairs,
            s.failed_runs,
            gate_str,
            s.score,
        );
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mode = parse_mode();
    let workload = parse_workload_profile();
    let mut runtime = workload_runtime(workload);
    if let Some(timeout_ms) = parse_timeout_override_ms() {
        runtime.retrieval_timeout_ms = timeout_ms;
    }
    let mut candidates = match mode {
        ExploreMode::Manual => build_manual_candidates(),
        ExploreMode::Auto => build_auto_candidates(),
    };
    let candidate_offset = parse_candidate_offset().min(candidates.len());
    if candidate_offset > 0 {
        candidates.drain(0..candidate_offset);
    }
    if let Some(limit) = parse_candidate_limit() {
        candidates.truncate(limit.min(candidates.len()));
    }
    let seeds: Vec<u64> = if let Some(seeds) = parse_seeds_override() {
        seeds
    } else {
        match (mode, workload) {
            (ExploreMode::Manual, WorkloadProfile::Standard) => vec![11, 22, 33, 44],
            (ExploreMode::Auto, WorkloadProfile::Standard) => vec![11, 22, 33],
            (ExploreMode::Manual, WorkloadProfile::HeavyMedia) => vec![11, 22],
            (ExploreMode::Auto, WorkloadProfile::HeavyMedia) => vec![11, 22],
        }
    };
    let seeds = if let Some(limit) = parse_seed_limit() {
        seeds.into_iter().take(limit).collect()
    } else {
        seeds
    };

    let mut configs = Vec::new();
    let mut config_candidate_idx = Vec::new();

    for (idx, candidate) in candidates.iter().enumerate() {
        for seed in &seeds {
            let (w_ref, w_cons, w_agg) = candidate.weights;
            configs.push(SimConfig {
                node_count: runtime.node_count,
                duration: Duration::from_secs(runtime.duration_secs),
                seed: *seed,
                // Fallback-only when strategy_mix is empty; keep aligned with reference.
                pool: candidate.reference_pool,
                discovery_interval_ms: 100,
                hello_reannounce_interval_ms: 400,
                churn_rate: 0.02,
                allow_rejoin: true,
                network_latency_ms: 30,
                retrieval_probe_count: runtime.retrieval_probe_count,
                retrieval_payload_bytes: runtime.retrieval_payload_bytes,
                retrieval_timeout_ms: runtime.retrieval_timeout_ms,
                max_events_retained: runtime.max_events_retained,
                retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
                retrieval_poll_interval_ms: 5,
                reference_strategy: Some("reference".to_string()),
                cashu_incentives: None,
                strategy_mix: vec![
                    NodeStrategyProfile {
                        name: "reference".to_string(),
                        weight: w_ref,
                        pool: candidate.reference_pool,
                        selection_strategy: candidate.reference_strategy.selection_strategy,
                        fairness_enabled: candidate.reference_strategy.fairness_enabled,
                        dispatch: candidate.reference_strategy.dispatch,
                        response_behavior: candidate.reference_strategy.response_behavior,
                    },
                    NodeStrategyProfile {
                        name: "conservative".to_string(),
                        weight: w_cons,
                        pool: candidate.conservative_pool,
                        selection_strategy: candidate.conservative_strategy.selection_strategy,
                        fairness_enabled: candidate.conservative_strategy.fairness_enabled,
                        dispatch: candidate.conservative_strategy.dispatch,
                        response_behavior: candidate.conservative_strategy.response_behavior,
                    },
                    NodeStrategyProfile {
                        name: "aggressive".to_string(),
                        weight: w_agg,
                        pool: candidate.aggressive_pool,
                        selection_strategy: candidate.aggressive_strategy.selection_strategy,
                        fairness_enabled: candidate.aggressive_strategy.fairness_enabled,
                        dispatch: candidate.aggressive_strategy.dispatch,
                        response_behavior: candidate.aggressive_strategy.response_behavior,
                    },
                    NodeStrategyProfile {
                        name: "goofball".to_string(),
                        weight: 20,
                        pool: candidate.conservative_pool,
                        selection_strategy: SelectionStrategy::RoundRobin,
                        fairness_enabled: true,
                        dispatch: RequestDispatchConfig::default(),
                        response_behavior: goofball_behavior(),
                    },
                    NodeStrategyProfile {
                        name: "adversarial".to_string(),
                        weight: 20,
                        pool: candidate.aggressive_pool,
                        selection_strategy: SelectionStrategy::Random,
                        fairness_enabled: true,
                        dispatch: RequestDispatchConfig::default(),
                        response_behavior: adversarial_behavior(),
                    },
                ],
            });
            config_candidate_idx.push(idx);
        }
    }

    println!(
        "Mode: {:?} | Workload: {:?} | timeout={}ms | Running {} configs ({} candidates x {} seeds, offset {})",
        mode,
        workload,
        runtime.retrieval_timeout_ms,
        configs.len(),
        candidates.len(),
        seeds.len(),
        candidate_offset
    );

    let results = run_parameter_sweep_with_progress(&configs).await;

    let mut per_candidate_runs: Vec<Vec<&hashtree_sim::SweepResult>> =
        vec![Vec::new(); candidates.len()];
    for (result, candidate_idx) in results.iter().zip(config_candidate_idx.iter().copied()) {
        per_candidate_runs[candidate_idx].push(result);
    }

    let mut summaries: Vec<Summary> = Vec::new();
    for (candidate, runs) in candidates.iter().zip(per_candidate_runs.into_iter()) {
        if runs.is_empty() {
            continue;
        }

        let mut success_sum = 0.0;
        let mut p95_sum = 0.0;
        let mut overhead_sum = 0.0;
        let mut component_sum = 0.0;
        let mut largest_share_sum = 0.0;
        let mut tick_p95_sum = 0.0;
        let mut peak_links_sum = 0.0;
        let mut run_success_rates = Vec::new();
        let mut run_largest_component_shares = Vec::new();
        let mut run_component_counts = Vec::new();

        for result in &runs {
            let reference = result
                .stats
                .strategy_retrieval
                .get("reference")
                .unwrap_or(&result.stats.retrieval);

            success_sum += reference.success_rate;
            p95_sum += reference.p95_latency_ms as f64;
            let overhead = if reference.payload_bytes == 0 {
                0.0
            } else {
                reference.data_plane_bytes as f64 / reference.payload_bytes as f64
            };
            overhead_sum += overhead;

            let component_count = result.final_topology.component_count as f64;
            let largest_share = if result.final_topology.node_count == 0 {
                0.0
            } else {
                result.final_topology.largest_component as f64
                    / result.final_topology.node_count as f64
            };

            component_sum += component_count;
            largest_share_sum += largest_share;
            tick_p95_sum += result.stats.local_resources.tick_p95_us as f64;
            peak_links_sum += result.stats.local_resources.peak_connection_pairs as f64;

            run_success_rates.push(reference.success_rate);
            run_largest_component_shares.push(largest_share);
            run_component_counts.push(component_count);
        }

        let n = runs.len() as f64;
        summaries.push(Summary {
            label: candidate.label.clone(),
            reference_pool: candidate.reference_pool,
            conservative_pool: candidate.conservative_pool,
            aggressive_pool: candidate.aggressive_pool,
            weights: candidate.weights,
            runs: runs.len(),
            avg_success_rate: success_sum / n,
            avg_p95_ms: p95_sum / n,
            avg_overhead_ratio: overhead_sum / n,
            avg_component_count: component_sum / n,
            avg_largest_component_share: largest_share_sum / n,
            avg_tick_p95_us: tick_p95_sum / n,
            avg_peak_connection_pairs: peak_links_sum / n,
            run_success_rates,
            run_largest_component_shares,
            run_component_counts,
        });
    }

    let exploration = GateProfile {
        name: "exploration",
        min_success_rate: 0.70,
        min_largest_component_share: 0.80,
        max_component_count: 3.0,
        max_failed_runs: 1,
    };
    let promotion = GateProfile {
        name: "promotion",
        min_success_rate: 0.90,
        min_largest_component_share: 0.95,
        max_component_count: 2.0,
        max_failed_runs: 0,
    };

    print_ranked_for_profile(exploration, &summaries);
    print_ranked_for_profile(promotion, &summaries);
}
