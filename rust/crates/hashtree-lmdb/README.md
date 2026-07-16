# hashtree-lmdb

LMDB-backed content-addressed blob storage for hashtree.

High-performance storage backend using LMDB (Lightning Memory-Mapped Database) for fast key-value storage.

## Usage

```rust
use hashtree_core::Store;
use hashtree_lmdb::{compute_sha256, LmdbBlobStore};

let store = LmdbBlobStore::new("/path/to/data")?;
let data = b"immutable blob".to_vec();
let hash = compute_sha256(&data);

assert!(store.put(hash, data.clone()).await?);
assert!(!store.put(hash, data.clone()).await?); // idempotent

assert_eq!(store.get(&hash).await?, Some(data));
```

## Multiprocess sharing

Trusted processes running as the same OS user may open one store path on a local
filesystem directly; no storage daemon is required. LMDB serializes writers and
exposes only committed transactions to readers. Blob insertion uses `NO_OVERWRITE`,
so concurrent writes of one correctly computed hash are idempotent. An uncommitted
transaction is rolled back if its process dies. This guarantee does not extend to
network filesystems or to a separate application's mutable metadata databases.

Processes sharing Hashtree's application store should not construct
`LmdbBlobStore` from `data_dir/blobs` themselves. Use the canonical opener with
the same configured storage budget as the application:

```rust
use hashtree_lmdb::open_shared_lmdb_blob_store;
use std::sync::Arc;

let store = Arc::new(open_shared_lmdb_blob_store(data_dir, max_size_bytes)?);
```

The returned `ConfiguredLmdbBlobStore` implements `Store`. A fresh shared store is
a `PoolStore` with a persistent catalog and one default member. Existing single
LMDB stores remain single stores instead of being silently reclassified. The old
hot/legacy environment variables are recognized only as a migration bridge.

The pool is one application-owned write destination. Its LMDB members are opaque
storage resources, and the pool exclusively chooses one member for every blob.
It persists exact hash placement and member identity, centrally verifies SHA-256
on writes, reads, and moves, and keeps pin/access metadata in the shared catalog.
Read-only `BlobRouter` code treats the complete pool as one route; it does not
select pool members.

Member configuration is explicit and persisted. Operators can override:

- logical capacity, which controls capacity-proportional placement;
- LMDB map size, independently of capacity for external-pack-backed members;
- external pack directory, spill threshold, pack target, and sync behavior; and
- per-member read and write concurrency limits.

These are guardrails, not media classes. The pool does not label a path SSD, HDD,
local, or remote. Per-process latency, throughput, reliability, cooldown, and
occasional exploration influence placement among eligible members, but that
bounded learning is disposable and correctness never depends on it.

Use the CLI to inspect and change membership:

```text
htree storage pool status
htree storage pool add PATH --capacity-gb N [member overrides]
htree storage pool configure MEMBER_ID [--capacity-gb N] [--max-reads N] [--max-writes N]
htree storage pool drain MEMBER_ID
htree storage pool maintain --max-items N
htree storage pool remove MEMBER_ID
```

Planned removal is `drain`, repeated bounded `maintain`, then `remove`. Every move
is read, hash-verified, written, hash-verified, catalog-committed, and only then
deleted from the source. `remove` refuses a non-empty member. Sudden loss is not
presented as redundancy: hashes placed only on the unavailable member fail until
that exact member identity returns or another source can supply the bytes.

The immutable blob invariant is narrower than the mutable application state:

- `LmdbBlobStore` callers must provide the SHA-256 key for bytes they write. The
  pooled store recomputes it at its boundary, while retrieval layers still
  hash-verify any bytes received from another route before return or caching.
- Access order, aggregate statistics, pin counts, and garbage collection changes
  are transactional shared state. Pin counts are durable application references,
  not process leases: a process dying does not undo a committed pin.
- Processes allowed to run garbage collection must use one agreed quota policy
  because eviction affects every process sharing the catalog. Placement capacity
  is not a second application-level quota or write policy.
- Every process must use the canonical opener. Stable identity markers prevent a
  missing mount or newly recreated directory from masquerading as a member.

All simultaneously running processes must open each member with the same,
sufficient map size. If another process enlarges a map after a process has opened
it, that older handle reports `MDB_MAP_RESIZED`; it must exit and reopen before
accessing the enlarged environment. Establish a larger map before starting the
other processes. Membership and concurrency changes are generation-tracked and
refresh safely across live processes; LMDB map resizing is deliberately not a live
membership operation.

For online imports, `htree storage pool migrate-lmdb` opens the source read-only,
never resizes it, scans in bounded hash order, and writes only through the target
pool. Persist its cursor with `--state-file`; replay is idempotent. Because a live
source can add a hash before the saved cursor, run another complete pass after
stopping source writes, then verify the final destination before cutover.

## Features

- Memory-mapped I/O for fast reads
- ACID transactions
- Multiprocess-safe committed reads and idempotent blob writes
- Crash recovery for unfinished LMDB transactions

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
