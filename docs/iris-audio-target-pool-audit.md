# Iris Audio target-Pool residency audit

`iris_audio_target_pool_audit` is a strictly read-only, resumable release
auditor for an exact Iris Audio inventory. It proves every reachable catalog,
song, audio, and immutable image block against one pinned PoolStore member set.
It opens the Pool catalog and members read-only and never repairs locations,
updates temperature state, or writes crawler authority files.

The command is:

```text
cargo run -p hashtree-lmdb --release \
  --example iris_audio_target_pool_audit -- \
  CONFIG_JSON INVENTORY_TSV LEDGER_JSONL CHECKPOINT_JSON MANIFEST_JSON
```

`--max-batches N` performs a bounded number of durable batches. Running the
same command again resumes only when the raw config SHA-256, inventory SHA-256,
inventory record count, derived inventory identity, and the exact initial Pool
manifest identity all match the checkpoint.

The inventory header is exactly:

```text
sourceKey	songId	hash	key
```

The config schema is
`iris-audio-target-pool-residency-audit-config/v1`. A release config pins:

- the exact Pool catalog path and complete expected manifest member-ID set;
- the allowed native target member-ID subset (which deliberately excludes
  draining legacy members that remain in the manifest during this audit);
- the exact inventory SHA-256 and record count;
- every legacy fallback tier used to make absence exhaustive;
- at least one immutable catalog root;
- optional batching and bounded-read sizes.

Fallback tiers are evidence sources, never acceptable release destinations.
The raw config bytes are hashed into the terminal manifest, so changing a
member, fallback tier, catalog root, or limit cannot silently resume an older
run.

The auditor requires the live manifest IDs to equal `expectedPoolMembers`
exactly and requires every `targetMembers` entry to be in that full set. This
lets the proof run before legacy-member removal without treating physical
bytes on the still-present legacy member as migrated.

Each JSONL block row has schema
`iris-audio-target-pool-residency-block/v1`. It records the parent inventory or
additional-root identity, block path and role, catalog candidates, the result
from every exact target member, every probed fallback tier, a hash-valid
witness, CHK/tree traversal outcome, and one residency classification:

- `target-valid`: the Pool catalog has a terminal `Stored` location on an
  allowed native target member and that exact member returned full bytes whose
  SHA-256 equals the block hash. `Pending` and `Moving` catalog states never
  pass, even when target bytes already exist.
- `fallback-only`: no target member is valid, but a configured fallback tier
  returned hash-valid bytes.
- `catalog-mismatch`: target bytes are hash-valid but the catalog does not name
  that member.
- `missing`: every target and fallback tier was conclusively absent.
- `corrupt`: stored bytes exist but no hash-valid witness exists.
- `unknown`: an I/O or catalog error prevents an exact conclusion.

Valid fallback bytes are used to continue traversal so one missing root does
not hide repair work in its descendants. They still make the release gate
fail. Encrypted nodes and chunks must pass CHK authentication; declared tree
links must decode with the expected type. Immutable `htree://nhash...` roots
found in JSON and track-entry metadata are added to the same transitive audit.
Traversal deduplicates by `(hash, key, expected link type)`, so cycles remain
bounded while two references that declare incompatible types are both checked;
at least one incompatible declaration then fails closed.

The checkpoint schema is
`iris-audio-target-pool-residency-audit-checkpoint/v1`. A checkpoint is
published only after the JSONL prefix is flushed and synced, and records the
exact committed byte offset plus the starting Pool manifest generation and
SHA-256. That SHA-256 covers the exact stored manifest bytes, including member
states and every member configuration. Resume truncates any uncommitted suffix
to the committed offset and rejects a changed manifest.

The terminal manifest separately pins `expectedPoolMemberIds` (the complete
manifest identity) and `targetMemberIds` (the only acceptable residency
witnesses), together with the full manifest generation and SHA-256. Before
writing a terminal manifest the auditor opens a fresh read-only Pool reader and
requires that complete identity to equal the checkpointed start identity. A
long-running audit therefore cannot certify stale member configuration or
membership.

The terminal manifest schema is
`iris-audio-target-pool-residency-manifest/v1`. It pins the inventory identity,
config SHA-256, target member IDs, fallback tier names, JSONL SHA-256 and byte
length, row and unique-block counts, complete classification totals, and
`releaseReady`. `releaseReady` is true only when every work item completed,
every row is `target-valid`, and there are no traversal failures.
