# @hashtree/nostr

Publish and follow mutable roots, store indexed Nostr events, and capture signed
root snapshots. Blob storage/transport is separate; Nostr carries the pointer.

```bash
npm install @hashtree/core @hashtree/nostr nostr-tools
```

[Quickstart](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md) · [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md)

## Publish and follow a root

This integration function uses `nostr-tools`' `SimplePool`. Supply your relay
URLs, tree name, signer's hex pubkey, signing callback, an already-uploaded root,
and a UI update callback. The signer may be a wallet; NDK is optional.

```typescript
import { type CID } from '@hashtree/core';
import { createNostrRefResolver } from '@hashtree/nostr';
import { SimplePool, nip19, type Event, type EventTemplate } from 'nostr-tools';

export async function publishAndFollowRoot(
  relays: string[],
  treeName: string,
  pubkey: string,
  signEvent: (template: EventTemplate) => Promise<Event>,
  root: CID,
  onRoot: (root: CID | null) => void,
): Promise<() => void> {
  const pool = new SimplePool({ enableReconnect: true });
  const resolver = createNostrRefResolver({
    nip19,
    getPubkey: () => pubkey,
    subscribe: (filter, onEvent) => {
      const subscription = pool.subscribeMany(relays, { ...filter }, { onevent: onEvent });
      return () => subscription.close();
    },
    publish: async (template) => {
      const signed = await signEvent({
        ...template,
        created_at: template.created_at ?? Math.floor(Date.now() / 1000),
      });
      await Promise.any(pool.publish(relays, signed)); // At least one relay accepted it.
      return true;
    },
  });
  const key = `${nip19.npubEncode(pubkey)}/${treeName}`;
  const unsubscribe = resolver.subscribe(key, onRoot);
  const close = () => { unsubscribe(); resolver.stop?.(); pool.destroy(); };
  try {
    const result = await resolver.publish!(key, root, { visibility: 'public' });
    if (!result.success) throw new Error('Root publication failed');
    return close; // Call when the view/app no longer follows this tree.
  } catch (error) {
    close();
    throw error;
  }
}
```

For a read-only view, create the same resolver and call `subscribe()` with the
publisher's `npub/treeName`; omit the publish call. `getPubkey` may return `null`
and `publish` may be `async () => false` when your adapter cannot write. Keep
subscriptions open after EOSE and during quiet periods; absence of an event is
not a missing tree. Handle fetch failures in async UI callbacks yourself.
`resolve(key)` is a one-shot lookup that can wait indefinitely; use a subscription
with explicit cleanup for cancellable/live views.

Upload **all blocks first**, then publish the root. The resolver updates its local
cache optimistically; an `onRoot` callback is not proof of relay acceptance.
Catch rejected signing/publishing requests and check `result.success`.
Relay acceptance is also not evidence that the root's blobs are available.

## Names and visibility

The resolver key is `npub/treeName`; everything after the first slash belongs to
the tree name. To read a file, first resolve that exact root name and then call
`HashTree.resolvePath(root, 'path/to/file')` with the separate file path.

New roots use kind **30064**, tags `d` (tree name), `l=hashtree`, and `hash`.
Readers also accept legacy kind **30078**. Visibility controls key disclosure:

| Visibility | Key handling |
| --- | --- |
| `public` (default) | Publishes the content key in a `key` tag; anyone can decrypt retrieved blocks |
| `link-visible` | Publishes `encryptedKey` / `keyId`; keep the returned `result.linkKey` in the secret share URL |
| `private` | Requires `visibility.encrypt` / `decrypt` callbacks for NIP-44 self-encryption |

For link-visible readers, supply `visibility.getLinkKey`; supply NIP-44 callbacks
to let the owner recover keys too. Visibility options need an encrypted root
CID. Changing visibility cannot revoke keys or old roots already shared.
For direct signed replaceable-event publishing, `createReplaceablePublishQueue()`
coalesces bursts per coordinate; see its API before building your own queue.

## Store and query events

This standalone example creates a temporary signing identity for local demo data.
Apps should use their existing signer and validate incoming event signatures.

```typescript
import { MemoryStore } from '@hashtree/core';
import { NostrEventStore } from '@hashtree/nostr';
import { generateSecretKey, finalizeEvent } from 'nostr-tools';

const event = finalizeEvent({
  kind: 1, created_at: 1, tags: [['t', 'hashtree']], content: 'Hello from Nostr',
}, generateSecretKey());
const events = new NostrEventStore(new MemoryStore());
const root = await events.add(null, event);
const found = await events.query(root, { authors: event.pubkey, kinds: [1] }, { limit: 50 });
console.log(found[0]?.content); // Hello from Nostr
for await (const note of events.streamQuery(root, { tags: { t: 'hashtree' } })) {
  console.log(note.content); // Hello from Nostr
}
```

Retain the root returned by `add()` / `build()`. `query()` and `streamQuery()`
select the available author/kind/tag/time index. Pass `{ strict: true }` to reject
missing/unreadable selected event blobs instead of skipping them.

## Signed snapshots

`storeTreeEventSnapshot(tree, nip19, signedRootEvent)` stores an immutable signed
root event and returns its `snapshotCid`, `snapshotNhash`, and `rootCid` (or `null`
for an unsuitable event). `readTreeEventSnapshot()` restores it;
`buildTreeEventSnapshotPermalink({ snapshotNhash, path, linkKey })` makes a route.
The snapshot does not copy the root's file blocks. Preserve/upload those too.

Use snapshots for permalinks, history, or offline reuse. For live state, keep a
root subscription open. `watchLatestTreeEventSnapshot()` combines relay discovery
with snapshot storage and returns a cleanup function; the app owns cache policy.
Peer blob fetching uses [FIPS transport](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-fips-transport/README.md).

## License

MIT
