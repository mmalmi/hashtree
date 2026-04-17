# hashtree-sim

P2P network simulation for hashtree, testing routing strategies and network behavior.

## Recommended: mesh_sim

The `mesh_sim` module uses the **exact same code** as the production `MeshStore`,
just with mock transports. This is the recommended approach for testing:

```rust
use hashtree_sim::mesh_sim::{Simulation, SimConfig};

let config = SimConfig {
    node_count: 100,
    pool: PoolConfig { max_connections: 16, satisfied_connections: 8 },
    retrieval_probe_count: 200,
    retrieval_payload_bytes: 4096,
    retrieval_timeout_ms: 1500,
    max_events_retained: 20_000,
    ..Default::default()
};

let sim = Simulation::new(config);
sim.run().await;
let report = sim.report_json().await;
println!("{}", serde_json::to_string_pretty(&report).unwrap());
```

## Shared Code with Production

The simulation uses the same shared router/store core as the default production mesh stack:

```rust
// Uses hashtree_network::PoolConfig - same defaults as the production mesh wrapper
let config = SimConfig {
    pool: PoolConfig::default(),  // max_connections: 16, satisfied_connections: 8
    ..Default::default()
};
```

This ensures simulation behavior matches production as closely as possible.

## Parameter Sweeps

Use `run_parameter_sweep` to compare protocol settings across seeds or policies:

```rust
use hashtree_sim::{run_parameter_sweep, SimConfig, PoolConfig};

let configs = vec![
    SimConfig {
        seed: 1,
        pool: PoolConfig { max_connections: 8, satisfied_connections: 4 },
        retrieval_probe_count: 100,
        ..Default::default()
    },
    SimConfig {
        seed: 2,
        pool: PoolConfig { max_connections: 12, satisfied_connections: 6 },
        retrieval_probe_count: 100,
        ..Default::default()
    },
];

let results = run_parameter_sweep(&configs).await;
for result in results {
    println!(
        "seed={} success_rate={:.2}% p95={}ms components={} local_tick_p95_us={} peak_links={}",
        result.config.seed,
        result.stats.retrieval.success_rate * 100.0,
        result.stats.retrieval.p95_latency_ms,
        result.final_topology.component_count,
        result.stats.local_resources.tick_p95_us,
        result.stats.local_resources.peak_connection_pairs
    );
}
```

### Local Resource Objectives

`report_json()` now includes local efficiency objectives so sweeps can optimize beyond retrieval quality:
- `local_cpu_tick_p95_us`
- `local_cpu_run_wall_ms`
- `local_mem_peak_event_log_entries`
- `local_mem_peak_connection_pairs`
- `reference_success_rate` / `reference_p95_latency_ms` / `reference_failure_rate`

### Mixed Strategy Simulation

`SimConfig` supports heterogeneous node behavior via `strategy_mix` and `reference_strategy`.
This lets sweeps evaluate one candidate strategy inside a network of mixed incentives.

```rust
use hashtree_sim::{
    NodeStrategyProfile, PoolConfig, RequestDispatchConfig, SelectionStrategy, SimConfig,
};

let config = SimConfig {
    reference_strategy: Some("reference".to_string()),
    hello_reannounce_interval_ms: 1000,
    strategy_mix: vec![
        NodeStrategyProfile {
            name: "reference".to_string(),
            weight: 35,
            pool: PoolConfig { max_connections: 18, satisfied_connections: 9 },
            selection_strategy: SelectionStrategy::UtilityUcb,
            fairness_enabled: true,
            dispatch: RequestDispatchConfig { initial_fanout: 2, hedge_fanout: 2, max_fanout: usize::MAX, hedge_interval_ms: 5 },
        },
        NodeStrategyProfile {
            name: "aggressive".to_string(),
            weight: 30,
            pool: PoolConfig { max_connections: 24, satisfied_connections: 12 },
            selection_strategy: SelectionStrategy::Weighted,
            fairness_enabled: true,
            dispatch: RequestDispatchConfig::default(),
        },
        NodeStrategyProfile {
            name: "conservative".to_string(),
            weight: 35,
            pool: PoolConfig { max_connections: 12, satisfied_connections: 6 },
            selection_strategy: SelectionStrategy::HighestSuccessRate,
            fairness_enabled: true,
            dispatch: RequestDispatchConfig { initial_fanout: 1, hedge_fanout: 1, max_fanout: usize::MAX, hedge_interval_ms: 8 },
        },
    ],
    ..Default::default()
};
```

The tuning example evaluates two gate profiles:
- `exploration`: coarse filter for short/fast sweeps
- `promotion`: stricter thresholds for candidates considered production-ready
- both profiles hard-gate on per-run failures (not just averages)
- use `--mode=manual` for fixed candidate lists and `--mode=auto` for generated strategy/weight grids

## Cashu Incentive Simulation

`SimConfig.cashu_incentives` enables a local test mint that models:
- channel open (`payer -> payee`) with fixed capacity
- many offchain micropayments inside the channel
- final settlement at simulation end

When enabled, paid retrievals first negotiate an expiring quote with candidate peers,
then send the follow-up data request with the accepted quote ID. Successful delivery
attempts a post-delivery micropayment. Successful settlement adds payment credit to
future peer selection; failed settlement records requester defaults in peer metadata
and can optionally block future service after a configured threshold. If disabled
(or unset), routing stays reputation-only.

`Simulation::new(...)` still uses the in-process local mint by default. For integration
tests or future real-mint plumbing, `Simulation::new_with_mint_client(...)` accepts any
custom `MintClient` implementation.

### Timing Modes

`SimConfig.retrieval_timing_mode` controls probe timeout behavior:
- `WallClock`: uses real timeout progression
- `VirtualSteps`: uses simulated step budgets for faster runs

In `VirtualSteps`, the simulator pauses Tokio time for the duration of the run.
That means transport-layer `tokio::time::sleep(...)` calls in shared network code
advance on the simulated clock instead of wall clock, so response delay models
(first-byte delay, throughput, stalls) stay realistic without turning sweeps into
multi-minute real-time waits.

Latency simulation is still reflected in ordering and timeout outcomes:
- link/message delay is stepped via the simulator poll loop
- transport sleeps follow the paused Tokio clock
- wall-clock runtime stays short even for heavier payload workloads

For standalone binaries/examples, use a current-thread Tokio runtime if you want
paused simulated time (`#[tokio::main(flavor = "current_thread")]`).

## Network Connectivity

### The Component Problem

In P2P networks, nodes may form disconnected "components" (islands) if there aren't enough connections. A fully connected network has exactly 1 component.

`k > ln(N)` is only a rough lower bound for random graphs. Real protocol dynamics
(join timing, pool limits, signaling collisions, churn) need significantly more headroom.

Current simulation defaults are not guaranteed to produce a single connected component at 1000 nodes.
Use `largest_component` and `component_count` as first-class tuning objectives, not just degree targets.

`max_connections` and periodic hello re-announcement are the strongest topology controls today.

### Discovery with Perfect Negotiation

Nodes discover each other via Hello messages on a mock relay. We use the shared
concurrent-offer negotiation pattern:

1. When a node sees a Hello and NEEDS more peers (below `satisfied_connections`), it sends an offer
2. Both peers may send offers simultaneously - this is expected, not an error
3. On collision (both sent offers), the **"polite" peer** (lower ID) backs off and accepts the incoming offer
4. The **"impolite" peer** (higher ID) ignores the incoming offer and waits for their answer
5. Nodes periodically re-announce Hello (`hello_reannounce_interval_ms`) so late joiners can still discover already-satisfied peers.

```rust
// Polite peer backs off on collision
fn is_polite_peer(local_id: &str, remote_id: &str) -> bool {
    local_id < remote_id  // Lower ID is polite
}
```

**Why perfect negotiation?** With simple tie-breaking, if peer A is "satisfied" and peer B needs connections, B might not be able to connect if A was supposed to initiate. Perfect negotiation solves this: B sends an offer, A accepts it (since A can still accept up to `max_connections`).

## Routing Strategies

### Flood-All
- Sends requests to all connected peers immediately.
- Highest success and lowest tail latency in sparse/uncertain neighborhoods.
- More data-plane overhead.

### Hedged Fanout
- Sends a small initial fanout, then expands in timed waves until response/timeout.
- Uses peer ordering (`SelectionStrategy`) to try likely-good peers first.
- Lower overhead in favorable topologies; can raise p95 latency if too conservative.

### Utility-UCB Ordering
- Score combines good/bad outcome ratio, RTT, and bytes efficiency.
- Adds exploration bonus for less-sampled peers (bandit/UCB style).
- Helps avoid local optima where one historically-good peer monopolizes traffic.

## Latency Simulation

Per-link latency is configurable:
- `network_latency_ms`: Mean latency for the simulated direct-link transport

In `VirtualSteps`, latency still affects ordering/outcomes but wall-clock runtime is compressed.

## Multi-Hop Forwarding (HTL)

Requests include a **Hops-To-Live** counter (like Freenet):
- Starts at MAX_HTL (10)
- Decremented at each hop (with probabilistic variation per-peer)
- When HTL=0, request is not forwarded further
- Prevents infinite loops and limits network load

## Running Simulations

```bash
# Manual candidate sweep
cargo run -p hashtree-sim --example tune_webrtc_params -- --mode=manual

# Auto-generated candidate grid
cargo run -p hashtree-sim --example tune_webrtc_params -- --mode=auto

# Heavy-media workload preset (larger payloads, longer timeout)
cargo run -p hashtree-sim --example tune_webrtc_params -- --mode=manual --heavy-media

# Focus on a subset of manual candidates
cargo run -p hashtree-sim --example tune_webrtc_params -- --mode=manual \
  --heavy-media --candidate-offset=8 --candidate-limit=4

# Override timeout or seeds for focused comparisons
cargo run -p hashtree-sim --example tune_webrtc_params -- --mode=manual \
  --heavy-media --timeout-ms=2600 --seeds=11,22,33,44

# Progress prints once per config; redirect stdout if you want just the final tables in a file
cargo run -p hashtree-sim --example tune_webrtc_params -- --mode=manual \
  --heavy-media > /tmp/hashtree-sim-heavy.txt

# 1000-node connectivity/scalability test
cargo test -p hashtree-sim mesh_sim::tests::test_mesh_sim_1000_nodes_connectivity -- --nocapture
```

Progress notes and experiment outcomes are tracked in:
`/Users/sirius/src/hashtree/docs/webrtc-strategy-observations.md`

## Key Learnings

1. **max_peers matters**: Too low (< ln(N)) causes network fragmentation
2. **Hedged fanout can reduce overhead**, but max-fanout/timing must be tuned to avoid success regressions
3. **Latency variation** is important - uniform latency is unrealistic
4. **Multi-hop forwarding** dramatically increases reach but adds latency
5. **Perfect negotiation + periodic Hello beats one-shot discovery**: late joiners need periodic discovery refresh, otherwise large runs fragment.
6. **Use same code for simulation**: Using the exact same signaling/router/store core as production ensures simulation behavior matches reality. The `mesh_sim` module does this.
