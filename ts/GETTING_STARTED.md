# Getting Started For App Builders

This guide is for building decentralized apps on hashtree without sliding back into centralized "platform backend" habits.

Use hashtree as the app data substrate:

- `@hashtree/core` stores immutable blobs and directories.
- `@hashtree/collection` turns app records into source-owned manifests plus derived indexes.
- `@hashtree/nostr` resolves live mutable roots and gives you a raw Nostr event collection format with query helpers.
- `@hashtree/worker` is the portable browser runtime when the app should run both in normal browsers and in Iris shells.

## Mental Model

Think in terms of many source-owned collections, not one global mutable database.

- Each user, provider, host, driver, merchant, or publisher owns one or more roots.
- Canonical writes live in those source roots.
- Search, browse, ranking, trust, and local joins are derived indexes or local overlays.
- Mutable discovery happens through Nostr-published roots.
- Immutable content is still addressed directly by hash.

That same model works for feeds, catalogs, marketplaces, lodging listings, ride supply, job boards, booking inventories, and similar "platform-like" apps.

## Recommended Stack

For a typical app:

1. Use `@hashtree/core` plus a local store (`MemoryStore`, Dexie, Blossom fallback, or worker runtime).
2. Model each publisher-owned dataset as a `CollectionWriter`.
3. Publish the collection root as a mutable Nostr root.
4. Read remote collections through `CollectionSource` or `NostrEventStore`.
5. Merge many sources locally instead of inventing a central query API.

## Live Roots First

For mutable app data, resolve the live root from relays first.

- Use `createNostrRefResolver()` for `npub/tree/path` style roots.
- Use `storeTreeEventSnapshot()` only for immutable permalinks, offline fallback, or signed historical captures.
- Do not bundle a snapshot when the product needs the current live state.

## Keep Query Logic In The Library

If your app needs Nostr-event indexes, use `NostrEventStore`.

```ts
import { MemoryStore } from '@hashtree/core';
import { NostrEventStore } from '@hashtree/nostr';

const store = new MemoryStore();
const events = new NostrEventStore(store);

const profileFeed = await events.query(rootCid, {
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

The point is that apps should not need their own `/api/nostr/query` layer just to scan a collection. Pick the best index in the library, then only do app-specific ranking in app code.

## No Framework Requirement

`@hashtree/nostr` does not require NDK.

- Pass raw subscribe/publish callbacks to `createNostrRefResolver()`.
- Use `nostr-tools`, `window.nostr`, your own relay client, or a native bridge.
- Keep the app dependency surface as small as the app actually needs.

## Collection Pattern For Platform Apps

`@hashtree/collection` is the generic app record/index layer.

Good fits:

- marketplace listings
- apartment or room inventories
- ride availability / driver state
- booking offers and calendar slices
- menus, products, or service catalogs
- local trust or reputation projections

Recommended split:

- Raw record: publisher-defined JSON/blob/event.
- Collection `byId`: canonical owned records.
- Key indexes: exact lookups and structured browse paths.
- Search indexes: lightweight text discovery.
- Local overlay: ranking, trust, dedupe, policy, and temporary UX state.

## Pitfalls To Avoid

- Do not invent a central app API when the app can read hashtree roots and indexes directly.
- Do not scan a global feed if a per-author or per-source index exists.
- Do not assume one universal schema across the network.
- Do not force all apps onto NDK or another large SDK if simple relay callbacks are enough.
- Do not merge everyone into one canonical shared mutable root unless that is explicitly the product model.

## Minimal Publishing Flow

For an app-owned source:

1. Write records into a `CollectionWriter`.
2. Publish the resulting root through your mutable Nostr root event.
3. Let other apps resolve that root and query the published indexes directly.

This keeps authorship, trust, portability, and caching aligned with the storage model instead of rebuilding a centralized backend behind the scenes.
