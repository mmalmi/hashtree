# hashtree-sim

P2P network simulation for hashtree, testing routing strategies and network behavior.

## Recommended: webrtc_sim

The `webrtc_sim` module uses the **exact same code** as production WebRTCStore,
just with mock transports. This is the recommended approach for testing:

```rust
use hashtree_sim::webrtc_sim::{Simulation, SimConfig};

let config = SimConfig {
    node_count: 100,
    pool: PoolConfig { max_connections: 10, satisfied_connections: 5 },
    ..Default::default()
};

let sim = Simulation::new(config);
sim.run().await;
```

## Shared Code with Production

The simulation uses the **same types and defaults as production WebRTC**:

```rust
// Uses hashtree_webrtc::PoolConfig - same defaults as real WebRTC
let config = SimConfig {
    pool: PoolConfig::default(),  // max_connections: 10, satisfied_connections: 5
    ..Default::default()
};
```

This ensures simulation behavior matches production as closely as possible.

## Network Connectivity

### The Component Problem

In P2P networks, nodes may form disconnected "components" (islands) if there aren't enough connections. A fully connected network has exactly 1 component.

**Graph theory**: For N nodes with k connections each, connectivity requires `k > ln(N)`:

| Nodes | ln(N) | Real WebRTC default (10) |
|-------|-------|--------------------------|
| 50    | 3.9   | Well connected           |
| 100   | 4.6   | Well connected           |
| 200   | 5.3   | Well connected           |
| 1000  | 6.9   | Well connected           |
| 10000 | 9.2   | Edge case                |

The default `max_connections: 10` works well for networks up to ~10K nodes.

### Discovery with Perfect Negotiation

Nodes discover each other via Hello messages on a mock relay. We use the WebRTC **"perfect negotiation"** pattern:

1. When a node sees a Hello and NEEDS more peers (below `satisfied_connections`), it sends an offer
2. Both peers may send offers simultaneously - this is expected, not an error
3. On collision (both sent offers), the **"polite" peer** (lower ID) backs off and accepts the incoming offer
4. The **"impolite" peer** (higher ID) ignores the incoming offer and waits for their answer

```rust
// Polite peer backs off on collision
fn is_polite_peer(local_id: &str, remote_id: &str) -> bool {
    local_id < remote_id  // Lower ID is polite
}
```

**Why perfect negotiation?** With simple tie-breaking, if peer A is "satisfied" and peer B needs connections, B might not be able to connect if A was supposed to initiate. Perfect negotiation solves this: B sends an offer, A accepts it (since A can still accept up to `max_connections`).

## Routing Strategies

### Flooding
- Sends requests to ALL connected peers simultaneously
- First response wins
- High bandwidth, low latency
- Good for small networks or when speed is critical

### Adaptive
- Tries peers sequentially, ordered by past performance
- Learns which peers have data and respond quickly
- Low bandwidth, slightly higher latency
- Uses exponential backoff for slow/unreliable peers

## Latency Simulation

Per-link latency is configurable:
- `network_latency_ms`: Mean latency (e.g., 50ms for realistic WebRTC)
- `latency_variation`: How much latency varies per link (0.0-1.0)
- `latency_seed`: Seed for reproducible latency distribution

Each link gets a fixed latency drawn from a distribution centered on `network_latency_ms`.

## Multi-Hop Forwarding (HTL)

Requests include a **Hops-To-Live** counter (like Freenet):
- Starts at MAX_HTL (10)
- Decremented at each hop (with probabilistic variation per-peer)
- When HTL=0, request is not forwarded further
- Prevents infinite loops and limits network load

## Running Simulations

```bash
# Basic simulation
cargo run --example run_simulation

# With options
cargo run --example run_simulation -- \
  --nodes 200 \
  --strategy adaptive \
  --latency 50 \
  --seed 42

# Benchmark mode (measures throughput)
cargo run --example run_simulation -- --bench --nodes 100

# Burst benchmark (realistic load)
cargo run --example run_simulation -- --burst --nodes 50

# Multiple runs for variance analysis
cargo run --example run_simulation -- --bench --runs 5
```

## Key Learnings

1. **max_peers matters**: Too low (< ln(N)) causes network fragmentation
2. **Adaptive beats Flooding** for bandwidth efficiency once it learns peer quality
3. **Latency variation** is important - uniform latency is unrealistic
4. **Multi-hop forwarding** dramatically increases reach but adds latency
5. **Perfect negotiation beats simple tie-breaking**: With simple "lower ID initiates" tie-breaking, satisfied nodes don't initiate, leaving unsatisfied nodes unable to connect to them. Perfect negotiation (both sides can send offers, collisions resolved by polite/impolite) solves this by letting unsatisfied nodes reach satisfied-but-not-full nodes.
6. **Use same code for simulation**: Using the exact same signaling code as production ensures simulation behavior matches reality. The `webrtc_sim` module does this.
