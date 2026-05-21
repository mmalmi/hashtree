# Formal Verification Plan for Hashtree (Rust-Native, Step-by-Step)

## Summary
This plan establishes formal-style correctness guarantees for hashtree using Rust-native methods first: property-based testing, state-machine testing, and bounded model checks where practical.
Execution starts with **Resolver + Core** (highest consensus and data-integrity risk), then extends to **git-remote** and **mesh transport**.
CI rollout is **advisory first**, then promoted to hard gate after stability criteria are met.

Primary correctness targets in current code:
- Nostr root selection ordering in `rust/crates/hashtree-resolver/src/nostr.rs` vs spec in `docs/HTS-01.md`.
- Canonical encoding and hash determinism in `rust/crates/hashtree-core/src/codec.rs`.
- Merkle build/read/diff invariants in `rust/crates/hashtree-core/src/hashtree.rs`, `rust/crates/hashtree-core/src/reader.rs`, `rust/crates/hashtree-core/src/diff.rs`.

## Scope
In scope:
- Resolver determinism and spec conformance.
- Core codec/tree/diff safety properties.
- Formal verification artifact structure and CI.
- Subsequent wave specifications for git-remote and mesh transport.

Out of scope in first wave:
- Heavy theorem-prover-first adoption (Prusti/Creusot-first workflow).
- Replacing existing integration/e2e suites.
- Protocol redesign.

## Public APIs / Interfaces / Types (Planned Changes)
1. Add a stronger integrity API in core:
- File: `rust/crates/hashtree-core/src/reader.rs`
- New public function:
  `pub async fn verify_tree_integrity<S: Store>(store: Arc<S>, root_hash: &Hash) -> Result<VerifyIntegrityResult, ReaderError>`
- New public type:
  `pub struct VerifyIntegrityResult { pub valid: bool, pub missing: Vec<Hash>, pub corrupted: Vec<Hash> }`
- Re-export from `rust/crates/hashtree-core/src/lib.rs`.

2. Tighten diff behavior for keyed nodes:
- File: `rust/crates/hashtree-core/src/diff.rs`
- Behavior change: if `key` is present and decryption fails, return `HashTreeError::Decryption` instead of silently treating ciphertext as plaintext.
- No function signature change, but semantics become strict and sound.

3. Resolver event selection ordering:
- File: `rust/crates/hashtree-resolver/src/nostr.rs`
- Internal interface addition:
  `fn pick_latest_event<'a, I>(events: I) -> Option<&'a Event> where I: IntoIterator<Item=&'a Event>`
- Ordering fixed to `(created_at, event.id)`.

## Step-by-Step Execution Plan

## Step 1: Create verification documentation scaffold
- Add `docs/formal-verification-plan.md` with this exact plan.
- Add `docs/formal-invariants.md` with invariant IDs and status.
- Add invariant ID format: `HT-RES-*`, `HT-CORE-*`, `HT-GIT-*`, `HT-WEBRTC-*`.
- Add columns: `Invariant`, `Spec Source`, `Code Location`, `Proof Artifact`, `CI Job`, `Status`.
- Done when every step below maps to invariant IDs before any code changes.

## Step 2: Resolver deterministic selection (spec conformance)
- Implement `pick_latest_event` in `rust/crates/hashtree-resolver/src/nostr.rs`.
- Replace manual `latest_created_at` loops in `resolve`, `resolve_shared`, and `list` dedupe logic.
- Update subscription update condition from timestamp-only to full tuple ordering.
- Ensure behavior matches `docs/HTS-01.md:146`-`docs/HTS-01.md:150`.
- Add tests:
  `test_resolve_tie_breaks_with_event_id`.
  `test_resolve_shared_tie_breaks_with_event_id`.
  `test_subscribe_tie_breaks_with_event_id`.
  `test_list_dedupe_tie_breaks_with_event_id`.
- Done when resolver and git client ordering behavior align with `rust/crates/git-remote-htree/src/nostr_client.rs:250`.

## Step 3: Core codec formal properties
- Add dev dependency `proptest = "1"` to `rust/crates/hashtree-core/Cargo.toml`.
- Add new test file `rust/crates/hashtree-core/tests/formal_codec_props.rs`.
- Implement generators for bounded `TreeNode` and `Link` data.
- Verify properties:
  `decode(encode(node)) == normalized(node)`.
  `encode(node)` is deterministic across repeated encodes.
  Metadata insertion order does not change encoding.
  Invalid node type is rejected.
  Unknown link type decodes as `Blob`.
- Done when property suite passes with `PROPTEST_CASES=500` locally and in CI.

## Step 4: Core put/get/tree equivalence properties
- Add `rust/crates/hashtree-core/tests/formal_tree_props.rs`.
- Verify properties across random payloads and configs:
  `get(put(bytes)) == bytes` for encrypted and public modes.
  `put_stream(cursor(bytes))` produces same CID and bytes as `put(bytes)` under same config.
  `read_file_range` equals direct slice for all valid `(start,end)` in public mode.
  Directory path resolution returns expected target for randomly generated directory trees.
- Include deterministic seeds and bounded sizes for CI stability.
- Done when no flaky failures in 50 repeated local runs.

## Step 5: Diff soundness and strict decryption semantics
- Update `rust/crates/hashtree-core/src/diff.rs` to error on keyed decrypt failure.
- Add `rust/crates/hashtree-core/tests/formal_diff_props.rs`.
- Verify properties:
  Identity: `diff(old, old).added == []`.
  Completeness: every new reachable hash not in old reachable set appears in `added`.
  Minimality: no hash in `added` exists in old reachable set.
  First push: `old=None` returns all reachable new hashes.
  Encrypted correctness: strict failure when key/ciphertext mismatch.
- Done when all diff properties pass and legacy tests updated for strict behavior.

## Step 6: Add strong integrity verifier
- Implement `verify_tree_integrity` and `VerifyIntegrityResult` in `rust/crates/hashtree-core/src/reader.rs`.
- Integrity rule: for each traversed edge `parent -> child_hash`, fetched child bytes must satisfy `sha256(bytes) == child_hash`.
- Keep existing `verify_tree` API unchanged for backward compatibility.
- Re-export in `rust/crates/hashtree-core/src/lib.rs`.
- Add tests:
  `test_verify_tree_integrity_valid`.
  `test_verify_tree_integrity_missing`.
  `test_verify_tree_integrity_corrupted_hash_mismatch`.
- Done when strong verifier detects corruption that old verifier cannot.

## Step 7: CI advisory rollout
- Add new workflow `.github/workflows/formal-verify.yml`.
- Jobs:
  `resolver-formal` for resolver determinism tests.
  `core-props` for codec/tree/diff property suites.
  `core-integrity` for strong integrity verifier tests.
- Run on `pull_request` and `push` to `master`.
- Set jobs advisory initially with `continue-on-error: true`.
- Upload failing seeds/artifacts for reproducibility.
- Done when workflow runs on all PRs with visible status but non-blocking.

## Step 8: Promote verification to hard gate
- Promotion criteria:
  10 consecutive green runs on `master`.
  Zero unresolved flaky failures for 14 days.
  At least one seed-reproduction playbook validated.
- Remove advisory mode from workflow.
- Mark verification jobs as required checks in repo settings.
- Done when merges to `master` require formal verification green.

## Step 9: Second wave spec and implementation (git-remote)
- Add `docs/formal_git_state_machine.md`.
- Build deterministic state-machine tests around `rust/crates/git-remote-htree/src/helper.rs` and `rust/crates/git-remote-htree/src/git/refs.rs`.
- Invariants:
  Non-fast-forward push rejected unless forced.
  Ref deletions affect only target ref.
  Ref names always pass validation constraints.
  Existing remote refs are preserved when not targeted by push.
- Add `formal_git_props.rs` integration test suite.
- Roll into same advisory->hard-gate flow after stabilization.

## Step 10: Third wave spec and implementation (mesh transport)
- Add `docs/formal_mesh_invariants.md`.
- Build deterministic simulation/property tests for:
  HTL monotonicity and forwarding stop at zero from `rust/crates/hashtree-network/src/types.rs`.
  Peer selector fairness/backoff ordering in `rust/crates/hashtree-network/src/peer_selector.rs`.
  Mesh frame validation and seen-set dedupe in `rust/crates/hashtree-network/src/types.rs`.
- Integrate into formal workflow after pass stability.

## Test Cases and Scenarios (Acceptance Matrix)
- Resolver:
  equal `created_at`, different `event.id`.
  labeled vs unlabeled hashtree events.
  shared/private/public key tags parsing.
- Core codec:
  random tree shapes.
  random metadata key order.
  invalid link/node type inputs.
- Core tree:
  random byte payloads including empty and max-bounded sizes.
  encrypted/public configs.
  random valid range windows.
- Diff:
  old/new identical.
  first push.
  subtree reuse.
  keyed decryption mismatch.
- Integrity:
  missing child blobs.
  child blob tampering.
  mixed valid/missing/corrupt trees.

## Implementation Order for Actual Work Sessions
1. Step 1.
2. Step 2.
3. Step 3.
4. Step 4.
5. Step 5.
6. Step 6.
7. Step 7.
8. Step 8.
9. Step 9.
10. Step 10.

Each step must finish with:
- invariant table update.
- targeted test run.
- full affected crate tests.
- commit on `master`.
- push to `htree://self/hashtree`.

## Assumptions and Defaults
- Default verification approach is Rust-native formal methods first.
- First execution wave is Resolver + Core.
- CI rollout is advisory first, then hard gate.
- Stable Rust toolchain remains default; nightly-only tools are optional and not required in first wave.
- Verification tests are deterministic and do not depend on public relays or external production services.
- Existing behavior remains unchanged unless explicitly noted above (resolver tie-break fix and strict diff decryption semantics).
