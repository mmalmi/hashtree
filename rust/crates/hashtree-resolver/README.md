# hashtree-resolver

Root resolver for hashtree - maps human-readable keys to merkle root hashes.

Resolves `npub/path` style addresses to merkle root hashes by querying Nostr relays.

## Usage

```rust
use hashtree_resolver::{NostrRootResolver, NostrResolverConfig, RootResolver};

let config = NostrResolverConfig {
    relays: vec!["wss://relay.damus.io".to_string()],
    ..Default::default()
};
let resolver = NostrRootResolver::new(config).await?;

// Resolve npub/treename to hash
let entry = resolver.resolve("npub1.../myrepo").await?;
println!("Root hash: {}", entry.root_hash);
```

## Nostr Events

Uses Nostr kind 30078 (NIP-78) events to store tree references:
- `d` tag: tree name
- Content: JSON with root hash, optional encryption key

Part of [hashtree-rs](https://github.com/mmalmi/hashtree-rs).
