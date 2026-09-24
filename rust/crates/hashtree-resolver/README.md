# hashtree-resolver

Resolve mutable Nostr names (`npub1…/tree-name`) to immutable Hashtree `Cid`s.
This discovers roots; your application's store/transport must fetch their blocks.

[Published API](https://docs.rs/hashtree-resolver/latest/hashtree_resolver/) · [Core guide](https://github.com/mmalmi/hashtree/blob/master/rust/crates/hashtree-core/README.md)

## Install and resolve a root

The default crate provides the `RootResolver` trait. Enable `nostr` for the
Nostr implementation:

```bash
cargo add hashtree-core
cargo add hashtree-resolver --features nostr
cargo add tokio --features macros,rt-multi-thread
```

Save this as `src/main.rs`. Run `cargo run -- 'npub1…/tree-name'`, substituting
an actual author and tree name. Select relays that carry the author's events.
This example is compiled as a doctest; it does not run against public relays.

```rust,no_run
use hashtree_resolver::{
    nostr::{NostrResolverConfig, NostrRootResolver},
    RootResolver,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::args().nth(1).ok_or("Pass npub/tree-name as an argument")?;
    let resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: vec!["wss://relay.damus.io".to_string()],
        ..Default::default()
    }).await?;

    // Keep listening until a usable root arrives, even if the relays are slow.
    let result = resolver.resolve_wait(&key).await;
    resolver.stop().await?;
    let cid = result?;
    println!("Resolved root hash: {}", hashtree_core::to_hex(&cid.hash));
    // Keep the complete CID (including its key) for HashTree reads.
    Ok(())
}
```

`resolve_wait()` waits indefinitely; an application can cancel the future or
apply its own deadline, then call `stop()` when finished with the resolver.
`resolve()` instead returns `Result<Option<Cid>, ResolverError>` after a bounded
lookup. `None` means no usable root was found in that lookup, not proof that the
tree does not exist.

For live apps, use `subscribe(key).await?` and keep receiving with `rx.recv().await`.
Its initial `None` means no root is cached yet: keep the subscription open.
Updates may also be `None` when an event has no usable key. Drop the receiver when
the app no longer needs updates, and call `stop()` on application shutdown.

Pass the full tree name as the resolver key; tree names may themselves contain
slashes. Resolve paths inside the selected tree with `HashTree::resolve_path()`.
For URL parsing, see [URL encoding](https://github.com/mmalmi/hashtree/blob/master/docs/URL-ENCODING.md).

## Publishing and visibility

Reading public roots needs no secret key. Set `NostrResolverConfig::secret_key`
to the owner's persisted `Keys` to publish or read that owner's private roots.
Upload/replicate the tree's blocks before announcing its root.

- `publish(key, &cid)` exposes the CID's read key in a public event.
- `publish_shared(key, &cid, &share_secret)` masks the key for link holders;
  readers use `resolve_shared(key, &share_secret)`.
- `publish_private(key, &cid)` encrypts the key to the owner.

The CID and sharing secret are read capabilities. Preserve them and only share
with intended readers. A successful root announcement does not upload blocks.

## Event Format

Trees are published as **kind 30064** (parameterized replaceable with label). Readers also accept legacy **kind 30078** roots for compatibility:

```text
npub1abc.../treename/path/to/file.ext
      │        │           │
      │        │           └── Path within merkle tree (client-side traversal)
      │        └── d-tag value (tree identifier)
      └── Author pubkey (bech32 → hex for event)
```

**Tags:**
| Tag | Purpose |
|-----|---------|
| `d` | Tree name (replaceable event key) |
| `l` | `"hashtree"` label for discovery |
| `hash` | Merkle root SHA256 (64 hex chars) |
| `key` | Decryption key (public trees) |
| `encryptedKey` | XOR'd key (link-visible trees) |
| `selfEncryptedKey` | NIP-44 encrypted (private/link-visible) |

**Visibility:**
- **Public**: plaintext `key` tag
- **Link-visible**: `encryptedKey` + link key in share URL
- **Private**: only `selfEncryptedKey` (owner access)

## API and verification

From `rust/`, run:

```bash
cargo test --locked -p hashtree-resolver --features nostr --doc
cargo doc --locked -p hashtree-resolver --features nostr --no-deps --open
```

This guide is also the `nostr` module's generated documentation. The published
API follows the selected crate release.
