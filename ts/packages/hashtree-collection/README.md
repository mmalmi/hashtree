# @hashtree/collection

Publisher-owned records with stable IDs, key indexes, and text search. Use this
for app data; use `@hashtree/index` directly only when you need custom indexes.

```bash
npm install @hashtree/core @hashtree/collection
```

[Quickstart](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md) · [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md)

## Write, save, reopen, and update

A writer indexes CIDs; your app encodes and stores the original records. Each
manifest is an immutable snapshot of the index roots, serializable as JSON.
This example uses memory; use a persistent or remote `Store` in an app.

```typescript
import { HashTree, MemoryStore } from '@hashtree/core';
import {
  CollectionWriter, CollectionSource,
  type CollectionDefinition, type CollectionManifest,
} from '@hashtree/collection';

type Book = { id: string; title: string };
const definition: CollectionDefinition<Book> = {
  sourceId: 'my-books',
  getId: (book) => book.id,
  searchIndexes: [{ name: 'title', text: (book) => book.title }],
};
const store = new MemoryStore();
const tree = new HashTree({ store });
const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const writer = new CollectionWriter(store, definition);
const original: Book = { id: 'book-1', title: 'Growing a garden' };
const record = await tree.putFile(encode(original));
await writer.put(original, record.cid);

// Persist this complete manifest and retain its file CID (including the key).
const saved = await tree.putFile(encode(writer.manifest()));
const bytes = await tree.readFile(saved.cid);
if (!bytes) throw new Error('Manifest is unavailable');
const manifest: CollectionManifest = JSON.parse(new TextDecoder().decode(bytes));
const before = new CollectionSource(store, manifest, definition);
const reopened = new CollectionWriter(store, definition, manifest);

const previousCid = await before.get('book-1');
if (!previousCid) throw new Error('Record is missing');
const previousBytes = await tree.readFile(previousCid);
if (!previousBytes) throw new Error('Record is unavailable');
const previous: Book = JSON.parse(new TextDecoder().decode(previousBytes));
const updated: Book = { ...previous, title: 'Orchard handbook' };
const replacement = await tree.putFile(encode(updated));
await reopened.replace(updated, replacement.cid, previous);

const after = new CollectionSource(store, reopened.manifest(), definition);
console.log((await before.search('title', 'garden')).length); // 1: old snapshot
console.log((await after.search('title', 'garden')).length);  // 0: stale term removed
console.log((await after.search('title', 'orchard')).length); // 1
await reopened.delete(updated);
console.log(await new CollectionSource(store, reopened.manifest()).count()); // 0
```

Save the new `writer.manifest()` after each committed change and construct a new
`CollectionSource` to read it. Serialize writes to a given writer. Independent
writers starting from one manifest produce branches; your app must choose or
merge them before announcing a new root.

## Publish a complete source

Persist the record blocks, `byIdRoot`, all index roots, and the manifest before
announcing its CID through a [mutable root](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-nostr/README.md).
A manifest contains hex-encoded CID strings, not native tree links:
`walkBlocks(manifestCid)` alone does **not** copy the referenced indexes or records.
Using the same remote store for the tree and writer sends each write to that
store. With local storage, replicate each referenced root and its descendants
as well as the manifest. Preserve encryption keys; sharing a manifest grants
access to its referenced data when readers can fetch the blocks.

A directory representation can reserve `.collection-manifest.json` for
`collectionManifestMetadataFromManifest()` metadata (`schemaVersion` and
`publishedSchema`). That metadata is **not** the complete collection manifest.

## Query and schema rules

- `source.get(id)` returns a record CID; `source.search(indexName, text)` returns
  ranked links. Fetch and decode the record separately.
- For key indexes, define `keyIndexes: [{ name, keys: (item) => [...] }]` and use
  `queryIndex(name, { prefix, limit })`. Include the record ID in a key when
  multiple records share a value, e.g. `artist:ada:song-1`.
- Indexed updates need `replace(item, cid, previous)` or
  `put(item, cid, { previous })`; deletion needs the previous indexed fields.
- After changing index definitions, use `reindex(entries)` with each canonical
  item and its CID. Existing roots alone cannot reconstruct changed projections.
- `count()` / `exactCount()` count the by-ID tree. `countReported()` reads stored
  subtree counts and may return `null` when no count is available.
- `schema` defaults, normalization, validation, and migrations are local write
  rules. Encode the same normalized item you index. Validate untrusted manifest
  and record JSON in your application; TypeScript types are not runtime checks.
- Custom `searchIndexes[].terms` must also be used when querying: pass the
  definition to `CollectionSource`, or use `searchTerms()` with matching terms.
- `federatedSearch(store, [{ manifest, boost }], indexName, query)` queries source
  snapshots and combines hits by logical ID. Choose an ID namespace intentionally;
  unrelated records with the same ID otherwise collapse. Sources need compatible
  indexes and query normalization, not one universal raw-record schema.
