# hashtree/ts

TypeScript/JavaScript implementation of hashtree. This repo now contains the SDK
packages only; the Iris app workspaces live in sibling repos.

Part of the hashtree repository. See [../README.md](../README.md) for the project overview and [../rust/README.md](../rust/README.md) for the Rust CLI/daemon.

If you are building apps on top of hashtree, start with [GETTING_STARTED.md](GETTING_STARTED.md).

Blossom-compatible storage with chunking and directory structure. Merkle roots can be published on Nostr to get mutable `npub/path` addresses.

## Design

- **SHA256** hashing via Web Crypto API
- **MessagePack** encoding for tree nodes (deterministic)
- **Dumb storage**: Works with any key-value store (hash → bytes). Unlike BitTorrent, no active merkle proof computation needed—just store and retrieve blobs by hash.
- **2MB chunks** by default (optimized for blossom uploads)

## Packages

**npm packages:**
- [`@hashtree/core`](https://www.npmjs.com/package/@hashtree/core) - Core merkle tree library ([source](packages/hashtree))
- [`@hashtree/merge`](https://www.npmjs.com/package/@hashtree/merge) - Deterministic path-based overlay merge primitives ([source](packages/hashtree-merge))
- [`@hashtree/mesh`](https://www.npmjs.com/package/@hashtree/mesh) - Adaptive read-only `BlobRouter` across opaque routes ([source](packages/hashtree-mesh))
- [`@hashtree/nostr`](https://www.npmjs.com/package/@hashtree/nostr) - Nostr ref resolver, event collections, and signed root snapshots ([source](packages/hashtree-nostr))
- [`@hashtree/fips-transport`](https://www.npmjs.com/package/@hashtree/fips-transport) - Hash-verified blobs over reliable TCP/FIPS streams ([source](packages/hashtree-fips-transport))
- [`@hashtree/git`](https://www.npmjs.com/package/@hashtree/git) - Git/htree interoperability helpers ([source](packages/hashtree-git))
- [`@hashtree/dexie`](https://www.npmjs.com/package/@hashtree/dexie) - IndexedDB/Dexie storage adapter ([source](packages/hashtree-dexie))
- [`@hashtree/index`](https://www.npmjs.com/package/@hashtree/index) - B-Tree index structures ([source](packages/hashtree-index))
- [`@hashtree/worker`](https://www.npmjs.com/package/@hashtree/worker) - Modular browser worker runtime, including the browser-side `tree-root` registry subpath export ([source](packages/hashtree-worker))

The worker's optional NDK integrations are consumed from the independently
released Iris Kit packages. Hashtree does not maintain another NDK fork.

## Portable App Runtime

[`@hashtree/worker`](https://www.npmjs.com/package/@hashtree/worker) is the
app-facing runtime for sites that should work both in ordinary browsers and in
[`iris-browser`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser).
Browse the source at
[`hashtree/ts/packages/hashtree-worker`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree/ts/packages/hashtree-worker)
and see the Iris host/runtime notes in
[`iris-browser/apps/iris/README.md`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser/apps/iris/README.md).

## Installation

```bash
npm install @hashtree/core
# Optional:
npm install @hashtree/merge  # Path-based overlay merge primitives
npm install @hashtree/nostr  # Nostr resolver and event collections
npm install @hashtree/fips-transport  # Reliable TCP/FIPS blob transport
npm install @hashtree/dexie  # IndexedDB storage
npm install @hashtree/index  # B-Tree indexes
npm install @hashtree/mesh  # Adaptive read routing
npm install @hashtree/worker  # Worker runtime + tree-root helpers
```

## Storage Backends

The `Store` interface is just `get(hash) → bytes` and `put(hash, bytes)`. Implementations:

- `MemoryStore` - In-memory (in `@hashtree/core`)
- `BlossomStore` - Remote blossom server (in `@hashtree/core`)
- `DexieStore` - IndexedDB via Dexie (in `@hashtree/dexie`)

Writes always target the application-selected `Store`. Reads can adapt a store
with `StoreBlobRoute` and combine it with opaque network routes in the read-only
`BlobRouter` from `@hashtree/mesh`. A network implementation that owns several
servers or peers remains one composite route; the outer router never selects
its internal members. The worker's P2P composite uses the same router
recursively over exact identities advertised by its configured provider; it
does not fetch anonymously or infer routes from connected peers.

`BlobRouter` bounds route attempts and concurrency, passes nested routes a
deadline and attempt budget, and accepts the first centrally hash-verified
reply. A route-local `NoResult` does not cancel other routes. Timeout,
corruption, reset, and unreachable-provider results remain errors. Optional
cache writes go only to the explicitly configured cache store.

P2P remains a separate transport concern. `TcpBlobTransport` reads from exact
authenticated FIPS providers and can sit behind one composite `BlobRoute`.

## Usage

```typescript
import { MemoryStore, HashTree, toHex } from '@hashtree/core';

const store = new MemoryStore();
const tree = new HashTree({ store });

// Store a file (auto-chunked, encrypted by default)
const data = new TextEncoder().encode('Hello, World!');
const { cid } = await tree.putFile(data);
console.log('Hash:', toHex(cid.hash));

// Read it back
const content = await tree.readFile(cid);

// Create a directory
const { cid: dirCid } = await tree.putDirectory([
  { name: 'hello.txt', ...cid },
]);

// List directory
const entries = await tree.listDirectory(dirCid);
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

## P2P Transport (FIPS)

`@hashtree/fips-transport` carries blobs on reliable TCP/FIPS streams. FIPS
owns identity, peer discovery, signaling, routing, and underlay transports;
TCP/FIPS owns ordered delivery, flow control, and segment retransmission;
Hashtree owns peer choice, bounded whole-session retry, hash verification, and
cache writes. Browser providers join the shared `fips-overlay-v1` discovery
fabric by default.

```typescript
import {
  TcpBlobTransport,
  DEFAULT_FIPS_DISCOVERY_APP,
} from '@hashtree/fips-transport';

const transport = new TcpBlobTransport({
  endpoint: fipsNode,
  localStore,
});

console.log(DEFAULT_FIPS_DISCOVERY_APP); // fips-overlay-v1
const data = await transport.get(hash, peerIds);
await transport.close();
```

The protocol uses FIPS service port `39018`. A provider explicitly reports
found or missing. `null` therefore means every attempted provider reported a
miss; timeouts, resets, malformed responses, and mixed miss/failure results stay
errors rather than becoming false absence. See the
[networking protocol](../docs/NETWORKING.md#blob-protocol-v1) for the wire
format.

## Iris App Repos

The extracted app workspaces now live alongside this repo:

- [`iris-apps`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-apps) for portable Iris web apps and `iris-sites`
- [`iris-browser`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/iris-browser) for the native desktop shell
- [`hashtree-cc`](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree-cc) for the landing page app

## Web Viewer

Browse content and git repos at [git.iris.to](https://git.iris.to), for example [hashtree/ts](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree/ts).

Apps can be loaded directly via `nhash` or `npub/path` URLs, bypassing web servers, DNS, SSL certificates, and CDNs entirely. Content is fetched from the P2P network and Blossom servers by hash, verified locally.

## CI Integration

[hashtree-ci](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree-ci) runs CI jobs for hashtree repos. Results are shown in the git UI - commit history displays pass/fail badges similar to GitHub Actions.

Configure trusted runners in `.hashtree/ci.toml`:

```toml
[ci]
[[ci.runners]]
npub = "npub1..."
name = "my-runner"
```

## Development

From `ts/`:

```bash
pnpm install      # Install dependencies
pnpm test         # Run tests
pnpm run build    # Build
```

## License

MIT
