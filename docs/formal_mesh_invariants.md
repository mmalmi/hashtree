# Formal Mesh Invariants

## Scope
This document records formal-style invariants for the shared Hashtree mesh
protocol. Production peerfinding and links should be provided by FIPS; Hashtree
keeps hash-addressed request, routing, HTL, and pubsub invariants here.

Code scope:
- `rust/crates/hashtree-network/src/types.rs`
- `rust/crates/hashtree-network/src/protocol.rs`
- `rust/crates/hashtree-network/src/peer_selector.rs`
- `rust/crates/hashtree-network/src/mesh_store_core.rs`
- `ts/packages/hashtree-mesh/src/types.ts`
- `ts/packages/hashtree-mesh/src/protocol.ts`
- `ts/packages/hashtree-mesh/src/peerSelector.ts`

## Invariants
- `HT-MESH-001`: HTL is monotonic non-increasing per hop, and requests with `htl=0` are not forwarded.
- `HT-MESH-002`: Forwarding predicate equivalence holds (`should_forward(htl) == (htl > 0)`).
- `HT-MESH-003`: Backoff/fairness rules constrain peer ordering and avoid selecting only backed-off peers when alternatives exist.
- `HT-MESH-004`: Mesh frame validation enforces protocol/version/ID/HTL bounds and allows only signed kind `25050` events.
- `HT-MESH-005`: Seen-set dedupe rejects duplicate frame/event IDs and evicts oldest entries when over capacity.

## Safety Rules
- Never increase HTL during forwarding.
- Prefer non-backed-off peers; use backed-off peers only as fallback.
- Reject malformed mesh frames before local processing or forwarding.
- Deduplicate mesh frames/events before processing to prevent replay amplification.

## Test Strategy
- Deterministic tests in:
  - `rust/crates/hashtree-network/tests/formal_mesh_props.rs`
  - `rust/crates/hashtree-network/tests/types.rs`
  - `ts/packages/hashtree-mesh/tests/core.test.ts`
- No public relay dependencies for formal invariants.

## CI
- Planned CI job names:
  - `mesh-formal` (Rust mesh core)
  - `ts-mesh-formal` (TypeScript parity checks)
- Initially advisory, promoted to hard gate after stabilization.
