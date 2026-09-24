# Hashtree for TypeScript and JavaScript

Store files and directories by content hash, keep them encrypted by default, and
read the same data from local storage, Blossom servers, or peers. The SDK is
browser-first, uses ES modules, and includes TypeScript declarations. Core storage
also works in modern Node.js with Web Crypto; no daemon or Nostr account is needed
for local use.

- [Getting started](GETTING_STARTED.md): runnable examples, persistence, and app data.
- [API reference](API.md): searchable documentation for all packages and public subpaths.
- [Protocol](../docs/HTS-01.md): storage format and interoperability.

## Install

Install the current core library (`0.3.2`) directly from npm:

```bash
npm install @hashtree/core
```

Current `@hashtree/index`, `@hashtree/collection`, `@hashtree/nostr`, and
`@hashtree/nostr-pubsub` versions are also on npm and install their Hashtree
dependencies automatically. For `@hashtree/fips-transport`, follow its
[installation instructions](packages/hashtree-fips-transport/README.md#install)
to include the FIPS peers.

For current dexie, git, merge, mesh, and worker packages, use their immutable
archives from the [TypeScript runtime 0.5.7 release](https://github.com/mmalmi/hashtree/releases/tag/hashtree-ts-runtime-v0.5.7);
their npm registry versions are older. With npm 12+, add `--allow-remote=all`
to `npm install` and `npm ci` commands that use release URL dependencies.

## First file

```typescript
import { HashTree, MemoryStore, nhashEncode } from '@hashtree/core';

const tree = new HashTree({ store: new MemoryStore() });
const { cid } = await tree.putFile(new TextEncoder().encode('Hello, hashtree!'));
const bytes = await tree.readFile(cid);
if (!bytes) throw new Error('File is unavailable');
console.log(new TextDecoder().decode(bytes));

// Includes the decryption key: anyone with this identifier can read the file
// if they can retrieve its stored blocks.
const permalink = nhashEncode(cid);
```

`MemoryStore` lasts only for the life of the process or page. Storing a file there
does not upload it anywhere. The [getting-started guide](GETTING_STARTED.md)
continues with directories, immutable edits, streaming, and persistent storage.

## Choose packages

Start with core and add the layers your application needs. The package READMEs
provide integration examples; the [API reference](API.md) covers signatures.

| Package | Use it for |
| --- | --- |
| [@hashtree/core](packages/hashtree/README.md) | Files, directories, encryption, streaming, and memory/Blossom storage |
| [@hashtree/dexie](packages/hashtree-dexie/README.md) | Persistent browser storage in IndexedDB |
| [@hashtree/index](packages/hashtree-index/README.md) | B-tree indexes, text search, and ranked search |
| [@hashtree/collection](packages/hashtree-collection/README.md) | Publisher-owned records and derived indexes |
| [@hashtree/nostr](packages/hashtree-nostr/README.md) | Live mutable roots and indexed Nostr event collections |
| [@hashtree/nostr-pubsub](packages/hashtree-nostr-pubsub/README.md) | Reading replicated Nostr events from blob stores |
| [@hashtree/mesh](packages/hashtree-mesh/src/index.ts) | Adaptive, hash-verified reads across storage/network routes |
| [@hashtree/fips-transport](packages/hashtree-fips-transport/README.md) | Reliable peer-to-peer blob transport |
| [@hashtree/worker](packages/hashtree-worker/README.md) | Web Workers and apps that run in browsers and Iris shells |
| [@hashtree/git](packages/hashtree-git/README.md) | Git/htree URL and interoperability helpers |
| [@hashtree/merge](packages/hashtree-merge/README.md) | Deterministic path-based overlay merges |

Nostr integration accepts your relay client's subscribe/publish callbacks and
does not require NDK. The worker's optional NDK integrations use the separately
released Iris Kit packages.

## Content, keys, and roots

- A `CID` contains a 32-byte `hash` and, for encrypted content, a `key`.
- `putFile()` chunks and encrypts data by default. Keep the whole CID to read it.
- `nhashEncode(cid)` serializes both hash and key. Sharing it grants read access
  to anyone who can obtain the blocks. Sharing only the hash does **not** decrypt
  an encrypted file.
- Use `putFile(data, { unencrypted: true })` only when you intend to store
  plaintext. Directory encryption is selected independently of child encryption.
- CHK encryption is deterministic, so identical content deduplicates. It does not
  hide equality and does not protect predictable content against guessing.
- Tree edits return a new root CID. Retain that root; existing roots keep their
  original contents. Use a Nostr resolver to advertise a changing root under a
  stable name.

## Storage and network behavior

A `Store` implements `put`, `get`, `has`, and `delete`, with optional `watch` for
newly available blocks. Implement the interface to add a storage backend.
`BlossomStore` handles remote blob storage and accepts a signer for writes;
`DexieStore` provides local browser persistence.

For adaptive network reads, use `StoreBlobRoute` and `BlobRouter` from
`@hashtree/mesh`. Writes target the app-selected store. Routes return verified
bytes or an explicit miss; timeout, cancellation, corruption, and transport
failures remain errors. `FallbackStore` is a simpler best-effort cache adapter;
its `null` result must not be treated as proof of network-wide absence.

Directory operations can wait for blocks to arrive. Pass an `AbortSignal` to
`listDirectory()` and `resolvePath()` when your operation needs cancellation.
A quiet subscription window is not evidence that mutable data does not exist.

FIPS owns peer discovery, signaling, routing, and underlay transport;
`TcpBlobTransport` owns Hashtree blob requests, verification, and cache writes.
See the [network protocol](../docs/NETWORKING.md#blob-protocol-v1) and
[worker guide](packages/hashtree-worker/README.md) for app integration.

## Development

From `ts/` in a repository checkout:

```bash
pnpm install --frozen-lockfile
pnpm run build           # Core library
pnpm test               # Script and core tests
pnpm run lint
pnpm docs:api           # Generate docs/api/index.html
pnpm docs:check         # Build packages, type-check and run guide examples
```

The [API reference guide](API.md) explains offline browsing and CI artifacts.
See [npm publishing](PUBLISHING.md) for the GitHub Actions release workflow.
The SDK packages live here; app development lives in the sibling repositories
listed in the [project overview](../README.md).

## License

MIT
