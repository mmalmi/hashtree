# @hashtree/mesh

Adaptive, hash-verified blob reads across local storage and network routes.
`BlobRouter` is a read router; your app chooses where writes go.

## Install

The current runtime uses `0.3.2`; the npm registry version is older:

```bash
npm install @hashtree/core https://github.com/mmalmi/hashtree/releases/download/hashtree-ts-runtime-v0.5.7/hashtree-mesh-0.3.2.tgz
```

With npm 12+, add `--allow-remote=all`. [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md).

## Read through routes, write locally

```typescript
import { HashTree, MemoryStore, StoreBlobRoute, type Store } from '@hashtree/core';
import { BlobRouter } from '@hashtree/mesh';

const local = new MemoryStore();
const origin = new MemoryStore(); // Replace with a BlossomStore for remote reads.
const router = new BlobRouter([
  new StoreBlobRoute('local', local),
  new StoreBlobRoute('origin', origin),
], { cache: local, requestTimeoutMs: 10_000 });

const store: Store = {
  get: (hash) => router.get(hash),
  put: (hash, data) => local.put(hash, data),
  has: (hash) => local.has(hash),
  delete: (hash) => local.delete(hash),
};
const file = await new HashTree({ store: origin }).putFile(new TextEncoder().encode('From origin'));
const bytes = await new HashTree({ store }).readFile(file.cid);
if (!bytes) throw new Error('File is unavailable');
console.log(new TextDecoder().decode(bytes)); // From origin
console.log(await local.has(file.cid.hash)); // true: verified bytes were cached
```

Use stable, unique route IDs. A `StoreBlobRoute` wraps one store lookup; network
providers can implement `BlobRoute` directly. The router hedges reads, accepts
the first hash-valid result, and bounds route attempts. Its cache receives stored
bytes, so encrypted files remain encrypted there.

`router.get()` returns `null` for explicit misses from the routes searched (or
when no routes exist). Timeouts, invalid bytes, cancellation, and route failures
are errors when no valid result is found. A miss is not proof of absence across
the whole network. For cancellation use
`router.getDetailed(hash, { context: { signal } })`.

In this composition, `has` and `delete` apply only to the local store. A write or
local delete does not upload or delete remote copies. For persistent browser
caching, use [DexieStore](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-dexie/README.md); for managed worker/peer
integration, see [the worker guide](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-worker/README.md).
