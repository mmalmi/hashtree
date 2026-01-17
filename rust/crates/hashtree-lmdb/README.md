# hashtree-lmdb

LMDB-backed content-addressed blob storage for hashtree.

High-performance storage backend using LMDB (Lightning Memory-Mapped Database) for fast key-value storage.

## Usage

```rust
use hashtree_lmdb::LmdbStore;
use hashtree_core::Store;

let store = LmdbStore::new("/path/to/data")?;

// Store a blob
store.put(&hash, &data)?;

// Retrieve a blob
let data = store.get(&hash)?;
```

## Features

- Memory-mapped I/O for fast reads
- ACID transactions
- Crash-resistant

Part of [hashtree-rs](https://files.iris.to/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree).
