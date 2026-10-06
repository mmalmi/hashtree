# hashtree-index

Content-addressed B-tree indexes for hashtree.

## Indexes independent of servers

An index is a tree of hash-verified blocks, identified by its contents rather
than a server URL. The same index can be fetched from any peer or mirror that
has its blocks. Keep a snapshot, share it, build your own index from it, or query
several independently maintained indexes. No central registry controls who can
create or serve one. Updating an index creates a new root; a signed Nostr named
root can announce the latest snapshot while keeping its publisher's identity.

For Nostr data, the original signed events remain the source of truth. Relays
can deliver live events; Hashtree can also store and serve event indexes and
derived text-search indexes. Queries can use either or both. Index publishers
choose what to include, while clients verify the original authors' signatures
and apply their own ranking or social graph. Copying an index does not change
those authors, and an index need not contain every event on the network.

The [Nostr event store](../hashtree-nostr/src/lib.rs) provides event/filter indexes;
the [nostr-pubsub adapter](../hashtree-nostr-pubsub/src/lib.rs) exposes them through
the same query interface as relay-backed event sources. The
[TypeScript adapter](../../../ts/packages/hashtree-nostr-pubsub/README.md) supports
multiple Hashtree event sources, including shards and mirrors.

## Building indexes

The crate builds deterministic index trees on top of `hashtree-core`, so index
roots can be stored, replicated, and compared the same way as file trees.

Current uses include:

- key/value lookup over immutable tree roots
- link indexes that point directly at content-addressed blobs
- derived search indexes with tokenized prefix lookup and ranked results
- cross-language index interoperability with the TypeScript implementation

Part of [hashtree](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree).
