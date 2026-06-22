# @hashtree/nostr

Nostr ref resolving, replaceable publish helpers, signed root snapshots, and
event collections for hashtree.

For app-builder guidance and common pitfalls, see [../../GETTING_STARTED.md](../../GETTING_STARTED.md).

## Install

```bash
npm install @hashtree/nostr
```

## Nostr Event Collections

Use `NostrEventStore` when your app wants a hashtree-native Nostr event collection instead of inventing its own query API.

```typescript
import { MemoryStore } from '@hashtree/core';
import { NostrEventStore } from '@hashtree/nostr';

const store = new MemoryStore();
const events = new NostrEventStore(store);

const profileNotes = await events.query(rootCid, {
  authors: pubkey,
  kinds: [1],
}, { limit: 50 });

for await (const event of events.streamQuery(rootCid, {
  authors: pubkey,
  tags: { t: 'hashtree' },
})) {
  console.log(event.id, event.content);
}
```

`query()` and `streamQuery()` choose the best published index they can (`by-author`, `by-author-kind`, `by-kind`, `by-tag`, or recent) so app code does not need to hand-roll index selection.

## P2P Transport

P2P blob fetching is provided by `@hashtree/fips-transport`. FIPS owns peer
discovery, signaling, and WebRTC/UDP links; Hashtree carries verified mesh blob
frames over the FIPS node endpoint.

## Nostr Ref Resolver

Resolve `npub/treename` references to merkle root hashes via Nostr events.

### Event Format

Trees are published as **kind 30064** (parameterized replaceable with label). Readers also accept legacy **kind 30078** roots for compatibility:

```
npub1abc.../treename/path/to/file.ext
      │        │           │
      │        │           └── Path within merkle tree (client-side traversal)
      │        └── d-tag value (tree identifier)
      └── Author pubkey (bech32 → hex for event)
```

**Tags:**
| Tag | Purpose |
|-----|---------|
| `d` | Tree name (replaceable event key) |
| `l` | `"hashtree"` label for discovery |
| `hash` | Merkle root SHA256 (64 hex chars) |
| `key` | Decryption key (public trees) |
| `encryptedKey` | XOR'd key (link-visible trees) |
| `selfEncryptedKey` | NIP-44 encrypted (private/link-visible) |

**Visibility:**
- **Public**: plaintext `key` tag
- **Link-visible**: `encryptedKey` + link key in share URL
- **Private**: only `selfEncryptedKey` (owner access)

### Usage

```typescript
import { createNostrRefResolver } from '@hashtree/nostr';

const resolver = createNostrRefResolver({
  subscribe: (filters, onEvent) => { /* your relay client subscribe callback */ },
  publish: (event) => { /* your relay client publish callback */ },
});

const root = await resolver.resolve('npub1.../myfiles');
```

The resolver does not require NDK. Any raw relay client is fine as long as it can subscribe and publish signed events.

### Coalescing Replaceable Publishes

When app code signs replaceable events directly, publishing several updates inside one second can leave relays choosing by event id instead of the last UI state. `createReplaceablePublishQueue()` avoids app-side future timestamps by serializing publishes per replaceable coordinate and only sending the latest queued update in a one-second window.

```typescript
import {
  createReplaceablePublishQueue,
  HASHTREE_ROOT_KIND,
  replaceableEventCoordinateFromTemplate,
} from '@hashtree/nostr';

const publishQueue = createReplaceablePublishQueue();

await publishQueue.publish({
  coordinate: replaceableEventCoordinateFromTemplate(pubkey, {
    kind: HASHTREE_ROOT_KIND,
    tags: [['d', treeName]],
  }),
  publish: async (createdAt) => {
    const signed = await signEvent({
      kind: HASHTREE_ROOT_KIND,
      created_at: createdAt,
      tags: [['d', treeName], ['hash', rootHash]],
      content: '',
    });
    return publishSignedEvent(signed);
  },
});
```

## Signed Tree Snapshots

For immutable permalinks, store a copy of the signed root event as a plain hashtree blob. The snapshot gives you one signed root even when relays do not answer, and you can still watch for newer events later.

For live mutable app data, prefer resolving the current root from relays first. Snapshots are for permalinks, offline reuse, and signed historical captures, not for replacing a live source lookup.

```typescript
import {
  storeTreeEventSnapshot,
  readTreeEventSnapshot,
  fetchLatestTreeEventSnapshot,
  watchLatestTreeEventSnapshot,
} from '@hashtree/nostr';
import { HashTree } from '@hashtree/core';

const hashTree = new HashTree({ store });

const snapshot = await storeTreeEventSnapshot(hashTree, nip19, signedRootEvent);
const sameSnapshot = snapshot
  ? await readTreeEventSnapshot(hashTree, nip19, snapshot.snapshotCid)
  : null;

const latest = await fetchLatestTreeEventSnapshot(
  { snapshotTarget: hashTree, nip19, fetchEvents },
  'npub1...owner',
  'videos/demo',
);

const stop = watchLatestTreeEventSnapshot(
  { snapshotTarget: hashTree, nip19, fetchEvents, subscribeEvents },
  'npub1...owner',
  'videos/demo',
  (nextSnapshot) => {
    console.log(nextSnapshot.snapshotNhash, nextSnapshot.rootCid);
  },
);

// later
stop();
```

The library does not keep a global snapshot cache for you. It provides stateless helpers plus a live watcher; route caching and reuse policy stay with the app. Pass a `HashTree` when you already have one controlling write policy, or a raw `Store` when the default wrapper is enough.

Snapshot routes use the signed snapshot blob `nhash` plus a path and optional link key:

```typescript
import {
  buildTreeEventSnapshotPermalink,
  parseTreeEventSnapshotPermalink,
} from '@hashtree/nostr';

const href = buildTreeEventSnapshotPermalink({
  snapshotNhash: snapshot.snapshotNhash,
  path: ['index.html'],
  linkKey: 'abcd...optional 64-hex link key',
});
// nhash1.../index.html?snapshot=1&k=...

const parsed = parseTreeEventSnapshotPermalink(`htree://${href}`);
```

## License

MIT
