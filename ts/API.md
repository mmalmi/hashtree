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

The TypeScript Documentation CI job also provides a `typescript-api-docs` artifact. Download
and extract it, then open `index.html`. Generated HTML is not checked into Git.

## Packages

| Import | Start here | Purpose |
| --- | --- | --- |
| `@hashtree/core` | `HashTree`, `CID`, `Store`, `MemoryStore` | Files, directories, encryption, streaming, and storage adapters |
| `@hashtree/dexie` | `DexieStore` | Persistent browser storage in IndexedDB |
| `@hashtree/index` | `BTree`, `SearchIndex` | Ordered indexes and text search |
| `@hashtree/collection` | `CollectionWriter`, `CollectionSource` | Publisher-owned records and derived indexes |
| `@hashtree/nostr` | `createNostrRefResolver`, `NostrEventStore` | Mutable roots and indexed Nostr events |
| `@hashtree/nostr-pubsub` | Package exports | Nostr event replication over blob stores |
| `@hashtree/mesh` | `BlobRouter` | Hash-verified reads across storage and network routes |
| `@hashtree/fips-transport` | `TcpBlobTransport` | Peer-to-peer blob transport over FIPS |
| `@hashtree/worker` | `HashtreeWorkerClient`, `createHtreeRuntime` | Browser workers and portable app runtime |
| `@hashtree/git` | Package exports | Git/htree interoperability |
| `@hashtree/merge` | Package exports | Deterministic overlay merge primitives |

The reference describes the checked-out source. Released packages can lag behind
that source; use the matching release tag when generating version-specific docs.
See the [getting-started guide](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md)
for runnable examples and package selection.
