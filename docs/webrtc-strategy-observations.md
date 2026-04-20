# WebRTC Strategy Observations

Date: March 3, 2026
Scope: `hashtree-sim` / `hashtree-webrtc` routing and actor strategy tuning

## Strategy Set

We keep a small stable strategy set and iterate inside it:

1. `flood_weighted` (baseline)
2. `hedged_weighted` (primary challenger)
3. `hedged_utility`
4. `hedged_latency`

`auto` mode in `tune_webrtc_params` focuses on baseline + primary challenger to keep iteration fast.

## Actor Model Notes

The retired reciprocity-heavy selector used:

- reliability (`successes / requests_sent`)
- reciprocity (`bytes_received / bytes_sent`)
- retaliation penalties (timeouts/failures/backoff)
- small exploration term for low-sample peers

This is intended to avoid rewarding unresponsive or one-way peers while still exploring.

## Recent Runs

Command:

```bash
cargo run -p hashtree-sim --example tune_webrtc_params -- --mode=manual
```

Result file: `/tmp/hsim_manual_weighted.out`

Top exploration rows (score descending):

1. `manual:24/12:hedged_weighted`
2. `manual:20/10:hedged_latency`
3. `manual:24/12:hedged_latency`

Key observation:

- `hedged_weighted` is the relevant hedged challenger now that the older reciprocity-heavy selector has been removed.
- Promotion gates are currently too strict for this scenario mix (all candidates failed promotion). This indicates gate thresholds need environment-specific profiles, not that strategies are unusable.

## Scalability Checks

Always-on connectivity test:

```bash
cargo test -p hashtree-sim mesh_sim::tests::test_mesh_sim_1000_nodes_connectivity -- --nocapture
```

Recent sample:

- components: `~23-27`
- largest component: `~387-616`
- connections: `~7130-7155`
- runtime: about `10-11s` for this test

Key observation:

- 1000-node testing is fast enough for default CI runs.
- topology still fragments; optimize for `largest_component` and `component_count` directly.

## Next Hypotheses

1. Add explicit local debt ceiling for peers with persistent low reciprocity (soft-quarantine window).
2. Introduce strategy-specific hello cadence (high-degree actors can reannounce less often).
3. Tune hedged wave timing by network size (shorter intervals on larger overlays).
