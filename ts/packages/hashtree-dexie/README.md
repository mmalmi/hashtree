# @hashtree/dexie

Persistent browser storage for Hashtree, backed by IndexedDB via Dexie.

## Install

The current runtime uses `0.1.10`; the npm registry version is older:

```bash
npm install @hashtree/core https://github.com/mmalmi/hashtree/releases/download/hashtree-ts-runtime-v0.5.7/hashtree-dexie-0.1.10.tgz
```

With npm 12+, add `--allow-remote=all`. See [SDK installation](https://github.com/mmalmi/hashtree/blob/master/ts/README.md#install)
for the release policy and [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md) for signatures.

## Usage

```typescript
import { HashTree, nhashEncode } from '@hashtree/core';
import { DexieStore } from '@hashtree/dexie';

const store = new DexieStore('my-hashtree-db');
try {
  const tree = new HashTree({ store });
  const { cid } = await tree.putFile(new TextEncoder().encode('Persistent data'));
  const identifier = nhashEncode(cid); // Save separately in app settings.
  console.log(identifier.startsWith('nhash1')); // true
} finally {
  store.close();
}
```

Reopen with the same database name and decode your saved identifier with
`nhashDecode()`; see the [complete reopen example](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md#persist-data-in-the-browser).
IndexedDB stores the blocks, not your app's current root pointer. A hash alone
loses the encryption key. Browser storage can be cleared or evicted; keep another
copy of data you need to retain.

## Store operations

| Method | Purpose |
| --- | --- |
| `get(hash)`, `put(hash, bytes)`, `has(hash)`, `delete(hash)` | Raw stored blocks; use `HashTree` for logical files |
| `keys()`, `count()`, `totalBytes()` | Inspect stored blocks and their total size |
| `evict(maxBytes)` | Remove least recently used blocks to meet the byte budget |
| `clear()` | Delete every block in this database |
| `close()` | Release this database connection |

Eviction is block-based, not tree-aware: it may remove blocks still referenced by
a saved root. Use it for a recoverable cache, not your only authoritative copy.

## License

MIT
