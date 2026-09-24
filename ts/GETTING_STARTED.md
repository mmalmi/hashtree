# Getting started with Hashtree

This guide goes from a local file to publisher-owned app data. Start with
`@hashtree/core`; add persistent storage, networking, and mutable root discovery
when you need them. See [installation](README.md#install) for package installation
and [API reference](API.md) for complete signatures.

Use a browser bundler such as Vite, or a modern Node.js runtime with Web Crypto
and ES module support. The examples use top-level `await`. In Node, save compiled
JavaScript as `.mjs` or set `"type": "module"` in your application's `package.json`.
TypeScript declarations ship with every package.

## Store, read, and serialize a file

```typescript
import { HashTree, MemoryStore, nhashEncode, nhashDecode } from '@hashtree/core';

const tree = new HashTree({ store: new MemoryStore() });
const { cid } = await tree.putFile(new TextEncoder().encode('Hello, hashtree!'));

// Save the whole CID, including the key, in a portable string.
const identifier = nhashEncode(cid);
const bytes = await tree.readFile(nhashDecode(identifier));
if (!bytes) throw new Error('File is unavailable');
console.log(new TextDecoder().decode(bytes)); // Hello, hashtree!
```

A hash addresses stored bytes; it does not tell a reader where to find them.
This example stores data only in memory. Another client needs access to the same
blocks through persistent storage, Blossom, or a peer transport.

Files are CHK-encrypted by default. The `nhash` above includes the decryption key,
so treat it as a read capability. A hash alone cannot decrypt those bytes.
For plaintext storage, pass `{ unencrypted: true }` to `putFile()`; removing a key
from an encrypted CID does not turn the data into plaintext. CHK deduplicates
identical content and reveals equality, so it does not hide predictable content
from guessing attacks.

## Directories and immutable edits

Each directory entry needs a `name`, the child's complete `cid`, its plaintext
`size`, and a `type`. Use `LinkType.File` for files written with `putFile()`
(including single-chunk files), and `LinkType.Dir` for directories.

```typescript
import { HashTree, MemoryStore, LinkType } from '@hashtree/core';

const tree = new HashTree({ store: new MemoryStore() });
const file = await tree.putFile(new TextEncoder().encode('Hello'));
const original = await tree.putDirectory([
  { name: 'hello.txt', cid: file.cid, size: file.size, type: LinkType.File },
]);

const resolved = await tree.resolvePath(original.cid, 'hello.txt');
if (!resolved) throw new Error('Directory entry is missing');
const bytes = await tree.readFile(resolved.cid);
if (!bytes) throw new Error('File is unavailable');
console.log(new TextDecoder().decode(bytes)); // Hello

const note = await tree.putFile(new TextEncoder().encode('A new note'));
const updated = await tree.setEntry(
  original.cid, [], 'note.txt', note.cid, note.size, LinkType.File,
);
console.log((await tree.listDirectory(original.cid)).length); // 1
console.log((await tree.listDirectory(updated)).length); // 2
```

The empty path `[]` edits the root directory. For a nested directory, use path
segments such as `['notes', '2026']`. Keep the returned root from `setEntry()`,
`removeEntry()`, or `renameEntry()`; these operations never mutate old roots.
Encrypting a directory does not re-encrypt its children, and an unencrypted
directory can expose child keys stored in its entries.

## Stream larger files

Use `createStream()` when bytes arrive incrementally and `readFileStream()` to
consume them without assembling the entire file in memory. The small chunk size
below makes chunking visible; the default is 2 MiB.

```typescript
import { HashTree, MemoryStore } from '@hashtree/core';

const tree = new HashTree({ store: new MemoryStore(), chunkSize: 4 });
const writer = tree.createStream();
await writer.append(new TextEncoder().encode('Hello, '));
await writer.append(new TextEncoder().encode('streaming!'));
const root = await writer.finalize(); // { hash, key, size }, usable as a CID

const decoder = new TextDecoder();
let text = '';
for await (const chunk of tree.readFileStream(root)) {
  text += decoder.decode(chunk, { stream: true });
}
text += decoder.decode();
console.log(text); // Hello, streaming!
writer.clear();
```

For a bounded in-memory read, use `readFile(cid, { maxBytes })`; exceeding the
limit throws. For byte ranges, `readFileRange(cid, start, end)` uses an inclusive
start and exclusive end.

## Persist data in the browser

Install `@hashtree/dexie` from the same runtime release as core. IndexedDB keeps
blocks across page reloads; retain the CID or encoded `nhash` separately so your
app knows which root to open.

```typescript
import { HashTree, nhashEncode } from '@hashtree/core';
import { DexieStore } from '@hashtree/dexie';

const store = new DexieStore('my-hashtree-app');
try {
  const tree = new HashTree({ store });
  const { cid } = await tree.putFile(new TextEncoder().encode('Persistent data'));
  const identifier = nhashEncode(cid); // Save this in your app's root metadata.
  const bytes = await tree.readFile(cid);
  if (!bytes) throw new Error('File is unavailable');
  console.log(new TextDecoder().decode(bytes)); // Persistent data
} finally {
  store.close();
}
```

For remote storage, configure `BlossomStore` with server URLs and a signer for
uploads. A local write does not imply a successful remote upload. For browser
apps that need caching, peer reads, and Iris shell integration, follow the
[worker runtime guide](packages/hashtree-worker/README.md).

## Model app records as collections

Install `@hashtree/collection` for records with stable IDs and derived indexes.
The writer indexes CIDs; you store the original record bytes yourself. Each
publisher owns a collection manifest, while readers can combine sources locally.

```typescript
import { HashTree, MemoryStore } from '@hashtree/core';
import { CollectionWriter, CollectionSource } from '@hashtree/collection';

const store = new MemoryStore();
const tree = new HashTree({ store });
const definition = {
  sourceId: 'my-catalog',
  getId: (item: { id: string; title: string }) => item.id,
  searchIndexes: [{ name: 'title', text: (item: { title: string }) => item.title }],
};
const writer = new CollectionWriter(store, definition);
const item = { id: 'book-1', title: 'Growing a garden' };
const record = await tree.putFile(new TextEncoder().encode(JSON.stringify(item)));
await writer.put(item, record.cid);

const source = new CollectionSource(store, writer.manifest(), definition);
const found = await source.get('book-1');
if (!found) throw new Error('Record is missing');
const bytes = await tree.readFile(found);
if (!bytes) throw new Error('Record is unavailable');
console.log(JSON.parse(new TextDecoder().decode(bytes)).title); // Growing a garden
console.log((await source.search('title', 'garden')).length); // 1
```

When replacing records with derived indexes, pass the previous record to
`writer.replace()` or `writer.put(..., { previous })` so stale index entries can
be removed. Persist and publish the new manifest after writes. Schema defaults,
normalization, and migrations are local conveniences; peers do not have to share
one global schema. See the [collection guide](packages/hashtree-collection/README.md).

## Publish and follow mutable roots

Immutable CIDs never change. To give readers a stable name for the newest root,
use `createNostrRefResolver()` from `@hashtree/nostr` with your relay client's
subscribe/publish callbacks and signer. The resolver publishes kind `30064` root
events and accepts legacy kind `30078` roots. NDK is optional.

Keep root subscriptions open so later updates can arrive. Use
`storeTreeEventSnapshot()` for immutable permalinks or historical/offline captures,
not as a replacement for discovering live state. For Nostr event datasets,
`NostrEventStore.query()` and `streamQuery()` choose the relevant published index.
See the [Nostr guide](packages/hashtree-nostr/README.md) for relay integration.

## Handle unavailable data

- Core file reads can return `null` when blocks are unavailable. Handle that
  explicitly; a local miss does not establish absence across the network.
- `listDirectory()` and `resolvePath()` wait for directory blocks. Pass an
  `AbortSignal`, such as `AbortSignal.timeout(10_000)`, to bound an operation.
- `resolvePath()` returns `null` when a name is absent from a loaded directory.
  A timeout is an error, not a missing entry.
- Mesh routes distinguish explicit misses from timeouts, corruption, and transport
  failures. Handle rejections and cancellation separately from not-found results.

Run `pnpm docs:check` from `ts/` to type-check and execute the examples in this
guide against the workspace packages. The IndexedDB example runs with
`fake-indexeddb` in that Node-based check.
