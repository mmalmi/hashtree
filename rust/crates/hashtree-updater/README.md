# hashtree-updater

Rust helpers for app updates published through hashtree.

Apps bake a release reference such as:

```text
htree://npub1.../releases%2Fnostr-vpn/stable/latest
```

The first path segment after the `npub` is the mutable hashtree root name. Slashes
inside that root name are percent-encoded, so `releases%2Fnostr-vpn` resolves the
signed Nostr root `npub1.../releases/nostr-vpn`; the remaining segments are paths
inside the immutable release tree.

Release directories contain a `manifest.json`:

```json
{
  "schema": "hashtree.update.v1",
  "app": "nostr-vpn",
  "version": "1.2.3",
  "channel": "stable",
  "assets": [
    {
      "name": "nostr-vpn-aarch64-apple-darwin.tar.gz",
      "path": "assets/nostr-vpn-aarch64-apple-darwin.tar.gz",
      "targets": ["aarch64-apple-darwin"],
      "size": 12345678
    }
  ]
}
```

The mutable root event is the update authority. Extra binary signatures can still
be shipped as assets when an app wants them, but this crate does not require a
second manifest-signing scheme.

## Example

```rust,no_run
use hashtree_core::{HashTree, HashTreeConfig, MemoryStore};
use hashtree_resolver::nostr::{NostrResolverConfig, NostrRootResolver};
use hashtree_updater::{HashtreeUpdater, UpdateCheckOptions, UpdateRef, UpdateTarget};
use std::sync::Arc;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let store = Arc::new(MemoryStore::new());
let tree = HashTree::new(HashTreeConfig::new(store));
let resolver = NostrRootResolver::new(NostrResolverConfig::default()).await?;
let updater = HashtreeUpdater::new(resolver, tree);

let check = updater
    .check(UpdateCheckOptions {
        reference: UpdateRef::parse("htree://npub1.../releases%2Fnostr-vpn/stable/latest")?,
        current_version: "1.1.0".to_string(),
        target: UpdateTarget::current(),
        ..UpdateCheckOptions::default()
    })
    .await?;

if check.update_available {
    let artifact = updater.download_asset(&check, None).await?;
    // App-specific code can now run an installer, unpack an archive, or hand the
    // bytes to a helper process that swaps the current executable after exit.
    println!("downloaded {} bytes", artifact.bytes.len());
}
# Ok(())
# }
```
