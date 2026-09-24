# TypeScript API reference

The API reference is generated with TypeDoc from each SDK package's public
exports, TypeScript signatures, and source comments. It includes the exported
subpaths, such as `@hashtree/worker/runtime` and `@hashtree/fips-transport/browser`.
Private implementation members are omitted.

## Browse the reference

From the repository's `ts/` directory:

```bash
pnpm install --frozen-lockfile
pnpm docs:api
```

Open `docs/api/index.html` in your browser. The generated static site works
offline; it does not require a server. Use its search box to find a class,
function, or type, then follow the signature links for parameters and results.
Source links point to the commit used to generate the reference.

The TypeScript Documentation job in [GitHub CI](https://github.com/mmalmi/hashtree/actions/workflows/ci.yml)
also provides a `typescript-api-docs` artifact. Download
and extract it, then open `index.html`. Generated HTML is not checked into Git.

## Packages

| Import | Start here | Purpose |
| --- | --- | --- |
| `@hashtree/core` | `HashTree`, `CID`, `Store`, `MemoryStore` | Files, directories, encryption, streaming, and storage adapters |
| `@hashtree/dexie` | `DexieStore` | Persistent browser storage in IndexedDB |
| `@hashtree/index` | `BTree`, `SearchIndex` | Ordered indexes and text search |
| `@hashtree/collection` | `CollectionWriter`, `CollectionSource` | Publisher-owned records and derived indexes |
| `@hashtree/nostr` | `createNostrRefResolver`, `NostrEventStore` | Mutable roots and indexed Nostr events |
| `@hashtree/nostr-pubsub` | `HashtreeNostrEventReader` | Verified queries across replicated Nostr event indexes |
| `@hashtree/mesh` | `BlobRouter` | Hash-verified reads across storage and network routes |
| `@hashtree/fips-transport` | `TcpBlobTransport` | Peer-to-peer blob transport over FIPS |
| `@hashtree/worker` | `HashtreeWorkerClient`, `createHtreeRuntime` | Browser workers and portable app runtime |
| `@hashtree/git` | `parseHtreeUrl`, `resolveHtreeRootCid` | Git/htree URL and visibility helpers |
| `@hashtree/merge` | `mergePathSources` | Deterministic overlays with precedence and tombstones |

The reference describes the checked-out source. Released packages can lag behind
that source; use the matching release tag when generating version-specific docs.
See the [getting-started guide](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md)
for runnable examples and package selection.

## Implementing an app

Read the [quickstart](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md)
for the data model, then the relevant package recipe:

| Task | Recipe |
| --- | --- |
| Signed remote upload, fresh-client read | [Core / Blossom](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree/README.md#remote-storage-with-blossom) |
| Save, reopen, update, and search records | [Collection](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-collection/README.md) |
| Publish and follow a changing root | [Nostr](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-nostr/README.md) |
| Combine local storage and peer reads | [Mesh](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-mesh/README.md) |
| Browser worker and shell integration | [Worker](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-worker/README.md) |

For humans and coding agents, the installed package's declarations are the
authority for a released version: follow `exports` and `types` in
`node_modules/@hashtree/<package>/package.json` to its `.d.ts` files. Import only
public exports; avoid internal `src/` or `dist/` paths. Check your lockfile before
using an API from newer checkout docs. Most examples are standalone; integration
functions explicitly take the server URLs, signer, and app callbacks they need.
