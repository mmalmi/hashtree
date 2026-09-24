# hashtree-core

Encrypted, content-addressed files and directories over an async `Store`.
The core handles hashing, chunking, encryption, and immutable tree edits;
storage, transport, and mutable-name discovery are separate crates.

[Published API](https://docs.rs/hashtree-core/latest/hashtree_core/) · [Crate](https://crates.io/crates/hashtree-core) · [Protocol](https://github.com/mmalmi/hashtree/blob/master/docs/HTS-01.md)

## Install and store a file

In a Rust application:

```bash
cargo add hashtree-core
cargo add tokio --features macros,rt-multi-thread
```

Save this as `src/main.rs` and run `cargo run`. No daemon, server, or Nostr
identity is needed. The examples use Tokio; core streaming uses `futures` traits.

```rust
use hashtree_core::{
    nhash_decode, nhash_encode_full, Cid, HashTree, HashTreeConfig, MemoryStore, NHashData,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tree = HashTree::new(HashTreeConfig::new(Arc::new(MemoryStore::new())));
    let (cid, size) = tree.put(b"Hello, World!").await?;
    assert_eq!(size, 13); // Plaintext byte count, separate from the CID.

    // Persist the complete read capability, including the encryption key.
    let identifier = nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })?;
    let decoded = nhash_decode(&identifier)?;
    let restored = Cid { hash: decoded.hash, key: decoded.decrypt_key };
    let bytes = tree.get(&restored, Some(1024)).await?.ok_or("File is unavailable")?;
    assert_eq!(bytes, b"Hello, World!");
    println!("{}", String::from_utf8(bytes)?);
    Ok(())
}
```

`MemoryStore` lasts only for this process. The identifier does not contain the
blocks or tell a reader where to fetch them. Use persistent storage and save the
root identifier separately to reopen data later.

## Files, keys, and roots

- `put()` returns `(Cid, plaintext_size)`. A `Cid` contains a SHA-256 hash of the
  stored bytes and an optional 32-byte decryption key.
- Keep both hash and key. `nhash_encode_full()` above preserves both;
  `nhash_encode(&hash)` preserves only the hash. For local metadata,
  `cid.to_string()` / `Cid::parse()` also round-trip both as `hash:key` hex.
- Sharing a keyed CID grants read access to anyone who can retrieve the blocks.
  CHK encryption deduplicates equal content, revealing equality and allowing
  guesses of predictable content. It is not randomized encryption.
- `HashTreeConfig::new(store).public()` selects plaintext writes. Removing the
  key from an encrypted CID does not decrypt its data.
- Edits return new roots. Save the returned CID; old roots remain unchanged.
  Directory encryption is independent of child encryption, and a plaintext
  directory may expose child keys stored in its entries.

## Directories and immutable edits

Use `DirEntry::from_cid()` to retain the child's key, set its plaintext size, and
set `LinkType::File` for logical files or `LinkType::Dir` for directories.

```rust
use hashtree_core::{DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tree = HashTree::new(HashTreeConfig::new(Arc::new(MemoryStore::new())));
    let (file, size) = tree.put(b"Hello").await?;
    let original = tree.put_directory(vec![
        DirEntry::from_cid("hello.txt", &file).with_size(size).with_link_type(LinkType::File),
    ]).await?;
    let found = tree.resolve_path(&original, "hello.txt").await?.ok_or("Missing entry")?;
    assert_eq!(tree.get(&found, Some(1024)).await?, Some(b"Hello".to_vec()));

    let (note, size) = tree.put(b"A new note").await?;
    let updated = tree.set_entry(&original, &[], "note.txt", &note, size, LinkType::File).await?;
    assert_eq!(tree.list_directory_required(&original).await?.len(), 1);
    assert_eq!(tree.list_directory_required(&updated).await?.len(), 2);
    Ok(())
}
```

The empty path `&[]` edits the root directory; `&["notes", "2026"]` targets a
nested directory. `remove_entry()` and `rename_entry()` also return new roots.
These operations do not merge concurrent writers automatically.

## Streaming and bounded reads

Add `futures` with `cargo add futures` for this example. The small chunk size
makes chunking visible; the default is 2 MiB.

```rust
use futures::{io::Cursor, TryStreamExt};
use hashtree_core::{HashTree, HashTreeConfig, MemoryStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = HashTreeConfig::new(Arc::new(MemoryStore::new())).with_chunk_size(4);
    let tree = HashTree::new(config);
    let (cid, size) = tree.put_stream(Cursor::new(b"Hello, streaming!".to_vec())).await?;
    let mut stream = tree.get_stream(&cid);
    let mut received = 0;
    while let Some(chunk) = stream.try_next().await? {
        received += chunk.len() as u64; // Process or write each chunk here.
    }
    assert_eq!(received, size);
    assert_eq!(tree.read_file_range_cid(&cid, 7, Some(16)).await?, Some(b"streaming".to_vec()));
    Ok(())
}
```

`put_stream()` accepts `futures::io::AsyncRead`, not Tokio's identically named
trait; adapt a Tokio reader using `tokio_util::compat` with its `compat` feature.
`get(cid, Some(max_bytes))` limits assembled plaintext. For encrypted ranges,
use `read_file_range_cid()`: start is inclusive, end is exclusive. The raw
`Store::get_range()` instead uses an **inclusive** end on stored bytes.

## Storage and other crates

| Need | Crate / entry point |
| --- | --- |
| Persistent local blocks | [hashtree-lmdb](https://github.com/mmalmi/hashtree/blob/master/rust/crates/hashtree-lmdb/README.md): write/reopen example and shared-store policy |
| Remote blob uploads/downloads | [hashtree-blossom](https://github.com/mmalmi/hashtree/blob/master/rust/crates/hashtree-blossom/README.md): signed HTTP client |
| Adaptive network reads | [hashtree-network](https://github.com/mmalmi/hashtree/blob/master/rust/crates/hashtree-network/README.md): route selection and verification |
| Records and derived indexes | [hashtree-collection](https://github.com/mmalmi/hashtree/blob/master/rust/crates/hashtree-collection/README.md) |
| Mutable Nostr root names | [hashtree-resolver](https://github.com/mmalmi/hashtree/blob/master/rust/crates/hashtree-resolver/README.md) |

A custom `Store` implements async `put`, `get`, `has`, and `delete` and must be
`Send + Sync`; other trait methods have defaults. It stores raw addressed bytes,
not logical plaintext files. Preserve immutable hash-to-bytes mappings and verify
untrusted bytes before returning/caching them. `put()` returning `false` means
already present, not an upload failure. Flush buffered stores before publishing
roots, and apply the backend's durability policy. Local writes do not imply
remote uploads; replicate all referenced blocks before announcing a root.

## Handle unavailable data

- `get()` returns `Ok(None)` for a missing root, and may return `Err` for missing
  descendants, invalid encoding, decryption, storage, or size-limit failures.
- `list_directory_required()` rejects unavailable directory blocks.
  `list_directory()` and `list()` can return empty results for unavailable roots;
  do not use those empty results as proof of an empty remote directory.
- `resolve_path()` returns `Ok(None)` for an absent name/non-directory path and
  reports missing directory blocks as errors.
- `get_stream()` can end without chunks when the root is unavailable. Check the
  expected plaintext length when completeness matters; zero chunks alone do
  not establish whether the file exists.
- Keep transport failures separate from misses. Apply deadlines at the app or
  transport layer, and keep mutable-root subscriptions open until explicitly stopped.

## API and verification

The [online API](https://docs.rs/hashtree-core/latest/hashtree_core/) follows
published crate versions. Select the version matching `Cargo.lock`. For the
checkout's API, run these commands from `rust/`:

```bash
cargo doc --locked -p hashtree-core --no-deps --open
cargo test --locked -p hashtree-core --doc
```

This README is also the crate's generated documentation. Cargo compiles and runs
its Rust examples as doctests, including in the full release/CI gate. Use the
public `HashTree`, `Cid`, `DirEntry`, and `Store` APIs for app code; low-level
codec/builder helpers are for protocol and storage integrations.
