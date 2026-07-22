# Formal Invariants

This table tracks formal-style verification targets and their proof artifacts.

Status values:
- `Planned`
- `In Progress`
- `Verified`

| Invariant | Spec Source | Code Location | Proof Artifact | CI Job | Status |
| --- | --- | --- | --- | --- | --- |
| HT-RES-001: Resolve chooses newest `created_at`, then lowest `event.id` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_resolve_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-RES-002: resolve_shared chooses newest `created_at`, then lowest `event.id` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_resolve_shared_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-RES-003: subscribe updates only for newer timestamps or lower tied `event.id` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_subscribe_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-RES-004: list dedupe uses newest timestamp and lowest tied `event.id` per `d` | `docs/HTS-01.md` §7 | `rust/crates/hashtree-resolver/src/nostr.rs` | `test_list_dedupe_tie_breaks_with_event_id` | `resolver-formal` | Verified |
| HT-CORE-001: `decode(encode(node)) == normalized(node)` | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-002: encoding is deterministic across runs | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-003: metadata insertion order does not affect encoding | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-004: invalid node `t` rejected; invalid link `t` defaults to Blob | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/codec.rs` | `formal_codec_props.rs` | `core-props` | Verified |
| HT-CORE-014: BUD-16 directory profile gives one encoding per semantic directory | BUD-16 deterministic MessagePack profile | `rust/crates/hashtree-core/src/codec.rs`, `ts/packages/hashtree/src/codec.ts`, directory builders | `formal/bud16_messagepack_determinism/Bud16MessagePackDeterminism.tla`, `formal_codec_props.rs`, `codec.test.ts`, `builder.test.ts` | `bud16-messagepack-tlc` + `core-props` | Verified |
| HT-CORE-005: `get(put(bytes)) == bytes` (public + encrypted) | `docs/HTS-01.md` §2-§5 | `rust/crates/hashtree-core/src/hashtree.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-006: `put_stream` equivalent to `put` for same input/config | `docs/HTS-01.md` §2-§5 | `rust/crates/hashtree-core/src/hashtree.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-007: `read_file_range` equals slice semantics | `docs/HTS-01.md` §2-§4 | `rust/crates/hashtree-core/src/reader.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-008: path resolution consistency for generated directories | `docs/HTS-01.md` §3 | `rust/crates/hashtree-core/src/hashtree.rs` | `formal_tree_props.rs` | `core-props` | Verified |
| HT-CORE-009: diff identity (`diff(old, old)` empty) | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-010: diff completeness/minimality w.r.t. reachable sets | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-011: diff first push returns all new reachable hashes | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-012: keyed decryption failure in diff returns explicit error | `docs/HTS-01.md` §5 | `rust/crates/hashtree-core/src/diff.rs` | `formal_diff_props.rs` | `core-props` | Verified |
| HT-CORE-013: strong integrity verifier detects missing and corrupted children | `docs/HTS-01.md` §2 | `rust/crates/hashtree-core/src/reader.rs` | `test_verify_tree_integrity_*` | `rust-tests` | Verified |
| HT-GIT-001: non-fast-forward rejected unless force | `docs/HTS-01.md` §10 | `rust/crates/git-remote-htree/src/helper.rs` | `formal_git_props.rs` | `rust-tests` | In Progress |
| HT-GIT-002: deleting one ref does not mutate others | `docs/HTS-01.md` §10 | `rust/crates/git-remote-htree/src/git/storage.rs` | `formal_git_props.rs` | `rust-tests` | Verified |
| HT-GIT-003: all written refs satisfy ref-name constraints | git ref rules + project policy | `rust/crates/git-remote-htree/src/git/refs.rs` | `formal_git_props.rs` | `rust-tests` | Verified |
| HT-GIT-004: untouched remote refs are preserved during push | `docs/HTS-01.md` §10 | `rust/crates/git-remote-htree/src/helper.rs` | `formal_git_props.rs` | `rust-tests` | In Progress |
| HT-ROUTE-001: explicit route preference, route-local NoResult, bounded cooldown, recovery, and central hash verification preserve correctness | `docs/HTS-01.md` §11 | `rust/crates/hashtree-network/src/blob_router.rs` | `blob_router.rs` | `rust-tests` | Verified |
