# @hashtree/dexie

IndexedDB storage adapter for hashtree using Dexie.

## Install

```bash
npm install @hashtree/dexie
```

## Usage

```typescript
import { DexieStore } from '@hashtree/dexie';
import { HashTree } from '@hashtree/core';

const store = new DexieStore('my-hashtree-db');
const tree = new HashTree({ store });

// Store persists to IndexedDB
await tree.putFile(data);
```

## Features

- Persistent browser storage
- LRU eviction support
- Automatic schema migrations

## API

```typescript
const store = new DexieStore(dbName?: string);

await store.get(hash);
await store.put(hash, data);
await store.has(hash);
await store.delete(hash);
await store.keys();
await store.clear();
await store.count();
await store.totalBytes();
await store.evict(maxBytes);
store.close();
```

## License

MIT
