# Formal Git Remote State Machine

## Scope
This document defines deterministic state-machine invariants for `git-remote-htree` push/fetch safety.

Code scope:
- `rust/crates/git-remote-htree/src/helper.rs`
- `rust/crates/git-remote-htree/src/git/refs.rs`
- `rust/crates/git-remote-htree/src/git/storage.rs`

## States
- `Idle`: helper initialized, no pending operations.
- `Listed`: refs loaded from remote.
- `QueuedPush`: one or more push specs accepted.
- `Pushing`: push execution started.
- `Done`: push/fetch cycle completed.
- `Error`: operation aborted by validation or transport failure.

## Invariants
- `HT-GIT-001`: Non-fast-forward is rejected unless force flag is present.
- `HT-GIT-002`: Deleting one ref mutates only that target ref.
- `HT-GIT-003`: All accepted ref names satisfy git ref-name constraints.
- `HT-GIT-004`: Refs not targeted by current push remain present after write.

## Safety Rules
- Validate destination ref names before write.
- Perform ancestry check for non-forced updates.
- Preserve untouched refs when applying targeted updates.
- Reject invalid state transitions (e.g., malformed push spec).

## Test Strategy
- Deterministic integration tests in `rust/crates/git-remote-htree/tests/formal_git_props.rs`.
- Local-only tests; no relay/network dependencies.
- Use temporary git repos and storage roots for reproducibility.

## CI
- Runs in the ordinary `rust-tests` gate so the integration binary is built
  once and remains a required regression suite.
