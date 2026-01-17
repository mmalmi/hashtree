# hashtree-fs

Filesystem-based content-addressed blob storage for hashtree.

Simple storage backend that stores blobs as files on disk, organized by hash prefix for efficient lookup.

## Usage

```rust
use hashtree_fs::FsStore;
use hashtree_core::Store;

let store = FsStore::new("/path/to/data")?;

// Store a blob
store.put(&hash, &data)?;

// Retrieve a blob
let data = store.get(&hash)?;
```

## Storage Layout

Blobs are stored in a directory structure based on hash prefix:
```
data/
  ab/
    abcd1234...
  cd/
    cdef5678...
```

Part of [hashtree-rs](https://github.com/mmalmi/hashtree-rs).
