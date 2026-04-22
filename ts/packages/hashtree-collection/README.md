# @hashtree/collection

Immutable content-addressed collections for hashtree.

For app-builder guidance and common pitfalls, see [../../GETTING_STARTED.md](../../GETTING_STARTED.md).

This package adds a small layer on top of `@hashtree/index`:

- canonical `byId` roots
- auto-updated key indexes
- auto-updated search indexes
- optional schema defaults, normalization, and migration hooks
- published source manifests
- federated search across many source manifests

It is meant for decentralized app data such as personal catalogs, followed-user datasets, local merged views, and broader platform-style apps where many publishers own their own records.

## Design

This package is intended for decentralized data, so it does **not** assume one rigid global schema.

The intended model is:

- each publisher owns their own source
- canonical data is source-owned and content-addressed
- indexes are derived projections
- federated search is multi-query over many sources
- local schema rules are allowed, but global schema lockstep is not required

In practice, that means a collection source should be thought of as:

- raw item blobs
- a canonical `byId` root
- derived key/search indexes
- a manifest that advertises those roots

The current package focuses on the index and manifest layer. It does not try to be a full database.

## Platform Apps

This package is intended to be the generic data/index layer for apps that used to default to centralized "platform" backends.

Examples:

- marketplace listings
- room or apartment inventories
- ride availability and dispatch inputs
- booking slots and service catalogs
- jobs, offers, menus, and local reputation projections

The decentralized pattern is:

- each participant publishes their own source
- canonical state stays source-owned
- browse/search/trust are local derived views
- federated query replaces the one global SQL table

## Raw Data vs Projections

For decentralized systems, the safest long-term split is:

- raw item format:
  publisher-defined and potentially app-specific
- projection/index format:
  small normalized fields used for search, browse, ranking, and lightweight display

That split matters because clients may not understand every publisher's raw format, but they can still query published projections and indexes.

This package currently gives you the projection/index side:

- canonical `byId`
- named key indexes
- named search indexes
- source manifests
- federated search helpers

It is compatible with a future codec/projection layer, where a source can declare an item format and clients can optionally decode richer item payloads when they know that format.

## Published Metadata

When a collection root is published as a hashtree directory, reserve
`.collection-manifest.json` for collection-level metadata that peers can inspect
without any local runtime hooks.

Today that metadata is intentionally small:

- `schemaVersion`
- `publishedSchema.itemFormat`
- `publishedSchema.projectionFormat`
- optional `publishedSchema.schemaRef`

The JSON shape is the same in TypeScript and Rust. Index names are expected to
be meaningful enough on their own, so there is no extra per-index description
layer by default.

## Schema

`CollectionDefinition.schema` is intentionally a **local convenience**, not a universal contract.

Use it for:

- filling defaults
- normalization before indexing
- validation for your own writes
- migrating known legacy item shapes

Do **not** assume every remote source on the network shares the same schema or predictable migration chain.

For that reason, schema support in this package is intentionally small:

- `defaults`
- `normalize`
- `validate`
- `migrate`

If a decentralized source uses an unknown raw item format, the source can still participate in federated search as long as it publishes compatible derived indexes.

## Federated Query Model

The intended default is:

- query many source manifests in parallel
- merge results locally
- dedupe by logical id
- optionally boost by trust or social distance

This is usually better than physically merging everyone into one canonical shared mutable index.

Physical merge can still be useful as a local cache or overlay, but correctness should come from source snapshots, not from endlessly accumulating merged roots.

## Install

```bash
npm install @hashtree/collection
```

## Usage

```typescript
import { MemoryStore } from '@hashtree/core';
import { CollectionWriter, CollectionSource, federatedSearch } from '@hashtree/collection';

const store = new MemoryStore();

const songs = new CollectionWriter(store, {
  sourceId: 'npub1.../audio',
  schema: {
    version: 2,
    defaults: { tags: [] },
    normalize: (song) => ({
      ...song,
      title: song.title.trim(),
    }),
  },
  getId: (song) => song.id,
  keyIndexes: [
    { name: 'artist', keys: (song) => [`artist:${song.artist.toLowerCase()}`] },
  ],
  searchIndexes: [
    { name: 'songs', prefix: 's:', text: (song) => [song.title, song.artist] },
  ],
});

await songs.put({ id: 'song-1', title: 'Starlight Echo', artist: 'Ada' }, someCid);

const source = new CollectionSource(store, songs.manifest());
const results = await source.search('songs', 'starlight');
```

## Notes

- `put(item, cid)` is safe for inserts and by-id-only collections.
- `put(...)` requires `options.previous` when replacing an existing item in a collection with key/search indexes, so the library can remove stale derived entries deterministically.
- `replace(item, cid, previous)` is the explicit helper for indexed updates.
- `delete(item)` requires the indexed fields of the item being removed.
- `reindex(entries)` is the explicit way to rebuild all derived roots after adding indexes or changing derivation rules. It needs canonical item snapshots plus their CIDs; roots alone are not enough.
- Schemas are intentionally small: use `defaults`, `normalize`, `validate`, and `migrate` instead of a large schema framework.
- Federated search is multi-query first. You do not need to physically merge roots just to search across many sources.

## Direction

The likely next layer on top of this package is a codec/projection model:

- source declares an `itemFormat`
- clients optionally register adapters/codecs for known formats
- search and browse can still work from published projections even when raw items are unknown

That keeps the network open to many app-specific formats without giving up discoverability.
