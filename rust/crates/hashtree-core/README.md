# hashtree-core

Simple content-addressed merkle tree with KV storage.

This is the core library that implements the merkle tree structure used by hashtree. It provides:

- **SHA256** hashing
- **MessagePack** encoding for tree nodes (deterministic)
- **CHK encryption** by default (Content Hash Key)
- **2MB chunks** by default (optimized for blossom uploads)

## Usage

In a Rust application:

```bash
cargo add hashtree-core
cargo add tokio --features macros,rt-multi-thread
```

Save this as `src/main.rs` and run `cargo run`:

```rust
use hashtree_core::{HashTree, HashTreeConfig, store::MemoryStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store));

    // Store content (encrypted by default)
    let (cid, _size) = tree.put(b"Hello, World!").await?;

    // Read it back
    let data = tree.get(&cid, None).await?;
    let data = data.ok_or("File is unavailable")?;
    println!("{}", String::from_utf8(data)?);

    Ok(())
}
```

Keep the complete `Cid` (hash and encryption key). `MemoryStore` lasts only for
this process; readers need the same blocks and the key. For plaintext storage,
construct the tree with `HashTreeConfig::new(store).public()` instead. Immutable
edits return a new root; retain that root to read the updated tree.

The complete API is generated from Rust signatures and examples. From `rust/`,
run `cargo doc -p hashtree-core --no-deps --open`. See also the
[TypeScript guide](../../../ts/GETTING_STARTED.md) for the shared data model and
the [wire protocol](../../../docs/HTS-01.md) for interoperability.

## Tree Nodes

Every stored item is either raw bytes or a tree node. Tree nodes are MessagePack-encoded with a `type` field:

- `Blob` (0) - Raw data chunk
- `File` (1) - Chunked file: links are unnamed, ordered by byte offset
- `Dir` (2) - Directory: links have names, may point to files or subdirs

## Store Trait

The `Store` trait is just `get(hash) → bytes` and `put(hash, bytes)`. Works with any backend that can store/fetch by hash.

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
