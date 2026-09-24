# @hashtree/index

Immutable B-trees and text search over a Hashtree `Store`. For app records with
auto-updated indexes, start with [@hashtree/collection](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-collection/README.md).

```bash
npm install @hashtree/core @hashtree/index
```

[Quickstart](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md) · [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md)

## Ordered key/value index

```typescript
import { MemoryStore } from '@hashtree/core';
import { BTree } from '@hashtree/index';

const index = new BTree(new MemoryStore());
const first = await index.insert(null, 'book:1', 'Garden');
const root = await index.insert(first, 'book:2', 'Orchard');
console.log(await index.get(root, 'book:1')); // Garden

// Range/prefix methods return async iterators, not arrays.
for await (const [key, value] of index.range(root, 'book:1', 'book:3')) {
  console.log(key, value);
}
// book:1 Garden
// book:2 Orchard
console.log(await index.get(first, 'book:2')); // null: old root is unchanged
```

Keys are ordered strings; `range(root, start, end)` includes `start` and excludes
`end`. `prefix(root, prefix)` selects a shared prefix. Retain every returned root
as a complete CID; an insert with `null` starts a new index. Store blocks
persistently and serialize roots with `nhashEncode()` to reopen them later.

`insert()` / `get()` store string values. To index files while retaining their
keys and native tree links, use `insertLink(root, key, cid)` / `getLink(root, key)`
and iterate `linksEntries()` or `prefixLinks()`.

## Ranked document search

```typescript
import { MemoryStore } from '@hashtree/core';
import { RankedSearchIndex } from '@hashtree/index';

const search = new RankedSearchIndex(new MemoryStore());
const segment = await search.buildSegment([
  { id: 'one', fields: { title: 'Offline Nostr', content: 'Local event storage' } },
  { id: 'two', fields: { title: 'Garden', content: 'Growing fruit trees' } },
], {
  fields: {
    title: { boost: 4, lengthNormalization: 0.3 },
    content: { boost: 1, lengthNormalization: 0.75 },
  },
});
const hits = await search.search(segment, 'offline nostr', { operator: 'and' });
console.log(hits.map((hit) => hit.id).join(',')); // one
```

A segment is an immutable snapshot with fielded BM25F ranking and optional
caller-owned string `value`s. Build a new segment to change its documents. Queries
read matching postings and candidate statistics rather than scanning all records.
Text uses NFKC/lowercase normalization; quoted phrases use indexed positions,
limited by `maxTokensPerField` (default 4096). Hashtags are both ordinary words
and exact `#tag` terms. `SearchIndex` remains the simpler incremental keyword API.

## License

MIT
