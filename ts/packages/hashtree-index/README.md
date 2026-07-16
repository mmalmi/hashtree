# @hashtree/index

B-Tree index structures for hashtree.

Most app code should treat this package as the low-level primitive layer. For app-facing collection/query logic, prefer `@hashtree/collection` or `@hashtree/nostr` so index selection stays in the library instead of being reinvented in each app.

## Install

```bash
npm install @hashtree/index
```

## Usage

```typescript
import { BTree } from '@hashtree/index';
import { MemoryStore } from '@hashtree/core';

const store = new MemoryStore();
const btree = new BTree(store);

// Insert key-value pairs
const root = await btree.insert(null, 'key1', value1Hash);
const root2 = await btree.insert(root, 'key2', value2Hash);

// Lookup
const result = await btree.get(root2, 'key1');

// Range query
const entries = await btree.range(root2, 'a', 'z');
```

## Features

- Immutable B-Tree (each mutation returns new root)
- Content-addressed nodes
- Efficient range queries
- Configurable branching factor

## Ranked document segments

`RankedSearchIndex` builds immutable fielded-search segments without changing the
legacy `SearchIndex` API. A segment root links separate B-trees for postings,
per-term document frequency, per-document field lengths, and optional stored
values. Queries read only the requested term postings and candidate statistics;
they do not scan stored documents.

```typescript
import { RankedSearchIndex } from '@hashtree/index';

const search = new RankedSearchIndex(store);
const segment = await search.buildSegment(events, {
  fields: {
    title: { boost: 4, lengthNormalization: 0.3 },
    content: { boost: 1, lengthNormalization: 0.75 },
  },
});

const hits = await search.search(segment, 'offline nostr', { operator: 'and' });
```

The `hashtree/ranked-search-segment@1` manifest records NFKC/lowercase
normalization, BM25F parameters, field/corpus length statistics, and positional
field limits. Quoted phrases are strict and use every position retained within
the configured field token limit. Hashtags are indexed both as ordinary terms
and as exact `#tag` terms.

## License

MIT
