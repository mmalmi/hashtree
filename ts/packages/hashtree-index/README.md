# @hashtree/index

B-Tree index structures for hashtree.

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

## License

MIT
