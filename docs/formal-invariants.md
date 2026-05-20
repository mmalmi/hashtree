# Formal Invariants

This table tracks formal-style verification targets and their proof artifacts.

Status values:
- `Planned`
- `In Progress`
- `Verified`

| Invariant | Spec Source | Code Location | Proof Artifact | CI Job | Status |
| --- | --- | --- | --- | --- | --- |
| HT-RES-001: Resolve chooses latest by `(created_at, event.id)` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_resolve_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-RES-002: resolve_shared chooses latest by `(created_at, event.id)` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_resolve_shared_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-RES-003: subscribe updates only for newer `(created_at, event.id)` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_subscribe_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-RES-004: list dedupe uses latest `(created_at, event.id)` per `d` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_list_dedupe_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-CORE-001: `decode(encode(node)) == normalized(node)` | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-002: encoding is deterministic across runs | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-003: metadata insertion order does not affect encoding | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-004: invalid node `t` rejected; invalid link `t` defaults to Blob | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-005: `get(put(bytes)) == bytes` (public + encrypted) | `docs/HTS-01.md` §2-§5 | `rust/crates/hashtree-core/src/hashtree.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-006: `put_stream` equivalent to `put` for same input/config | `docs/HTS-01.md` §2-§5 | `rust/crates/hashtree-core/src/hashtree.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-007: `read_file_range` equals slice semantics | `docs/HTS-01.md` §2-§4 | `rust/crates/hashtree-core/src/reader.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-008: path resolution consistency for generated directories | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/hashtree.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-009: diff identity (`diff(old, old)` empty) | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-010: diff completeness/minimality w.r.t. reachable sets | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-011: diff first push returns all new reachable hashes | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-012: keyed decryption failure in diff returns explicit error | `docs/HTS-01.md` §5 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-013: strong integrity verifier detects missing and corrupted children | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/reader.rs` | `test_verify_tree_integrity_*` | `core-integrity` | Verified |
| HT-GIT-001: non-fast-forward rejected unless force | `docs/HTS-01.md` §10 | `rust/crates/git-remote-htree/src/helper.rs` | `formal_git_props.rs` | `git-formal` | In Progress |
| HT-GIT-002: deleting one ref does not mutate others | `docs/HTS-01.md` §10 | `rust/crates/git-remote-htree/src/git/storage.rs` | `formal_git_props.rs` | `git-formal` | Verified |
| HT-GIT-003: all written refs satisfy ref-name constraints | git ref rules + project policy | `rust/crates/git-remote-htree/src/git/refs.rs` | `formal_git_props.rs` | `git-formal` | Verified |
| HT-GIT-004: untouched remote refs are preserved during push | `docs/HTS-01.md` §10 | `rust/crates/git-remote-htree/src/helper.rs` | `formal_git_props.rs` | `git-formal` | In Progress |
| HT-WEBRTC-001: HTL never increases and forwarding halts at zero | `docs/HTS-01.md` §11.1 | `rust/crates/hashtree-webrtc/src/types.rs`, `peer.rs` | `formal_webrtc_props.rs` | `webrtc-formal` | Verified |
| HT-WEBRTC-002: fragment reassembly returns complete ordered payload once | `docs/HTS-01.md` §11 | `rust/crates/hashtree-webrtc/src/peer.rs` | `peer::tests::test_fragment_reassembly_*` | `webrtc-formal` | Verified |
| HT-WEBRTC-003: selector fairness/backoff ordering constraints hold | Freenet-inspired policy | `rust/crates/hashtree-webrtc/src/peer_selector.rs` | `formal_webrtc_props.rs` | `webrtc-formal` | Verified |
| HT-WEBRTC-004: stale pending reassemblies are bounded/cleaned up | robustness requirement | `rust/crates/hashtree-webrtc/src/peer.rs` | `peer::tests::test_fragment_reassembly_*` | `webrtc-formal` | Verified |
| HT-WEBRTC-005: shared blob/mesh HTL policy is monotonic, bounded, and edge-correct | relayless mesh design + Freenet-style HTL | `rust/crates/hashtree-cli/src/webrtc/types.rs`, `ts/packages/hashtree-mesh/src/protocol.ts` | `test_formal_htl_policy_monotonicity_and_bounds`, `core.test.ts` | `webrtc-cli-formal`, `ts-mesh-formal` | Verified |
| HT-WEBRTC-006: forwarding predicate equivalence holds (`should_forward(htl) == htl > 0`) | relayless mesh design | `rust/crates/hashtree-cli/src/webrtc/types.rs`, `ts/packages/hashtree-mesh/src/protocol.ts` | `test_formal_should_forward_htl_equivalence`, `core.test.ts` | `webrtc-cli-formal`, `ts-mesh-formal` | Verified |
| HT-WEBRTC-007: mesh frames reject invalid protocol/version/IDs/HTL and non-25050 events | relayless mesh frame spec (`htree.nostr.mesh.v1`) | `rust/crates/hashtree-cli/src/webrtc/types.rs`, `ts/packages/hashtree-mesh/src/types.ts` | `test_formal_mesh_frame_validation_*`, `core.test.ts` | `webrtc-cli-formal`, `ts-mesh-formal` | Verified |
| HT-WEBRTC-008: mesh seen-set dedupe rejects duplicate frame/event IDs and evicts oldest by cap | relayless mesh anti-loop requirement | `rust/crates/hashtree-cli/src/webrtc/signaling.rs` | `test_formal_timed_seen_set_*` | `webrtc-cli-formal` | Verified |
| HT-WEBRTC-009: relayless chain bootstrap enables A-C connect and blob fetch after relay shutdown | relayless mesh acceptance matrix | `rust/crates/hashtree-cli/tests/two_instances.rs` | `test_three_peers_chain_bootstrap_then_ac_connect_without_relay` | `webrtc-cli-formal` | Verified |
