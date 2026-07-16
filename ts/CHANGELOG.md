# Changelog

## Unreleased

### Fixed

- Corrected Nostr replaceable/addressable storage, snapshots, and worker root
  tracking to use NIP-01's lexically lowest event ID when timestamps tie;
  ordinary chronological event ordering is unchanged.

## TypeScript runtime 0.4.4 - 2026-07-16

### Fixed

- Required the shared worker's main and relay entries to resolve exact P2P
  provider identities before creating blob routes. An enabled bridge with no
  listed provider remains Blossom-only instead of issuing an anonymous fetch;
  configured routes preserve native HTL 10 and central hash verification.

### Changed

- Consolidated peer-list caching and route construction into one shared helper,
  removing the duplicate and less strict worker implementations.

## TypeScript runtime 0.4.3 - 2026-07-16

### Fixed

- Enabled `@hashtree/worker@0.3.2` media reads to use the shared P2P bridge
  when an application configures one. Workers without that bridge remain
  Blossom-only; no connected-peer inference or product fallback was added.
- Persisted peer-share authorization for explicitly uploaded encrypted trees
  and raw blocks across worker restarts. Ordinary cached and remotely fetched
  blobs remain session-only and fail closed after restart; stale or malformed
  authorization metadata is pruned.
- Replayed the newest configured P2P provider state after both worker clients
  report ready, preventing startup or replacement races from dropping an
  enable or disable transition.

### Changed

- Updated `@hashtree/fips-transport@0.4.3` development and peer requirements
  to the immutable FIPS TypeScript runtime 0.0.25 (`@fips/core@0.0.25` and
  `@fips/transport-webrtc@0.0.41`). Blob wire and HTL semantics are unchanged.
- Updated worker integration tests to the immutable Iris Kit runtime 0.2.2
  (`ndk@0.2.1` and `ndk-cache@0.2.1`).

## TypeScript runtime 0.4.2 - 2026-07-16

### Fixed

- Included the TypeScript sources referenced by every emitted JavaScript and
  declaration source map. Consumer builds no longer warn about missing package
  sources.

### Changed

- Released one immutable bundle containing `@hashtree/core@0.2.1`,
  `@hashtree/index@0.1.11`, `@hashtree/collection@0.2.7`,
  `@hashtree/dexie@0.1.7`, `@hashtree/git@0.1.6`,
  `@hashtree/merge@0.1.2`, `@hashtree/mesh@0.1.5`,
  `@hashtree/nostr@0.1.17`, `@hashtree/worker@0.3.1`, and
  `@hashtree/fips-transport@0.4.2`.
- Removed the duplicate in-repository NDK and NDK cache trees. Worker tests now
  consume the immutable Iris Kit runtime artifacts, leaving one maintained NDK
  implementation instead of two 52,500-line copies.

## TypeScript runtime 0.4.0 - 2026-07-16

### Added

- Added the shared `BlobRequest` / `BlobReply` / `BlobRoute` contract to
  `@hashtree/core@0.2.0`, with the native-compatible HTL range and default of
  `0..10` / `10`.
- Added bounded, cancellable worker blob routes that preserve route-local
  misses, surface provider failures, and accept the first centrally
  hash-verified data reply.
- Added explicit FIPS provider routes and exact authenticated
  `hashtree.blob/1` + port `39018` capability filtering. Connected FIPS peers
  are no longer inferred to be Hashtree providers.

### Changed

- Released one immutable TypeScript bundle containing `@hashtree/core@0.2.0`,
  `@hashtree/index@0.1.10`, `@hashtree/collection@0.2.6`,
  `@hashtree/dexie@0.1.6`, `@hashtree/git@0.1.5`,
  `@hashtree/mesh@0.1.4`, `@hashtree/nostr@0.1.16`,
  `@hashtree/worker@0.3.0`, and `@hashtree/fips-transport@0.4.0`.
- Prepared the breaking `@hashtree/fips-transport@0.4.0` release around the
  reliable TCP/FIPS blob protocol. Removed the obsolete raw endpoint-datagram
  transport/store API and its `@hashtree/mesh` dependency while preserving the
  browser and worker providers on `TcpBlobTransport`. Worker request HTL is
  carried unchanged through the terminal adapter exactly once.
- Closed worker providers now reject reads instead of reporting a false content
  miss.
- Consolidated the two worker P2P bridges into one implementation and removed
  the parallel framing, timeout, and pending-request logic.
- Removed the legacy direct Hashtree WebRTC worker mesh, its kind-25050
  signaling/proxy API, and the `@hashtree/worker/p2p` export. Browser mesh
  traffic now enters both worker clients through the FIPS provider interface,
  with WebRTC owned by `@fips/transport-webrtc`.
- Removed the stale in-workspace `nostr-social-graph` source copy. The worker's
  optional integration now resolves the independently published package.

## 0.2.13 - 2026-06-22

Changes since the previous npm package publish.

### Changed

- Published `@hashtree/core@0.1.8`, `@hashtree/nostr@0.1.15`, and
  `@hashtree/worker@0.2.17`.
- Changed TypeScript hashtree root publishing to use Nostr kind `30064`, while
  readers and subscriptions query both `30064` and legacy `30078` roots.
- Upstreamed the browser Blossom batch-read/timeout knobs and worker media root
  retry behavior so app repos no longer need version-specific patches for
  those fixes.

## 0.2.12 - 2026-05-20

Changes since the previous npm package publish.

### Changed

- Prepared `@hashtree/index@0.1.9` and `@hashtree/collection@0.2.5` for npm
  publication.
- Added tracked build output for the package versions used by Iris Audio so
  they can be consumed directly from pinned Git subdirectory dependencies when
  npm publication is blocked.
- Added `BTree.countStoredLinks(...)` for no-scan stored link counts and
  `BTree.scanLinks(...)` for explicit full scans. `BTree.countLinks(...)`
  remains the backwards-compatible scanning count.
- Changed `@hashtree/collection` to depend on the counted B-tree writer so
  rebuilt collection indexes store subtree sizes for ordinal reads and random
  sampling.

## 0.2.11 - 2026-05-19

Changes since the previous npm package publish.

### Added

- Published `@hashtree/core@0.1.7`, `@hashtree/nostr@0.1.14`, and
  `@hashtree/worker@0.2.16`.
- Added `putBlock(...)` and `putBlocks(...)` to `@hashtree/worker/client` so
  callers can store raw content-addressed blocks, optionally upload them to
  Blossom, and share published raw blocks with peers.
- Added `createReplaceablePublishQueue()` to `@hashtree/nostr` so consumer apps
  can coalesce same-coordinate replaceable publishes, sign at send time, and
  avoid app-side future `created_at` drift.
- Added optional `watch(hash, callback)` to the `Store` interface so the tree
  layer can react to block arrivals without polling. `MemoryStore` implements
  it; existing stores remain compatible because the method is optional.
- Added `loadBlock(store, hash, signal?)` (re-exported from `@hashtree/core`).
  Resolves once data for `hash` is available, using `watch` when present and
  falling back to polling otherwise.

### Changed

- Changed `@hashtree/index` CID-link B-trees to store subtree link counts in
  internal directory entry sizes during `buildLinks(...)` and `insertLink(...)`,
  so `countLinks(...)`, `getLinkEntryAt(...)`, and `sampleLinks(...)` can avoid
  recursively counting every child on counted trees while still falling back for
  legacy roots.
- Changed `@hashtree/core` Blossom uploads to abort stalled `PUT /upload`
  requests instead of waiting indefinitely.
- Changed `listDirectory`, `resolvePath`, and `HashTree.listDirectory` /
  `HashTree.resolvePath` to wait for the directory block to load instead of
  returning `[]` / `null` when the data isn't local yet. They now accept an
  optional `signal: AbortSignal` for callers that want a bounded wait. `[]` from
  `listDirectory` and `null` from `resolvePath` again mean "the directory loaded
  and the entry isn't there", which lets callers drop ad-hoc grace timers around
  `'not found'` UI.

## 0.2.10 - 2026-05-06

Changes since the previous npm package publish.

### Added

- Published `@hashtree/mesh@0.1.3`, `@hashtree/nostr@0.1.13`, and `@hashtree/worker@0.2.15`.
- Added TypeScript wire support for the Rust-compatible pubsub messages `PubsubInterest`, `PubsubFrame`, `PubsubInventory`, and `PubsubWant`, including constants, creators, encoders, parsers, and Nostr re-exports.

### Changed

- Changed the worker WebRTC send queue to prioritize pubsub inventory and want frames alongside blob requests so small pull-control messages are not stuck behind bulk payload responses.

## 0.2.9 - 2026-04-24

Changes since the previous npm package publish.

### Changed

- Published `@hashtree/worker@0.2.14`.
- Changed worker WebRTC signaling to include authenticated NIP-59 seals inside the encrypted hashtree relay envelope, so receivers derive directed signaling identity from the verified seal signer instead of an unsigned payload field.

### Added

- Added TypeScript/Rust signaling interop coverage for authenticated directed WebRTC offers in both directions.

## 0.2.8 - 2026-04-24

Changes since the previous npm package publish.

### Changed

- Published `@hashtree/collection@0.2.4`.
- Changed `CollectionWriter.reindex(...)` / `rebuild(...)` to accept async entry streams, so large consumers can rebuild derived roots directly from async storage without first materializing every canonical item in memory.

## 0.2.7 - 2026-04-23

Changes since the previous npm package publish.

### Changed

- Published `@hashtree/collection@0.2.3`.
- Changed `@hashtree/collection` search index definitions so `searchIndexes[].terms(...)` now drives writer updates, deletes, and rebuilds instead of only query-side helpers.
- Added optional query-side term reuse to `CollectionSource` by allowing readers with the original definition to pass `new CollectionSource(store, manifest, definition)` and keep `search(...)` aligned with the same custom query expansion.
- Narrowed the new `CollectionSource` query-definition argument to the query-time search shape it actually consumes, so full collection definitions remain assignable in TypeScript consumers.

## 0.2.6 - 2026-04-22

Changes since the previous npm package publish.

### Changed

- Published `@hashtree/collection@0.2.1`.
- Changed `CollectionSource.count()` to use the manifest's published `itemCount` by default and added `exactCount()` for callers that need a full `by-id` walk. This removes unnecessary whole-tree scans in large collection consumers and snapshot validators.

## 0.2.5 - 2026-04-22

Changes since the previous npm package publish.

### Added

- Added `@hashtree/index@0.1.8` with pre-tokenized `searchTerms(...)` and `searchLinksTerms(...)` helpers plus explicit `scanLimit` control, so consumers can share ranking logic while still expanding their own query terms.
- Added explicit `replace(...)` helpers to `@hashtree/collection` in both TypeScript and Rust so indexed record replacement is a first-class operation instead of an options convention.

### Changed

- Published `@hashtree/collection@0.2.0` and changed indexed overwrite semantics to fail fast when callers try to replace an existing item without supplying the previous indexed snapshot. This removes a silent stale-index footgun and pushes full rebuilds/reindexing into the explicit path for cases where the old item is unavailable.

## 0.2.4 - 2026-04-17

Changes since the previous npm package publish.

### Fixed

- Stabilized `@hashtree/worker` media reads over WebRTC by retrying transient path misses and missing chunks, honoring startup range requests more robustly, and allowing apps to pass file-size hints for media streaming.
- Fixed peer blob serving in `@hashtree/worker` so encrypted blobs are only shared with peers after they are confirmed reachable from a shared read source instead of leaking local-only cache state.
- Fixed `@hashtree/worker/client` and the underlying mesh store to clone transferred byte buffers before handing them across worker boundaries, avoiding detached-buffer corruption during media fetches.

### Improved

- Improved `@hashtree/worker` Blossom and primary-store read behavior with bounded local/read timeouts so mesh fallback can recover sooner on stalled reads.

## 0.2.2 - 2026-04-11

Changes since the previous npm package publish.

### Fixed

- Repacked `@hashtree/worker` with both `dist/` and `src/` included in the npm tarball so published consumers resolve the built entrypoints and source maps correctly.

## 0.2.1 - 2026-04-11

Changes since the previous npm package publish.

### Fixed

- Included `src/` in the published `@hashtree/worker` tarball so the distributed source maps resolve correctly in consumer builds and dev servers.
- Normalized the npm package `repository` metadata to the object form expected by the registry tooling.

## 0.2.0 - 2026-04-11

Changes since the previous npm package publish.

### Changed

- Renamed the published relay-backed worker surface from `@hashtree/worker/iris-client` and `@hashtree/worker/iris-entry` to `@hashtree/worker/relay-client` and `@hashtree/worker/relay-entry`, and renamed the exported client/types to `RelayWorkerClient`, `RelayWorkerConfig`, `RelayWorkerRequest`, and `RelayWorkerResponse`.
- Renamed the package-internal `src/iris/*` runtime tree to neutral `src/relay/*` modules so the reusable worker package no longer carries Iris-branded source paths or symbols.
- Switched runtime launch parameter lookup from `iris_htree_server` / `iris_htree_canonical` to generic `htree_server` / `htree_canonical`, while continuing to support host-provided `window.__HTREE_SERVER_URL__` and `window.__HTREE_CANONICAL_URL__`.

### Improved

- Removed the package-owned Iris Blossom defaults so consumers must provide their own upstream Blossom server policy instead of inheriting app-specific service URLs implicitly.
- Removed `wss://temp.iris.to` from the built-in relay fallback lists used for root resolution and tree-root history lookups.

## 0.1.25 - 2026-04-11

Changes since the previous npm package publish.

### Added

- Added `@hashtree/worker/iris-client` with a reusable `IrisWorkerClient` wrapper for `iris-entry` workers, including tree-root metadata lookups, tree-root subscription helpers, and media-port registration.

### Improved

- Made it possible for Iris runtime hosts such as `iris-sites` to consume the published Iris worker client instead of carrying a repo-local duplicate of the message plumbing.

## 0.1.24 - 2026-04-11

Changes since the previous npm package publish.

### Added

- Added `@hashtree/worker/worker` with `attachHashtreeWorker(...)` so apps can embed the hashtree worker protocol inside a custom worker or a dedicated `MessagePort` instead of being forced to use the package-owned worker entrypoint.

### Changed

- Changed the default `@hashtree/worker/entry` bootstrap to use the same attachable worker adapter as custom embedded workers.

## 0.1.23 - 2026-04-11

Changes since the previous npm package publish.

### Added

- Added bulk `build(...)` and `buildLinks(...)` helpers to `@hashtree/index` so callers can construct B-tree roots from unsorted key/value and key/CID iterables in one pass.

### Fixed

- Fixed `@hashtree/worker` root watches to start immediately instead of blocking client startup on the first relay resolution, and emit the first resolved root through the normal update channel when it arrives.
- Fixed the published `@hashtree/worker` package manifest to depend on released `@hashtree/*` versions instead of leaking `workspace:*` ranges to npm consumers.

### Improved

- Improved `@hashtree/worker/client` root-watch behavior so delayed initial roots no longer produce a synthetic `null` callback before the first real update.

## 0.1.22 - 2026-04-10

Changes since the previous npm package publish.

### Added

- `@hashtree/collection@0.1.1`
- `@hashtree/nostr@0.1.7`

- Added `CollectionSource.count()` and `CollectionSource.queryById(...)` so callers can enumerate `by-id` entries and perform prefix-limited ID queries without reading internal index objects directly.
- Added shared `.collection-manifest.json` helpers in `@hashtree/collection` so collection roots can publish and reload `schemaVersion`, item/projection formats, and optional schema references in a cross-runtime format.
- Added `@hashtree/nostr` event-root publication of the shared collection manifest metadata plus TS/Rust interop coverage that compares the actual published metadata payloads.

## 0.1.20 - 2026-04-10

Changes since the previous npm package publish.

### Added

- Added `createHtreeRuntime(...)` plus runtime URL helpers so Iris-compatible apps can consistently resolve the active htree, relay, and Blossom endpoints and generate `/htree/...` request URLs with per-client scoping.
- Added worker diagnostics events and client listeners so apps can observe runtime/media issues without scraping console output.

### Improved

- Improved `@hashtree/worker` Blossom reads by deduplicating concurrent fetches for the same hash and limiting cross-hash read concurrency.
- Updated the package documentation to show the intended worker bootstrap and Iris-compatible runtime pattern.

### Changed

- Removed `wss://offchain.pub` from the default relay fallback lists used by root resolution and Iris tree-root subscriptions.

## 0.1.11 - 2026-04-01

Changes since the previous npm package publish.

### Improved

- Added the `@hashtree/worker/client` export and moved `ndk`, `ndk-cache`, and `nostr-social-graph` to optional peer dependencies so apps can provide those integrations explicitly.

### Fixed

- Fixed Blossom reads in `@hashtree/worker` to fetch the hashed `.bin` payload directly, verify it, and treat the cache write as a trusted backfill instead of blocking the response path.
- Fixed media file streaming in `@hashtree/worker` to resolve requested subpaths before streaming and to clone transferred chunk buffers so browsers do not trip over detached `ArrayBuffer` state.
