# hashtree-core

Simple content-addressed merkle tree with KV storage.

This is the core library that implements the merkle tree structure used by hashtree. It provides:

- **SHA256** hashing
- **MessagePack** encoding for tree nodes (deterministic)
- **CHK encryption** by default (Content Hash Key)
- **2MB chunks** by default (optimized for blossom uploads)

## Usage

```rust
use hashtree_core::{HashTree, HashTreeConfig, store::MemoryStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store));

    // Store content (encrypted by default)
    let cid = tree.put(b"Hello, World!").await?;

    // Read it back
    let data = tree.get(&cid).await?;

    Ok(())
}
```

## Tree Nodes

Every stored item is either raw bytes or a tree node. Tree nodes are MessagePack-encoded with a `type` field:

- `Blob` (0) - Raw data chunk
- `File` (1) - Chunked file: links are unnamed, ordered by byte offset
- `Dir` (2) - Directory: links have names, may point to files or subdirs

## Store Trait

The `Store` trait is just `get(hash) → bytes` and `put(hash, bytes)`. Works with any backend that can store/fetch by hash.

Part of [hashtree-rs](https://github.com/mmalmi/hashtree-rs).
