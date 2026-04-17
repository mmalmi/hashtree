use super::*;

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

#[tokio::test]
async fn test_mesh_sim_small() {
    let config = SimConfig {
        node_count: 10,
        duration: Duration::from_secs(2),
        seed: 42,
        pool: PoolConfig {
            max_connections: 5,
            satisfied_connections: 3,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 0,
        retrieval_payload_bytes: 1024,
        retrieval_timeout_ms: 1500,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
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
async fn test_mesh_sim_with_churn() {
    let config = SimConfig {
        node_count: 20,
        duration: Duration::from_secs(3),
        seed: 123,
        pool: PoolConfig {
            max_connections: 5,
            satisfied_connections: 3,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.05,
        allow_rejoin: true,
        network_latency_ms: 0,
        retrieval_probe_count: 0,
        retrieval_payload_bytes: 1024,
        retrieval_timeout_ms: 1500,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
    };

    let sim = Simulation::new(config);
    sim.run().await;

    let stats = sim.analyze_topology().await;
    let sim_stats = sim.get_stats().await;

    println!("\nSimulation with churn:");
    Simulation::print_topology_stats(&stats);
    Simulation::print_sim_stats(&sim_stats);

    assert!(
        sim_stats.total_joins >= 20,
        "Should have at least initial joins"
    );
    assert!(
        sim_stats.total_connections_formed > 0,
        "Should record formed connections"
    );
}

#[tokio::test]
async fn test_mesh_sim_1000_nodes_connectivity() {
    let config = SimConfig {
        node_count: 1000,
        duration: Duration::from_secs(8),
        seed: 42,
        pool: PoolConfig {
            max_connections: 24,
            satisfied_connections: 12,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 30,
        retrieval_probe_count: 0,
        retrieval_payload_bytes: 1024,
        retrieval_timeout_ms: 1500,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
    };

    let sim = Simulation::new(config);
    sim.run().await;

    let stats = sim.analyze_topology().await;

    println!("\n=== 1000 Node Connectivity Test (12/24 pool) ===");
    Simulation::print_topology_stats(&stats);

    assert_eq!(stats.node_count, 1000, "Should have 1000 nodes");
    assert!(stats.connection_count > 0, "Should have connections");
    assert!(
        stats.largest_component >= 300,
        "Largest component should cover at least 300/1000 nodes, got {}",
        stats.largest_component
    );
    assert!(
        stats.connection_count >= 6_500,
        "Expected at least 6500 connections, got {}",
        stats.connection_count
    );
}

#[tokio::test]
async fn test_mesh_sim_collects_retrieval_probe_metrics() {
    let config = SimConfig {
        node_count: 12,
        duration: Duration::from_secs(4),
        seed: 7,
        pool: PoolConfig {
            max_connections: 8,
            satisfied_connections: 4,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 16,
        retrieval_payload_bytes: 512,
        retrieval_timeout_ms: 1200,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
    };

    let sim = Simulation::new(config);
    sim.run().await;

    let sim_stats = sim.get_stats().await;
    assert_eq!(sim_stats.retrieval.probes, 16);
    assert!(
        sim_stats.retrieval.successes > 0,
        "expected at least one successful retrieval probe"
    );
    assert_eq!(
        sim_stats.retrieval.failures + sim_stats.retrieval.successes,
        sim_stats.retrieval.probes
    );
    assert!(
        sim_stats.retrieval.p95_latency_ms >= sim_stats.retrieval.p50_latency_ms,
        "latency percentiles should be monotonic"
    );
}

#[tokio::test]
async fn test_mesh_sim_report_json_contains_objectives() {
    let config = SimConfig {
        node_count: 8,
        duration: Duration::from_secs(2),
        seed: 9,
        pool: PoolConfig {
            max_connections: 5,
            satisfied_connections: 3,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 6,
        retrieval_payload_bytes: 256,
        retrieval_timeout_ms: 1000,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
    };
    let sim = Simulation::new(config);
    sim.run().await;

    let report = sim.report_json().await;
    assert_eq!(report["config"]["retrieval_probe_count"].as_u64(), Some(6));
    assert_eq!(report["stats"]["retrieval"]["probes"].as_u64(), Some(6));
    assert!(report["objectives"]["retrieval_p95_latency_ms"].is_number());
    assert!(report["objectives"]["overhead_ratio_data_to_payload"].is_number());
    assert!(report["objectives"]["local_cpu_tick_p95_us"].is_number());
    assert!(report["objectives"]["local_mem_peak_event_log_entries"].is_number());
    assert!(report["stats"]["local_resources"]["tick_p95_us"].is_number());
}

#[tokio::test]
async fn test_mesh_sim_cashu_incentives_use_local_test_mint() {
    let config = SimConfig {
        node_count: 16,
        duration: Duration::from_secs(3),
        seed: 88,
        pool: PoolConfig {
            max_connections: 8,
            satisfied_connections: 4,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 12,
        retrieval_payload_bytes: 256,
        retrieval_timeout_ms: 1000,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: Some(CashuIncentiveConfig {
            enabled: true,
            channel_capacity_sat: 128,
            payment_per_probe_sat: 2,
            selection_bonus_weight: 0.8,
            payment_default_block_threshold: 0,
        }),
    };

    let sim = Simulation::new(config);
    sim.run().await;
    let stats = sim.get_stats().await;

    assert_eq!(stats.retrieval.probes, 12);
    assert!(
        stats.retrieval.successes > 0,
        "cashu incentives should not prevent retrieval"
    );
    assert!(
        stats.cashu.channels_opened > 0,
        "expected channels to open in local test mint"
    );
    assert!(
        stats.cashu.payments_sent > 0,
        "expected micropayments via local test mint"
    );
    assert!(
        stats.cashu.priority_credits_applied > 0,
        "expected peer priority credits to be applied"
    );
    assert!(
        stats.cashu.quote_requests_sent > 0,
        "expected paid retrievals to negotiate quotes before delivery"
    );
    assert!(
        stats.cashu.quote_responses_received > 0,
        "expected peers to answer quote requests when they can serve"
    );
    assert!(
        stats.cashu.quoted_retrieval_attempts > 0,
        "expected the requester to attempt retrieval with an accepted quote"
    );
    assert!(
        stats.cashu.payments_sent <= stats.retrieval.successes as u64,
        "post-delivery payments must not exceed successful deliveries"
    );
    assert_eq!(
        stats.cashu.priority_volume_sat,
        stats.cashu.priority_credits_applied * 2
    );
    assert_eq!(
        stats.cashu.settlements_finalized,
        stats.cashu.channels_opened
    );
}

#[tokio::test]
async fn test_mesh_sim_accepts_injected_mint_client() {
    let config = SimConfig {
        node_count: 12,
        duration: Duration::from_secs(3),
        seed: 188,
        pool: PoolConfig {
            max_connections: 8,
            satisfied_connections: 4,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 12,
        retrieval_payload_bytes: 128,
        retrieval_timeout_ms: 1000,
        max_events_retained: 10_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: Some(CashuIncentiveConfig {
            enabled: true,
            channel_capacity_sat: 64,
            payment_per_probe_sat: 2,
            selection_bonus_weight: 0.8,
            payment_default_block_threshold: 0,
        }),
    };

    let mint = Arc::new(LocalMintClient::new());
    let sim = Simulation::new_with_mint_client(config, mint.clone());
    sim.run().await;

    let stats = sim.get_stats().await;
    let mint_stats = mint.stats().await.expect("mint stats");

    assert!(
        mint_stats.channels_opened > 0,
        "expected injected mint client to receive channel opens"
    );
    assert_eq!(mint_stats.payments_sent, stats.cashu.payments_sent);
    assert_eq!(mint_stats.volume_sat, stats.cashu.volume_sat);
    assert_eq!(
        mint_stats.settlements_finalized,
        stats.cashu.settlements_finalized
    );
}

#[tokio::test]
async fn test_cashu_post_delivery_payment_failure_records_default_in_peer_metadata() {
    hashtree_network::clear_channel_registry().await;
    let config = SimConfig {
        node_count: 2,
        duration: Duration::from_secs(2),
        seed: 17,
        pool: PoolConfig {
            max_connections: 1,
            satisfied_connections: 1,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 250,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 10,
        retrieval_payload_bytes: 64,
        retrieval_timeout_ms: 500,
        max_events_retained: 1_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: Some(CashuIncentiveConfig {
            enabled: true,
            channel_capacity_sat: 1,
            payment_per_probe_sat: 2,
            selection_bonus_weight: 0.8,
            payment_default_block_threshold: 1,
        }),
    };

    let sim = Simulation::new(config);
    sim.spawn_node(0).await;
    sim.spawn_node(0).await;

    let (payer_id, payee_id, payer_store, payee_store) = {
        let nodes = sim.nodes.read().await;
        let mut ids: Vec<_> = nodes.keys().cloned().collect();
        ids.sort();
        let payer_id = ids[0].clone();
        let payee_id = ids[1].clone();
        let payer_store = nodes.get(&payer_id).expect("payer node").store.clone();
        let payee_store = nodes.get(&payee_id).expect("payee node").store.clone();
        (payer_id, payee_id, payer_store, payee_store)
    };

    sim.settle_cashu_delivery_payment(&payer_id, &payee_id, payer_store, payee_store.clone())
        .await;
    sim.finalize_cashu_stats().await;

    let stats = sim.get_stats().await;
    assert!(
        stats.cashu.payments_failed > 0,
        "expected failed post-delivery settlements when capacity < payment"
    );
    assert!(
        stats.cashu.payment_defaults_recorded > 0,
        "provider should record non-paying peers in metadata"
    );

    let snapshot = payee_store.peer_metadata_snapshot().await;
    let payer_meta = snapshot
        .peers
        .iter()
        .find(|peer| peer.principal == payer_id)
        .expect("payer metadata");
    assert_eq!(payer_meta.cashu_payment_defaults, 1);
    hashtree_network::clear_channel_registry().await;
}

#[tokio::test]
async fn test_mesh_sim_strategy_mix_reports_reference_metrics() {
    let config = SimConfig {
        node_count: 30,
        duration: Duration::from_secs(2),
        seed: 99,
        pool: PoolConfig {
            max_connections: 6,
            satisfied_connections: 3,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 8,
        retrieval_payload_bytes: 256,
        retrieval_timeout_ms: 900,
        max_events_retained: 10_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: vec![
            NodeStrategyProfile {
                name: "reference".to_string(),
                weight: 1,
                pool: PoolConfig {
                    max_connections: 10,
                    satisfied_connections: 5,
                },
                selection_strategy: SelectionStrategy::Weighted,
                fairness_enabled: true,
                dispatch: RequestDispatchConfig::default(),
                response_behavior: ResponseBehaviorConfig::default(),
            },
            NodeStrategyProfile {
                name: "other".to_string(),
                weight: 1,
                pool: PoolConfig {
                    max_connections: 4,
                    satisfied_connections: 2,
                },
                selection_strategy: SelectionStrategy::Weighted,
                fairness_enabled: true,
                dispatch: RequestDispatchConfig::default(),
                response_behavior: ResponseBehaviorConfig::default(),
            },
        ],
        reference_strategy: Some("reference".to_string()),
        cashu_incentives: None,
    };

    let sim = Simulation::new(config);
    sim.run().await;

    let stats = sim.get_stats().await;
    assert!(stats.strategy_joins.get("reference").copied().unwrap_or(0) > 0);
    assert!(stats.strategy_joins.get("other").copied().unwrap_or(0) > 0);
    assert!(
        stats
            .strategy_retrieval
            .get("reference")
            .map(|s| s.probes)
            .unwrap_or(0)
            > 0
    );

    let report = sim.report_json().await;
    assert!(report["stats"]["strategy_retrieval"]["reference"]["success_rate"].is_number());
    assert!(report["objectives"]["reference_success_rate"].is_number());
}

fn mixed_bad_actor_config(seed: u64, reference_selection: SelectionStrategy) -> SimConfig {
    let reference_dispatch = RequestDispatchConfig {
        initial_fanout: 1,
        hedge_fanout: 1,
        max_fanout: 4,
        hedge_interval_ms: 8,
    };

    SimConfig {
        node_count: 80,
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
        retrieval_probe_count: 24,
        retrieval_payload_bytes: 1024,
        retrieval_timeout_ms: 700,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: vec![
            NodeStrategyProfile {
                name: "reference".to_string(),
                weight: 60,
                pool: PoolConfig {
                    max_connections: 18,
                    satisfied_connections: 9,
                },
                selection_strategy: reference_selection,
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
                response_behavior: goofball_behavior(),
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
                response_behavior: adversarial_behavior(),
            },
        ],
        reference_strategy: Some("reference".to_string()),
        cashu_incentives: None,
    }
}

fn reference_success_rate(stats: &SimStats) -> f64 {
    stats
        .strategy_retrieval
        .get("reference")
        .expect("reference retrieval stats missing")
        .success_rate
}

#[tokio::test]
async fn test_mesh_sim_goofballs_reduce_reference_success() {
    let honest_config = SimConfig {
        node_count: 80,
        duration: Duration::from_secs(5),
        seed: 1234,
        pool: PoolConfig {
            max_connections: 16,
            satisfied_connections: 8,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 400,
        churn_rate: 0.02,
        allow_rejoin: true,
        network_latency_ms: 30,
        retrieval_probe_count: 24,
        retrieval_payload_bytes: 1024,
        retrieval_timeout_ms: 700,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: vec![NodeStrategyProfile {
            name: "reference".to_string(),
            weight: 1,
            pool: PoolConfig {
                max_connections: 18,
                satisfied_connections: 9,
            },
            selection_strategy: SelectionStrategy::Weighted,
            fairness_enabled: true,
            dispatch: RequestDispatchConfig::default(),
            response_behavior: ResponseBehaviorConfig::default(),
        }],
        reference_strategy: Some("reference".to_string()),
        cashu_incentives: None,
    };

    let mixed_config = mixed_bad_actor_config(honest_config.seed, SelectionStrategy::TitForTat);

    let honest = Simulation::new(honest_config);
    honest.run().await;
    let honest_stats = honest.get_stats().await;
    let honest_ref = reference_success_rate(&honest_stats);

    let mixed = Simulation::new(mixed_config);
    mixed.run().await;
    let mixed_stats = mixed.get_stats().await;
    let mixed_ref = reference_success_rate(&mixed_stats);

    assert!(
            mixed_ref < honest_ref,
            "expected mixed goofball/adversarial network to reduce reference success (honest={:.3}, mixed={:.3})",
            honest_ref,
            mixed_ref
        );
}

#[tokio::test]
async fn test_mesh_sim_report_json_includes_extended_response_behavior_fields() {
    let config = SimConfig {
        node_count: 6,
        duration: Duration::from_secs(1),
        seed: 909,
        pool: PoolConfig {
            max_connections: 4,
            satisfied_connections: 2,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 200,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 10,
        retrieval_probe_count: 0,
        retrieval_payload_bytes: 512,
        retrieval_timeout_ms: 500,
        max_events_retained: 128,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        reference_strategy: Some("reference".to_string()),
        cashu_incentives: None,
        strategy_mix: vec![NodeStrategyProfile {
            name: "reference".to_string(),
            weight: 1,
            pool: PoolConfig {
                max_connections: 4,
                satisfied_connections: 2,
            },
            selection_strategy: SelectionStrategy::TitForTat,
            fairness_enabled: true,
            dispatch: RequestDispatchConfig::default(),
            response_behavior: ResponseBehaviorConfig {
                extra_delay_ms: 11,
                first_byte_delay_ms: 22,
                bytes_per_second: 33_000,
                stall_response_prob: 0.25,
                stall_delay_ms: 44,
                ..Default::default()
            },
        }],
    };

    let sim = Simulation::new(config);
    sim.run().await;
    let report = sim.report_json().await;
    let strategies = report["config"]["strategy_mix"]
        .as_array()
        .expect("strategy mix array");
    let reference = strategies
        .iter()
        .find(|entry| entry["name"] == "reference")
        .expect("reference strategy");
    let behavior = &reference["response_behavior"];

    assert_eq!(behavior["extra_delay_ms"].as_u64(), Some(11));
    assert_eq!(behavior["first_byte_delay_ms"].as_u64(), Some(22));
    assert_eq!(behavior["bytes_per_second"].as_u64(), Some(33_000));
    assert_eq!(behavior["stall_response_prob"].as_f64(), Some(0.25));
    assert_eq!(behavior["stall_delay_ms"].as_u64(), Some(44));
}

#[tokio::test]
async fn test_mesh_sim_caps_event_log_for_memory() {
    let config = SimConfig {
        node_count: 30,
        duration: Duration::from_secs(3),
        seed: 77,
        pool: PoolConfig {
            max_connections: 6,
            satisfied_connections: 3,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.10,
        allow_rejoin: true,
        network_latency_ms: 0,
        retrieval_probe_count: 0,
        retrieval_payload_bytes: 256,
        retrieval_timeout_ms: 700,
        max_events_retained: 8,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
    };
    let sim = Simulation::new(config);
    sim.run().await;
    let stats = sim.get_stats().await;
    assert!(
        stats.events.len() <= 8,
        "event log should be capped, got {} entries",
        stats.events.len()
    );
    assert!(stats.local_resources.peak_event_log_entries <= 8);
}

#[tokio::test]
async fn test_run_parameter_sweep_returns_per_config_results() {
    let configs = vec![
        SimConfig {
            node_count: 6,
            duration: Duration::from_secs(1),
            seed: 1,
            pool: PoolConfig {
                max_connections: 4,
                satisfied_connections: 2,
            },
            discovery_interval_ms: 100,
            hello_reannounce_interval_ms: 1000,
            churn_rate: 0.0,
            allow_rejoin: false,
            network_latency_ms: 0,
            retrieval_probe_count: 0,
            retrieval_payload_bytes: 128,
            retrieval_timeout_ms: 1000,
            max_events_retained: 20_000,
            retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
            retrieval_poll_interval_ms: 5,
            strategy_mix: Vec::new(),
            reference_strategy: None,
            cashu_incentives: None,
        },
        SimConfig {
            node_count: 6,
            duration: Duration::from_secs(1),
            seed: 2,
            pool: PoolConfig {
                max_connections: 4,
                satisfied_connections: 2,
            },
            discovery_interval_ms: 100,
            hello_reannounce_interval_ms: 1000,
            churn_rate: 0.0,
            allow_rejoin: false,
            network_latency_ms: 0,
            retrieval_probe_count: 0,
            retrieval_payload_bytes: 128,
            retrieval_timeout_ms: 1000,
            max_events_retained: 20_000,
            retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
            retrieval_poll_interval_ms: 5,
            strategy_mix: Vec::new(),
            reference_strategy: None,
            cashu_incentives: None,
        },
    ];

    let results = run_parameter_sweep(&configs).await;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].config.seed, 1);
    assert_eq!(results[1].config.seed, 2);
}

#[tokio::test]
async fn test_mesh_sim_virtual_timing_reflects_network_latency() {
    let base = SimConfig {
        node_count: 36,
        duration: Duration::from_secs(3),
        seed: 5,
        pool: PoolConfig {
            max_connections: 14,
            satisfied_connections: 7,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.0,
        allow_rejoin: false,
        network_latency_ms: 0,
        retrieval_probe_count: 16,
        retrieval_payload_bytes: 1024,
        retrieval_timeout_ms: 1200,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
    };

    let mut low_latency_cfg = base.clone();
    low_latency_cfg.network_latency_ms = 15;
    let low_sim = Simulation::new(low_latency_cfg);
    low_sim.run().await;
    let low_stats = low_sim.get_stats().await;

    let mut high_latency_cfg = base;
    high_latency_cfg.network_latency_ms = 300;
    let high_sim = Simulation::new(high_latency_cfg);
    high_sim.run().await;
    let high_stats = high_sim.get_stats().await;

    assert!(
            high_stats.retrieval.p95_latency_ms > low_stats.retrieval.p95_latency_ms,
            "virtual timing should still reflect higher configured latency (low p95={}ms, high p95={}ms)",
            low_stats.retrieval.p95_latency_ms,
            high_stats.retrieval.p95_latency_ms
        );
}

#[tokio::test]
async fn test_mesh_sim_short_timeout_retrieval_success_floor() {
    // Regression guard for low retrieval success when timeouts are shorter
    // than sequential per-peer probing.
    let config = SimConfig {
        node_count: 60,
        duration: Duration::from_secs(3),
        seed: 22,
        pool: PoolConfig {
            max_connections: 16,
            satisfied_connections: 8,
        },
        discovery_interval_ms: 100,
        hello_reannounce_interval_ms: 1000,
        churn_rate: 0.02,
        allow_rejoin: true,
        network_latency_ms: 30,
        retrieval_probe_count: 20,
        retrieval_payload_bytes: 2048,
        retrieval_timeout_ms: 700,
        max_events_retained: 20_000,
        retrieval_timing_mode: RetrievalTimingMode::VirtualSteps,
        retrieval_poll_interval_ms: 5,
        strategy_mix: Vec::new(),
        reference_strategy: None,
        cashu_incentives: None,
    };

    let sim = Simulation::new(config);
    sim.run().await;
    let stats = sim.get_stats().await;
    assert!(
        stats.retrieval.success_rate >= 0.50,
        "retrieval success rate too low: {:.2}%",
        stats.retrieval.success_rate * 100.0
    );
}
