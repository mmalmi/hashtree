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

## Event Format

Trees are published as **kind 30064** (parameterized replaceable with label). Readers also accept legacy **kind 30078** roots for compatibility:

```
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

Part of [hashtree-rs](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
