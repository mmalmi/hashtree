# `@hashtree/nostr-pubsub`

One Hashtree-backed `NostrEventReader` for a `nostr-pubsub` router. A reader can
query several immutable Nostr event-index roots without registering one router
adapter per root.

The class structurally implements the `nostr-pubsub` 0.4 reader contract without
a package dependency in either direction. Both packages share `nostr-tools`
event/filter shapes, so the reader can be passed directly to the router while
keeping Hashtree out of the router core's dependency graph. `nostr-tools` is a
peer dependency so both packages use the same verified-event brand.

```ts
import { MemoryStore } from '@hashtree/core';
import { HashtreeNostrEventReader } from '@hashtree/nostr-pubsub';

const reader = new HashtreeNostrEventReader({
  store: new MemoryStore(),
  roots: [
    { partitionId: 'shared', replicaId: 'primary', root: sharedRoot },
    { partitionId: 'shared', replicaId: 'fallback', root: sharedReplicaRoot },
    { partitionId: 'personal', root: personalRoot },
  ],
});
```

Distinct `partitionId` values are additive and queried concurrently. Entries
with the same partition ID are replicas tried in order until one proves a
complete result, including a valid empty result. Results are signature-checked,
merged by event ID, deterministically ordered newest first, and globally
limited. Additive reads use at most eight concurrent partitions by default
(`maxConcurrentPartitions`, clamped to 1...64). When a global limit is present,
the reader incrementally retains only the merged top-k events instead of every
partition result.

A single `CID` or `null` is accepted as the `roots` value. For a very large or
catalog-backed index, implement `HashtreeNostrRootProvider`; its `snapshot`
method runs exactly once per query and receives the filters so it can select a
small relevant root set. This package intentionally does not define a catalog
schema.

NIP-50 `search` filters are rejected explicitly. Search should use a dedicated
Hashtree search index rather than pretending the event index implements NIP-50.
This reader also requires full lowercase 64-character values for `ids`,
`authors`, `#e`, and `#p`; NIP-01 hexadecimal prefix filters are rejected rather
than widened into an expensive full-index scan.
An incomplete partition is reported separately from a proven empty partition.
Abort signals and absolute deadlines bound root resolution and block reads.
