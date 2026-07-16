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

The returned `ConfiguredLmdbBlobStore` implements `Store`. It owns the configured
hot and legacy LMDB environments as one composite store, including their distinct
external-blob directories. The helper also owns the `data_dir/blobs` path, map-size
floor, environment flags, and deterministic external-marker fallback, so consumers
do not duplicate those choices. All sharing processes must still receive identical
`HTREE_LMDB_*` environment overrides and `max_size_bytes`; change those options only
while the other processes are stopped.

The immutable blob invariant is narrower than the mutable application state:

- Callers must provide the SHA-256 key for the bytes they write. The backend does
  not recompute it. Retrieval layers must hash-verify bytes before returning or
  caching them.
- Access order, aggregate statistics, pin counts, and garbage collection changes
  are transactional shared state. Pin counts are durable application references,
  not process leases: a process dying does not undo a committed pin.
- `max_bytes` is configured on each store handle. Processes allowed to run garbage
  collection must use one agreed quota policy because eviction affects every
  process sharing the path. Quota decisions read the shared persisted totals rather
  than a process-local byte count.
- Every process using external blobs or packs must use the canonical opener so
  marker paths and hot/legacy tier ownership remain identical.

All simultaneously running processes must open the environment with the same,
sufficient map size. If another process enlarges the map after a process has opened
it, that older handle reports `MDB_MAP_RESIZED`; it must exit and reopen before
accessing the enlarged environment. Establish a larger map before starting the
other processes. The default map size is fixed and intentionally large, so ordinary
same-host sharing does not require runtime resizing.

## Features

- Memory-mapped I/O for fast reads
- ACID transactions
- Multiprocess-safe committed reads and idempotent blob writes
- Crash recovery for unfinished LMDB transactions

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
