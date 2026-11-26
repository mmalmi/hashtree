# Forward append and rollback

`htree nostr-index append-forward` appends a bounded, authenticated collector batch
to the current catch-up archive. It preserves the historical policy, pass,
author cursor and event counter. Only the root and coverage head change. The
historical author census remains required for coverage validation; forward
event authors need not belong to it.

Keep the collector's immutable batch, configuration digest and `since` value.
Supply them with `--batch`, `--batch-sha256`, `--config-sha256`, `--since`,
`--authors-file` and the deployment's `--min-free-bytes` floor. The archive's
existing writer lock serializes this operation with historical catch-up. An
unfinished historical append refuses new forward work.

The command verifies the exact canonical batch bytes, every event ID and
signature, the approved public kinds, and the size/time limits. Kind 5 is stored
as a signed deletion request, consistent with the existing archive importer;
this command does not apply tombstones or expiry cleanup. Each acknowledgement
proves either the exact signed event remains indexed or a verified newer event
occupies its same replaceable coordinate. Old roots and superseded blocks are
preserved.

## Commit and recovery

Within the archive's `nostr-index` namespace, the command retains:

- `catchup-forward/<batch-sha256>.batch.json`: exact input bytes.
- `catchup-forward/<batch-sha256>.json`: durable acknowledgement.
- `catchup-coverage/<coverage-head>.json`: a forward coverage bridge to the
  unchanged historical coverage evidence.
- `catchup-forward-pending.json`: a transaction awaiting complete durable commit.

Blocks and the bridge are synced before the pending transaction. The canonical
checkpoint is then replaced, the acknowledgement is synced, and pending state
is retired. A compatible `append-forward` or `catch-up` invocation finishes a
pending transaction locally under the writer lock before proceeding. Retry the
exact batch; a completed receipt is returned unchanged even after subsequent
historical progress. A failed or uncertain attempt is not permission to rewind
the checkpoint, discard receipts, or delete its blocks.

## Native rollback boundary

Before the first forward commit, the previous executable can resume its ordinary
historical checkpoint. Once a forward bridge becomes the current coverage head,
the previous executable cannot parse that head. Preserve a verified executable
with both forward recovery and bridge-aware catch-up until this boundary closes.
If a forward transaction is pending, that compatible executable must first
complete it using the retained batch and census. Keep the compatible publisher
and outbox recovery path available to finish acknowledgement and publication.

One **successfully completed ordinary historical author checkpoint** replaces
the forward head with the existing ordinary coverage format. It starts from the
forward root, retains those events and advances only the normal author cursor.
The ordinary receipt's `previous_head` still refers to the bridge. The historical
native validator checks that reference's digest shape but does not traverse it,
so the ordinary checkpoint restores native resume compatibility. If the pass was
already complete, start the next legitimate pass and complete its first author;
merely preparing a pass or receiving an incomplete source response is insufficient.

Before switching native executables, require a closed writer, no pending forward
transaction, an ordinary current coverage receipt, exact retained-event readback,
and accepted publication of the resulting root. Do not fabricate an empty author
receipt or restore a pre-forward checkpoint. Native compatibility does not prove
that an old publisher understands the retained bridge history: publisher and
outbox rollback require their own qualification.

## Qualification

The normal forward integration test exercises append, historical continuation,
exact receipt replay and pass rollover using an isolated loopback relay. The
separate `rollback::old_cli_rejects_bridge_then_resumes_after_one_real_historical_checkpoint`
test requires an externally authenticated historical CLI artifact. Set
`HTREE_FORWARD_ROLLBACK_BINARY` and `HTREE_FORWARD_ROLLBACK_SHA256`, then run that
exact integration test with `--ignored`. It hashes the artifact before execution,
proves it works before the forward append, verifies rejection without checkpoint
mutation at a forward head, then proves successful continuation and signed-event
readback after one real historical checkpoint. Record the old artifact's source
binding separately. Without this explicit run, old-binary rollback remains
unqualified; ordinary test passes do not imply it.
