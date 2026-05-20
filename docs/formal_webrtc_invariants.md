# Formal Legacy WebRTC Invariants

## Scope
This document records formal-style invariants for the legacy Hashtree-owned
WebRTC adapter. New Hashtree P2P transport should use FIPS and keep shared mesh
protocol invariants in `ts/packages/hashtree-mesh`.

Code scope:
- `rust/crates/hashtree-webrtc/src/types.rs`
- `rust/crates/hashtree-webrtc/src/peer.rs`
- `rust/crates/hashtree-webrtc/src/peer_selector.rs`
- `rust/crates/hashtree-cli/src/webrtc/types.rs`
- `rust/crates/hashtree-cli/src/webrtc/signaling.rs`
- `rust/crates/hashtree-cli/tests/two_instances.rs`
- `ts/packages/hashtree-mesh/src/types.ts`
- `ts/packages/hashtree-mesh/src/protocol.ts`
- `ts/packages/hashtree-mesh/src/peerSelector.ts`

## Invariants
- `HT-WEBRTC-001`: HTL is monotonic non-increasing per hop, and requests with `htl=0` are not forwarded.
- `HT-WEBRTC-002`: Fragment reassembly outputs payload only when all fragments `[0..n-1]` are present exactly once.
- `HT-WEBRTC-003`: Backoff/fairness rules constrain peer ordering and avoid selecting only backed-off peers when alternatives exist.
- `HT-WEBRTC-004`: Pending reassembly state is bounded and cleared on completion.
- `HT-WEBRTC-005`: Shared probabilistic HTL policy for blob and mesh traffic is bounded and follows edge rules at `max_htl` and `htl=1`.
- `HT-WEBRTC-006`: Forwarding predicate equivalence holds (`should_forward(htl) == (htl > 0)`).
- `HT-WEBRTC-007`: Mesh frame validation enforces protocol/version/ID/HTL bounds and allows only signed kind `25050` events.
- `HT-WEBRTC-008`: Seen-set dedupe rejects duplicate frame/event IDs and evicts oldest entries when over capacity.
- `HT-WEBRTC-009`: After chain bootstrap (`A-B`, `B-C`) and relay shutdown, `A-C` can still connect via relayless mesh and fetch content.

## Safety Rules
- Never increase HTL during forwarding.
- Preserve deterministic fragment ordering during reassembly.
- Prefer non-backed-off peers; use backed-off peers only as fallback.
- Ensure completed reassemblies are removed from pending state.
- Reject malformed mesh frames before local processing or forwarding.
- Deduplicate mesh frames/events before processing to prevent replay amplification.

## Test Strategy
- Deterministic tests in:
  - `rust/crates/hashtree-webrtc/tests/formal_webrtc_props.rs`
  - `rust/crates/hashtree-cli/src/webrtc/tests.rs`
  - `rust/crates/hashtree-cli/src/webrtc/signaling.rs` (`mod tests`)
  - `rust/crates/hashtree-cli/tests/two_instances.rs`
  - `ts/packages/hashtree-mesh/tests/core.test.ts`
  - `ts/packages/hashtree-fips-transport/tests/fipsTransport.test.ts`
- No public relay dependencies for formal invariants; relayless mesh proof uses local test relays.

## CI
- Planned CI job names:
  - `webrtc-formal` (transport/core)
  - `webrtc-cli-formal` (mesh manager + relayless chain integration)
  - `ts-webrtc-formal` (TypeScript parity checks)
- Initially advisory, promoted to hard gate after stabilization.
