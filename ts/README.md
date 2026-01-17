# hashtree/ts

TypeScript/JavaScript implementation of hashtree. Includes the core SDK packages and the Iris Files web app.

Part of the hashtree repository. See [../README.md](../README.md) for the project overview and [../rust/README.md](../rust/README.md) for the Rust CLI/daemon.

When published as a single tree, this subtree lives at `/hashtree/ts`.

Blossom-compatible storage with chunking and directory structure. Merkle roots can be published on Nostr to get mutable `npub/path` addresses.

## Design

- **SHA256** hashing via Web Crypto API
- **MessagePack** encoding for tree nodes (deterministic)
- **Dumb storage**: Works with any key-value store (hash → bytes). Unlike BitTorrent, no active merkle proof computation needed—just store and retrieve blobs by hash.
- **2MB chunks** by default (optimized for blossom uploads)

## Packages

- [`packages/hashtree`](packages/hashtree) - Core merkle tree library
- [`packages/hashtree-dexie`](packages/hashtree-dexie) - IndexedDB/Dexie storage adapter
- [`packages/hashtree-index`](packages/hashtree-index) - B-Tree and index structures for hashtree
- [`../apps/iris-files`](../apps/iris-files) - Web/desktop app with Nostr integration (Iris Files + Iris Video)
- [`packages/ndk`](packages/ndk) - Nostr SDK used by the apps
- [`packages/ndk-cache`](packages/ndk-cache) - Cache layer for NDK

## Storage Backends

The `Store` interface is just `get(hash) → bytes` and `put(hash, bytes)`. Implementations:

- `MemoryStore` - In-memory
- `DexieStore` - IndexedDB via Dexie (in `hashtree-dexie`)
- `OpfsStore` - Origin Private File System
- `BlossomStore` - Remote blossom server
- `WebRTCStore` - P2P network (fetches from peers)

## Usage

```typescript
import { MemoryStore, HashTree, toHex } from 'hashtree';

const store = new MemoryStore();
const tree = new HashTree({ store });

// Store a file
const data = new TextEncoder().encode('Hello, World!');
const cid = await tree.put(data);
console.log('Hash:', toHex(cid.hash));

// Read it back
const content = await tree.get(cid);

// Create a directory
const dirCid = await tree.putDirectory([
  { name: 'hello.txt', hash: cid.hash, size: cid.size },
]);

// List directory
const entries = await tree.listDirectory(dirCid.hash);
```

## Encryption (CHK)

All data is encrypted by default using **Content Hash Key (CHK)** encryption:

- Data is encrypted with AES-256-GCM using a key derived from the content hash (~2-3x overhead vs plain)
- The encryption key is stored alongside the hash in the CID (`cid.key`)
- Share the hash alone for public data, or hash+key for private data
- Deduplication still works: identical content produces identical hashes

```typescript
// Encrypted by default
const { cid } = await tree.putFile(data);
console.log('Hash:', toHex(cid.hash));
console.log('Key:', toHex(cid.key));  // Share this for decryption

// Reading requires the key
const content = await tree.readFile(cid);  // cid includes key
```

## Tree Nodes

Every stored item is either raw bytes or a tree node. Tree nodes are MessagePack-encoded with a `type` field:

- `Blob` (0) - Raw data chunk (not a tree node, just bytes)
- `File` (1) - Chunked file: links are unnamed, ordered by byte offset
- `Dir` (2) - Directory: links have names, may point to files or subdirs

Wire format: `{t: LinkType, l: [{h: hash, s: size, n?: name, t: linkType, ...}]}`

## P2P Transport (WebRTC)

The core library is transport-agnostic—any system that can fetch bytes by hash works. `WebRTCStore` is one implementation using WebRTC with Nostr signaling:

```typescript
import { WebRTCStore } from 'hashtree';

const store = new WebRTCStore({
  signer,           // NIP-07 compatible
  pubkey,
  encrypt,          // NIP-04
  decrypt,
  localStore,       // Fallback store
  relays: ['wss://relay.example.com'],
});

await store.start();
const data = await store.get(hash);  // Fetches from peers
```

Falls back to Blossom servers when data isn't found on peers or WebRTC isn't available.

**Data channel protocol**: Just 2 message types, MessagePack-encoded with a type prefix byte:

| Type | Byte | Format | Description |
|------|------|--------|-------------|
| Request | `0x00` | `{h: hash32, htl?: u8}` | Request data by hash |
| Response | `0x01` | `{h: hash32, d: bytes}` | Return data |

**Request forwarding**: Peers forward requests they can't fulfill locally. HTL (Hops-To-Live, default 10) limits propagation depth. Uses Freenet-style probabilistic decrement—each peer randomly decides whether to decrement at HTL boundaries, making it harder to infer request origin.

## Desktop App (Tauri)

Iris Files can be built as a native desktop app using [Tauri](https://tauri.app/):

```bash
cd ../apps/iris-files

# Install dependencies
pnpm install

# Development
pnpm run tauri:dev

# Build for distribution
pnpm run tauri:build
```

Features:
- Native window with system tray
- Autostart on login (configurable in Settings)
- Native file dialogs and notifications

Requirements: [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) (Rust, platform-specific deps)

## Development

From `ts/`:

```bash
pnpm install      # Install dependencies
pnpm test         # Run tests
pnpm run build    # Build
pnpm run dev      # Dev server
pnpm run test:e2e # E2E tests
```

## License

MIT
