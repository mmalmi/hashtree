# Changelog

## Unreleased

## 0.2.131 - 2026-07-21

### Changed

- Closed-store Nostr repair now detaches every derived projection while
  retaining the authoritative author/kind/time index, then rebuilds all eight
  derived indexes in bounded pages. This recovers from damage in more than one
  projection, reclaims broken generations before rebuilding, and emits a
  measured checkpoint after every page.

## 0.2.130 - 2026-07-21

### Added

- `nostr-index prepare-time-repair` atomically publishes a closed-store repair
  manifest that preserves every authoritative index and event link while
  detaching a damaged derived chronological index. Retained-root cleanup can
  then reclaim the broken generation before rebuilding it without requiring
  extra filesystem headroom.

## 0.2.129 - 2026-07-21

### Fixed

- Copy-on-write B-tree updates no longer report an original node as
  superseded when a split retains that content-addressed node as one of its
  replacement chunks. Offline chronological-index repair can now reclaim
  intermediate generations without deleting nodes still reachable from the
  repaired root.

## 0.2.120 - 2026-07-20

### Changed

- Two-phase Nostr fetch backpressure is now based on the 8 GiB staged-live-byte
  budget instead of an arbitrary 256-author lag. Authors with empty or small
  segments no longer make relay fetching wait for slow global-index projection.

## 0.2.119 - 2026-07-20

### Changed

- Two-phase Nostr crawls accept `--staging-data-dir`, allowing fetch state and
  individual event blobs to live in a separate Hashtree store. Relay ingestion
  and index projection no longer contend for the same LMDB writer lock.
- Fetching applies durable backpressure at 256 projected authors or 8 GiB of
  selected live data ahead, bounding staging-disk growth without re-coupling
  either LMDB writer.

## 0.2.118 - 2026-07-20

### Changed

- Social-graph archive crawls can run as two independent durable phases:
  `--stage-only` fetches each author and stores every selected event as an
  individual content-addressed Hashtree blob, while `--project-staged` consumes
  those blobs without contacting relays and advances the published index
  watermark separately.
- Staging segments and fetch state are atomically persisted and fsynced before
  their cursor advances. Projection batches are bounded by both author and
  event counts, can follow a live staging pass, and retain the existing
  resumable index cursor as their publication watermark.
- Release `hashtree-nostr` 0.2.87 with explicit fetch-only and independent
  event-blob storage APIs used by the two-phase CLI.

## 0.2.117 - 2026-07-20

### Changed

- Durable Nostr author checkpoints can use LMDB `NO_SYNC`/`NO_META_SYNC` for
  bounded internal event-index commits, then force-sync the graph, ambient,
  metadata, pool catalog, and pool members once before advancing the author
  cursor. This removes hundreds of redundant disk barriers from large authors
  without weakening checkpoint durability.
- Nostr checkpoint logs now report `checkpoint_sync_ms` separately from relay
  fetching and index construction.

## 0.2.116 - 2026-07-20

### Changed

- Resumed Nostr event ingestion now flushes event blobs and each independent
  B-tree projection separately. This bounds the buffered node set without
  forcing tiny event commits that repeatedly walk all nine indexes.
- Sparse updates of legacy B-tree roots preserve unknown subtree counts instead
  of recursively scanning untouched descendants during ingestion. Exact counts
  remain available through explicit scans, while newly built and fully counted
  subtrees continue to expose constant-time stored counts.
- Release `hashtree-core` 0.2.88, `hashtree-index` 0.2.84,
  `hashtree-collection` 0.2.84, and `hashtree-nostr` 0.2.86 with the bounded
  projection-flush path used by the CLI.

## 0.2.115 - 2026-07-20

### Changed

- Bounded Nostr event-store builds now return freed per-index change-map arenas
  to glibc after every configured index commit. Large single-author crawls no
  longer need to reach the outer author checkpoint before reclaiming memory.

## 0.2.114 - 2026-07-20

### Changed

- Durable social-graph Nostr checkpoints now return freed B-tree working
  arenas to glibc before processing the next author batch. This prevents
  bounded multi-checkpoint crawls from accumulating inactive heap until their
  cgroup memory limit is exhausted.
- Native FIPS WebRTC endpoints now bootstrap over the same authenticated
  WebSocket seeds as browser consumers. `server.fips_websocket_seed_urls` can
  override the defaults, and an explicit empty list disables seed bootstrap.
- Release `hashtree-fips-transport` 0.4.11 with FIPS core 0.4.20 so configured
  WebSocket adjacencies remain admitted under configured-only discovery.
- Release `hashtree-core` 0.2.87 with the optimistic batch-store API already
  used by the CLI, keeping registry builds self-contained.
- Release `hashtree-index` 0.2.83 and `hashtree-lmdb` 0.2.86 with their batch
  APIs already consumed by the CLI, completing registry package verification.
- Release `hashtree-nostr` 0.2.84 with the bounded event-store options and
  crawl timing fields already consumed by the CLI.
- Release `hashtree-collection` 0.2.83 with the known-absent batch writer used
  by the Nostr event store.

## 0.2.113 - 2026-07-20

### Changed

- Bounded social-graph Nostr checkpoints retain only kind-0 metadata in their
  in-memory report projection while still committing every selected event to
  Hashtree. This removes a redundant full-content copy that could exhaust a
  1 GiB crawler cgroup during large author batches.

## 0.2.112 - 2026-07-20

### Changed

- Nostr crawl batches reuse their existing by-id absence check instead of
  walking the same large B-tree a second time during collection updates.
- `htree socialgraph index` now uses bounded 32,768-event index commits and
  256-way B-tree nodes by default. Both values are configurable with
  `--index-commit-batch-size` and `--btree-order` for storage-specific tuning.

## 0.2.111 - 2026-07-19

### Added

- `htree socialgraph index --per-author-kind-event-limit` independently bounds
  retained events for every requested author/kind pair. Compact identity crawls
  can now keep exactly the newest profile, follow list, and mute list without
  one replaceable kind displacing another.

## 0.2.110 - 2026-07-19

### Changed

- Each bounded profile checkpoint now buffers mirrored kind-0 events and both
  profile index updates into one `put_many` flush. This replaces thousands of
  tiny LMDB write transactions with one checkpoint-sized commit while keeping
  the overlay bounded by the caller's author checkpoint cadence.

## 0.2.109 - 2026-07-19

### Added

- `htree socialgraph publish-profile-indexes` publishes the existing
  `profile-search` and `profiles-by-pubkey` roots without rebuilding either
  Hashtree index, allowing bounded crawlers to expose durable checkpoints
  progressively without loading the whole profile catalog into memory.

## 0.2.108 - 2026-07-19

### Changed

- Profile metadata checkpoints now update the profile-by-pubkey tree and all
  profile-search terms in two subtree-reusing Hashtree batches instead of one
  tree rewrite per profile and term. This removes profile-crawl write
  amplification while keeping the checkpoint transaction and memory bounded.
- String-valued B-tree indexes now support deterministic batched insertions and
  deletions with stored subtree counts, matching the existing CID-link batch
  update path.

## 0.2.107 - 2026-07-19

### Changed

- Full-history Nostr crawls now prefer Negentropy per relay while allowing a
  healthy source to advance the durable author cursor when another relay is
  unavailable or lacks reconciliation support. Optional relay work has a
  bounded deadline, while relays without Negentropy retain the paged REQ
  fallback.
- Large Nostr event updates now apply and flush every 8,192 events instead of
  retaining a whole author checkpoint in one in-memory store overlay. Derived
  collection projections materialize one change map at a time, bounding peak
  memory and LMDB transaction size without changing the final index schema.

## 0.2.106 - 2026-07-19

### Changed

- Resumable Nostr crawls now treat author batch size and checkpoint cadence as
  operational tuning rather than content identity. Operators can increase the
  batch after a durable checkpoint to amortize cumulative tree-update costs;
  relay coverage, author ordering, selection limits, and every other content
  policy field remain exact resume invariants.

## 0.2.105 - 2026-07-19

### Fixed

- Resumed PoolStore-backed index builds now batch locally generated
  content-addressed candidates without rereading payloads for hashes already
  present in the committed location catalog. Ordinary writes retain full
  payload verification and repair, while interrupted crawl retries avoid
  multi-gigabyte read amplification before committing their next root.

## 0.2.104 - 2026-07-19

### Fixed

- Full-history Nostr crawls now tolerate relay records that are advertised by
  negentropy but omitted identically by both direct retrieval and a complete
  paging pass. Incomplete paging or disagreeing omission sets still fail the
  all-relay checkpoint, so transient relay gaps cannot silently advance the
  durable author cursor.

## 0.2.103 - 2026-07-19

### Fixed

- Resumed Nostr event-index builds now resolve existing events in batched tree
  walks and apply each collection projection with one subtree-preserving bulk
  update. This removes the per-event B-tree mutation amplification that could
  stall large crawl checkpoints while preserving replacement, duplicate,
  missing-blob, and obsolete-blob semantics.

## 0.2.102 - 2026-07-18

### Fixed

- Strict all-relay Nostr crawls now wait within the existing fetch timeout for
  startup and reconnecting WebSocket handshakes before rejecting a relay. This
  removes the fixed 250 ms startup race without weakening checkpoint coverage.

## 0.2.101 - 2026-07-18

### Added

- Added bounded, resumable full-history Nostr indexing for explicit author
  allowlists. One ordinary event-index root now advances behind an atomic local
  cursor, exact crawl-policy validation, an exclusive writer lock, required
  all-relay coverage, incremental profile indexing, and per-checkpoint
  fetch-versus-build timings without a whole-index completion scan.

### Fixed

- Prepared `hashtree-nostr-pubsub` 0.2.83, `hashtree-cli` 0.2.101, and
  `hashtree-embedded` 0.2.87 with the
  transport-neutral Nostr router: bounded Hashtree index queries are merged
  with the explicitly selected FIPS or traditional-relay backend, while
  publish and live routes remain explicit and globally deduplicate event IDs.
- Prepared `hashtree-fips-transport` 0.4.9 for FIPS 0.4.11: configured
  Nostr relays remain ordinary discovery/signaling relays, while the removed
  `nostr_relay` packet transport and kind-21060 adapter are no longer exposed
  or accepted as peer addresses. Native WebSocket listeners and URL-only
  first-adjacency seeds are available through the endpoint options.
- Released `hashtree-fips-transport` 0.4.8 with adaptive TCP blob polling:
  active transfers retain the 10 ms service cadence while transports without
  application work back off to 250 ms, eliminating needless mobile idle CPU.
- Released `hashtree-fips-transport` 0.4.7 with bounded 100 ms provider
  hedging, so a failed or slow first TCP/FIPS provider cannot starve a healthy
  peer during a multi-provider blob read.

## 0.2.100 - 2026-07-17

### Added

- Released `hashtree-network` 0.2.87 with `RoutedStore`, the canonical
  Store-shaped facade for centrally verified routed reads and mutations owned
  only by the application's explicit primary Store. It replaces the deleted
  transport-owned same-host adapter without introducing a write router.
- Added a bounded Hashtree mesh-forwarding owner that consumes exactly one HTL
  before handing a request to the FIPS composite route, coalesces equal
  in-flight work, and suppresses lower-budget cycle re-entry. FIPS carriers and
  terminal routes continue to preserve HTL unchanged.

### Changed

- Released `hashtree-cli` 0.2.100 and `hashtree-embedded` 0.2.86 with the same
  three-node `2 -> 1 -> 0` forwarding and exhaustion behavior. The FIPS
  composite remains the sole owner of its peer set and preserves its bounded
  discovery-rank, explicit-peer interleave, and first-valid race policy.
- Documented that production paid blob retrieval never existed: the deleted
  quote/chunk path was called only by the simulator. Cashu wallet operations
  remain available, but paid blob retrieval remains unimplemented; obsolete
  paid-blob policy fields are now ignored as legacy TOML rather than implying a
  configured incentive route.

## 0.2.99 - 2026-07-17

### Changed

- Published CLI artifacts now compile the FIPS WebRTC underlay on all five
  targets while continuing to omit optional FUSE integration. Every builder
  extracts its finished archive and starts that exact `htree` binary, requiring
  the WebRTC transport to initialize before the artifact can be released.
- Released `hashtree-embedded` 0.2.85 with the same FIPS WebRTC feature so
  application-owned embedded daemons and the standalone CLI expose one
  consistent native transport boundary.

### Fixed

- Replaced cached Blossom `HEAD` misses immediately after a hash-verified batch
  commit. A concurrent Git push can no longer mistake a freshly stored old-tree
  blob for missing and abandon its bounded diff upload.

## 0.2.98 - 2026-07-17

### Changed

- Released `hashtree-network` 0.2.86 as the read-only adaptive `BlobRouter`
  crate. Route implementations own transport addresses and composite peer
  selection; the outer router retains bounded, decaying route outcomes and
  centrally verifies every Data reply.
- Released `hashtree-config` 0.2.83, `hashtree-embedded` 0.2.84, and
  `hashtree-cli` 0.2.98 without the retired direct-WebRTC/STUN/multicast peer
  settings or status surfaces. FIPS WebRTC remains available strictly as a
  FIPS-owned underlay.

### Removed

- Deleted the duplicate `DataRequest`/`DataResponse` MessagePack framing,
  response fragmentation, raw data-channel carrier, private mesh pubsub,
  WebRTC compatibility stub, dormant paid-peer wrapper, and their simulations.
  Nostr events remain owned by Nostr providers; blob reads retain the published
  `BlobRequest`/`BlobReply` TCP/FIPS wire format and HTL semantics.

## 0.2.97 - 2026-07-17

### Added

- Released `hashtree-lmdb` 0.2.85 with an automatic, multi-terabyte-safe
  `PoolStore` temperature balancer. Reads feed sampled bounded CLOCK queues;
  decaying heat is flushed in batches, and a persisted incremental cursor keeps
  startup and cycle work independent of total pool size.
- Added adaptive hot promotion and cold capacity demotion without media labels.
  Per-member high/low watermarks preserve fast-member headroom, while minimum
  residence and measured performance hysteresis prevent thrashing.
- Added configurable cycle, byte, concurrency, sample, scan, heat, foreground
  load, lease, residence, and stream-chunk bounds. `htree storage pool` now
  exposes member watermarks and an explicit bounded `balance-temperature`
  command; the application still owns one Store and one write destination.

### Fixed

- Made every relocation persist and resume `Moving(source, target)` state,
  stream source bytes in bounded chunks, verify size and SHA-256 before the
  atomic location switch, and delete the source only afterward. Interrupted
  target writes are verified and reused after restart.
- Kept multiprocess balancers under one expiring catalog lease while allowing
  every process to batch its own samples. Long moves heartbeat ownership,
  process death still expires it, and corrupt interrupted target copies are
  discarded and rebuilt from the authoritative source. Foreground load
  throttles new move batches, and correctness remains independent of all heat
  and performance observations.

## 0.2.96 - 2026-07-17

### Added

- Added a strict-auth daemon endpoint that validates, indexes, and pins a
  complete encrypted or public DAG atomically, with storage-sized byte bounds,
  a hard traversal limit, and rollback on missing or invalid descendants.
- Added an opt-in, bounded `fips_open_discovery_max_pending` daemon setting;
  the default remains closed and configured-peer-only.

### Fixed

- Prevented a daemon whose client-facing Blossom endpoint points at its own
  listener from recursively querying itself on a local miss; the daemon now
  keeps that endpoint for explicit client writes while excluding it only from
  its upstream read alternatives.
- Made background mirror DAG uploads owned, cancellable tasks so daemon reload
  and shutdown cannot wait indefinitely for unreachable Blossom servers.
- Released `hashtree-nostr` 0.2.83 so optional negentropy timeouts fall back
  to the configured bounded relay query or paging path. Strict negentropy and
  failures after a relay advertises exact missing IDs still remain errors.
- Released `hashtree-network` 0.2.85 with consistent mesh-event HTL semantics:
  publishers preserve the initial inventory budget, forwarding peers consume
  it, and bounded deduplication accepts a later copy only when it carries more
  remaining budget. Long first-arrival paths can no longer suppress a valid
  shorter subscription route.
- Corrected the GitHub release bootstrap to fetch archives from GitHub's flat
  release directory while preserving the `/assets` default used by htree-hosted
  releases. Added an exact installer smoke test and changelog-backed release
  notes so publication fails before creating an unusable immutable release.
- Required `fips-core` 0.4.6 and released `hashtree-fips-transport` 0.4.6
  plus the superseding `hashtree-cli` 0.2.96 artifact release, keeping
  session-control handshake futures within Tokio's default worker stack without
  changing FMP, FSP, or blob wire formats.
- Routed advertised same-host FIPS blob requests through the daemon's existing
  `BlobRouter`, so a local miss can continue to its configured authenticated
  peer set without decrementing HTL. Released the correction in
  `hashtree-cli` 0.2.94 without changing the blob wire format.
- Corrected replaceable/addressable event resolution, storage, snapshots,
  bounded caches, and worker root tracking to use NIP-01's lexically lowest
  event ID when timestamps tie, without changing ordinary event ordering.
- Required final `fips-core` 0.4.5 and released `hashtree-fips-transport`
  0.4.5 so native CLI packaging uses the verified final FIPS runtime.
- Released `hashtree-core` 0.2.86 with the shared LMDB runtime needed by the
  packaged pool store, and moved the CLI release to 0.2.93.
- Gave the 1 MiB TypeScript crypto round-trip an explicit bounded test budget
  so the full gate remains deterministic on slower hosted runners.

## Adaptive pool storage - 2026-07-16

### Added

- Added `hashtree-lmdb` 0.2.84's application-owned `PoolStore`: persisted
  member identity and exact placement, hash-verified idempotent writes and
  moves, shared pins/access metadata, bounded adaptive member ordering, and
  safe add, drain, rebalance, repair, and remove operations.
- Added real multiprocess crash, resize, pin/GC, member-refresh, placement, and
  migration coverage, plus a resumable read-only LMDB migration command and
  systemd service template.

### Changed

- Made a fresh shared same-host LMDB store one canonical pool while preserving
  existing single-store layouts until explicitly migrated.
- Released `hashtree-cli` 0.2.93 with pool status/configuration/maintenance and
  migration commands.

### Removed

- Removed the four hot/legacy LMDB environment variables and their duplicate
  read/write/retention branches. Applications now write to one explicit store;
  the read-only `BlobRouter` continues to treat the complete pool as one opaque
  route.

## Blob routing crates - 2026-07-16

### Changed

- Added `BlobRouter` as the single adaptive read router across opaque routes,
  with explicit route preferences, bounded decaying outcomes, cooldown and
  recovery, route-local misses, bounded hedging, and central hash verification.
- Made Hashtree mesh resolution one composite route that retains peer selection,
  cycle suppression, caching, and HTL semantics; only mesh forwarding decrements
  HTL, while terminal and FIPS routes receive the request unchanged.
- Added `FipsBlobRoute` 0.4.4 as the sole owner of a deduplicated, bounded union
  of discovered and explicit FIPS peers, and removed `SameHostBlobStore`.
- Added `hashtree-lmdb` 0.2.83's canonical shared-store opener and documented
  the boundary between immutable blob sharing and mutable pins, quota, and GC.
- Released `hashtree-core` 0.2.85, `hashtree-network` 0.2.84, and
  `hashtree-cli` 0.2.89 for the unified route contract.

### Fixed

- Made cross-process quota and GC decisions read persisted LMDB totals instead
  of stale process-local byte counts.
- Removed the non-WebRTC fallback's duplicate mesh request types and framing;
  it now re-exports the canonical Hashtree network codec without wire changes.

## 0.2.88 - 2026-07-16

### Fixed

- Reused the live LMDB store while `htree add --publish` uploads the new DAG,
  avoiding a conflicting second environment open after the store resizes.
- Added a real CLI-process regression covering explicit publication from a
  resized store through both the file-server and Nostr stages.

## 0.2.87 - 2026-07-16

### Fixed

- Made explicit `htree add --publish` fail unless configured file servers accept
  the content and a configured Nostr relay accepts the root announcement.
- Published immutable content before the mutable Nostr root, stopped the
  resolver on both success and failure, and printed `published:` only after
  both required stages completed.

## 0.4.3 - 2026-07-16

### Fixed

- Recognized configured `nostr_relay:<npub>` peer addresses as canonical FIPS
  relay fallbacks at priority 250 instead of misparsing them as UDP addresses.
- Required `fips-core` 0.4.3 so an authenticated relay fallback keeps racing
  available direct and WebRTC upgrades.

## 0.4.2 - 2026-07-16

### Fixed

- Enabled the ordinary FIPS Nostr relay carrier whenever an embedded endpoint
  has relay discovery configured, reusing the canonical bounded
  `fips-core::NostrRelayAdapter` without changing FMP, FSP, or blob wire
  formats.

## 0.2.86 - 2026-07-16

### Fixed

- Made the htree daemon own the FIPS relay adapter for its full endpoint
  lifetime, so browser and native peers can establish the authenticated base
  session before negotiating WebRTC over the same configured relays.
- Packaged the daemon with `fips-core` 0.4.2 and
  `hashtree-fips-transport` 0.4.2 instead of resolving the older WebRTC stack.

## 0.4.1 - 2026-07-16

### Fixed

- Required `fips-core` 0.4.1 so WebRTC consumers receive the corrected
  multi-address offer handling instead of retaining the 0.4.0 registry lock.

## 0.4.0 - 2026-07-16

### Changed

- Exposed native endpoint configuration independently from the legacy mesh
  carrier, including endpoint-only WebRTC support.
- Removed the duplicate raw-datagram `legacy-mesh` carrier, framing, and
  feature. `hashtree-fips-transport` now has one authenticated TCP/FIPS blob
  protocol; `hashtree-network` remains the canonical HTL router and is
  composed through the shared `BlobRoute` contract.

### Fixed

- Retried only transient pre-establishment TCP/FIPS readiness failures, with a
  short delay bounded by the existing request deadline; authenticated protocol,
  hash, and post-establishment failures remain terminal.

## 0.2.85 - 2026-07-16

### Changed

- Replaced the htree daemon's legacy raw FIPS status and Nostr mesh carrier
  with one native embedded endpoint and one authenticated `nostr-pubsub`
  service shared by local-only root operations and decentralized relay events.
- Made the fixed loopback UDP rendezvous address optionally configurable for
  isolated full-stack tests while preserving the well-known production default.
- Made FIPS LAN/mDNS discovery independently disableable so loopback-only labs
  cannot silently use host-network discovery; the production default remains on.
- Removed the CLI's `legacy-mesh` feature dependency and obsolete Nostr mesh
  forwarding, fanout, HTL, framing, and payload helpers; canonical Hashtree
  blob routing remains on `BlobResolver` and `fips-tcp`.

### Fixed

- Made local adds that spill blobs into the deterministic external pack tier
  readable after a fresh process opens the store, while ordinary writes remain
  inline unless storage policy explicitly opts into external blobs.

## 0.3.1 - 2026-07-15

### Changed

- Generalized the authenticated `hashtree.blob/1` TCP/FIPS service from a
  direct store wrapper to any bounded `BlobRoute`, so a same-host provider can
  answer locally or continue through its normal Hashtree resolver.
- Added explicit strong and weak peer-route handles, live authenticated inbound
  policy, configurable local/standalone HTL, and transport deadlines that
  preserve the resolver's search budget.

### Fixed

- Rejected oversized requests, responses, and HTL values at every adapter
  boundary, and kept provider failure distinct from an explicit `NoResult`.

## 0.2.84 - 2026-07-15

### Changed

- Made `BlobRequest { hash, htl }`, `BlobReply::{Data, NoResult}`, and
  `BlobRoute` the shared retrieval contract across local stores, Hashtree mesh
  routing, same-host providers, and the daemon's HTTP cache path.
- Routed daemon FIPS retrieval through the canonical HTL resolver over
  `fips-tcp`, with exact one-step HTL consumption only at Hashtree mesh
  forwarding and central hash verification and caching.

### Fixed

- Preserved timeout, corruption, overload, and incomplete searches as retryable
  failures instead of false absence at the HTTP boundary.
- Bounded fanout, blob size, HTL, and authenticated inbound work; made hedged
  source cancellation abort-safe; and prevented same-hash requests with equal
  or different HTL budgets from overwriting or stranding each other.

## 0.3.0 - 2026-07-15

### Added

- Added a bounded Rust TCP/FIPS blob service and optional local-first Store
  wrapper that discovers providers through FIPS 0.4's authenticated same-host
  capability roster.
- Added a bidirectional Rust/TypeScript process gate for small and multi-segment
  hits plus explicit misses over real FIPS and TCP/FIPS implementations.

### Changed

- Made the old Rust mesh adapter an explicit `legacy-mesh` compatibility
  feature; the TCP blob path has no duplicate framing fallback.
- Updated the compatibility CLI to `nostr-pubsub-fips` 0.3.0 so every FIPS
  endpoint in the process uses the FIPS 0.4 type.

### Fixed

- Preserved pins while replacing corrupt cache data, kept mixed
  miss/provider-failure results as errors, and prevented client-only stores from
  serving inbound blobs.

## 0.2.83 - 2026-07-15

### Added

- Added the canonical `BlobRoute` request/reply boundary to `hashtree-core`,
  including compact HTL-aware request vectors and terminal Store routing.
- Let the `htree` daemon advertise its existing `StorageRouter` as an
  authenticated same-host blob provider through its existing FIPS endpoint.

### Changed

- Versioned only `hashtree-core`, `hashtree-cli`, and `hashtree-embedded` at
  `0.2.83`, allowing embedded consumers to resolve one Core/transport/FIPS type
  graph while workspace-versioned crates remain at `0.2.82` and
  `hashtree-fips-transport` advances independently to `0.3.0`.

## 0.2.82 - 2026-07-14

### Added

- Exposed bounded, first-winner delivery evidence for hash-verified mesh blocks,
  allowing application adapters to account for useful service without coupling
  Hashtree networking to a payment implementation.
- Reported delivery-evidence overflow explicitly so lost evidence is never
  inferred as a billable service claim.

### Changed

- Updated the optional service-accounting integration to `cashu-service` 0.3.0
  and the FIPS pubsub adapter to `nostr-pubsub-fips` 0.2.3.
- Restored warning-denying Rust 1.95 Clippy coverage across all targets and
  features, including benchmarks and async test serialization.

### Fixed

- Credited useful bytes only to the first valid responder; corrupt responses,
  late duplicates, and concurrent losers no longer receive reciprocity credit.
- Included every publishable workspace crate in the staged Cargo release plan.

## 0.2.81 - 2026-07-14

### Changed

- Updated the decentralized Nostr pubsub core, FIPS, relay, social-graph, and
  FIPS endpoint dependencies to their hardened production releases.

## 0.2.80 - 2026-07-13

### Changed

- Added a transport-neutral updater event cache that derives the exact signed
  Hashtree root subscription from the trusted release reference, consumes
  `nostr-pubsub` providers, and seeds relayless update resolution.
- Updated the FIPS pubsub adapter to the peer-serving `0.1.8` release with
  isolated service-port receive ownership and late-subscriber replay.

### Fixed

- Rejected malformed, wrong-author, wrong-tree, duplicate, and stale update
  announcements before they reach the release resolver.

## 0.2.79 - 2026-07-13

### Changed

- Routed browser Hashtree mesh traffic through the external FIPS provider and
  removed the duplicate direct WebRTC worker transport.
- Simplified shared Rust and TypeScript storage, indexing, Git, Nostr, and
  updater paths while preserving their existing public behavior.

### Fixed

- Hardened Git tree and pack transfer against incomplete or transient reads,
  bounded relay publication, and let normal pushes rebuild a published root
  only after a definitive `404`/not-found result.

## 0.2.78 - 2026-07-11

### Changed

- Let transport adapters ingest verified kind `30064`/legacy `30078` root
  events into `NostrRootResolver`, including relayless one-shot resolution and
  open subscription updates.
- Added a secure updater builder that seeds its resolver from signed root
  events supplied by decentralized pubsub.

## 0.2.77 - 2026-07-09

### Changed

- Added normal Nostr-filter import, query, and local-relay replay for stored
  event indexes, including signed rating facts.
- Added feature-gated decentralized Nostr pubsub over FIPS with bounded caches
  and subscription-routed inventory adverts.

### Fixed

- Closed failed, duplicate, replaced, and timed-out FIPS WebRTC sessions so
  repeated inbound offers cannot leak ICE sockets or drive an mDNS CPU loop.
- Deduplicated non-replaceable Nostr events within one index batch, preventing
  repeated relay events from failing derived-index updates.

## 0.2.76 - 2026-06-29

### Fixed
- Reuploaded a missing published git root from the local hashtree store when
  push ref discovery resolves the root event but Blossom returns 404 for that
  tree, then retried normal ref loading before deciding whether a push is stale.

## 0.2.75 - 2026-06-29

### Fixed
- Rejected ordinary htree git pushes when existing remote state cannot be read,
  keeping network/root-download failures from being treated like empty new
  repositories. Use `git push --force` only for explicit root repair.

## 0.2.74 - 2026-06-29

### Changed

- Ordered local tree-root publishes by write time so browser/runtime consumers
  replay locally queued roots in the same order they were written.
- Buffered FIPS app message bursts before forwarding them to the runtime so
  startup-time peer traffic is less likely to be dropped.
- Tidied `htree add --publish` output so mutable links and helper URLs are
  grouped more clearly after a publish.

### Fixed

- Pinned shared Dexie package versions in the TypeScript workspace so downstream
  consumers avoid unresolved workspace ranges.

## 0.2.73 - 2026-06-23

### Changed

- Kept htree release bootstrap publishing as a top-level `install.sh` while
  removing same-origin checksum/signature sidecar verification from that
  bootstrap path.
- Tightened release artifact defaults so prebuilt binaries omit optional
  FUSE/WebRTC paths while source builds can still opt into them.

### Fixed

- Rejected Blossom uploads at the route layer before JSON/body extraction and
  body-limit handling when upload auth is missing or invalid.
- Hardened WebSocket relay and Git import paths against untrusted or mismatched
  blobs reaching trusted storage or clients.
- Preserved legacy Nostr contact closure behavior while avoiding fallback
  default secret material.

## 0.2.72 - 2026-06-22

### Changed

- Published mutable hashtree roots as Nostr kind `30064` while keeping legacy
  kind `30078` read/query compatibility.
- Added embedded daemon bootstrap roots for native hosts that need to serve
  app-provided hashtree roots before relay resolution catches up.
- Lowered browser embedded storage/social graph defaults and allowed explicit
  browser settings to use smaller LMDB map sizes.

### Fixed

- Reported the last indeterminate upstream Blossom miss reason in daemon status
  to make native/browser root and blob fetch diagnosis clearer.
- Fixed chunked-file reads to follow explicit child link types so raw file
  chunks that resemble MessagePack tree nodes are not mis-decoded.
- Updated architecture docs to describe kind `30064` roots with legacy `30078`
  compatibility.

## 0.2.71 - 2026-06-17

### Fixed

- Restored explicitly allowed mutable `npub` plaintext release routes for
  encrypted hashtree roots while keeping public-supplied decryption keys
  forbidden on HTTP content-serving routes.

## 0.2.70 - 2026-06-17

### Changed

- Cached immutable file chunk metadata and seek directly to overlapping uniform
  chunks for range reads, cutting hot PMTiles range latency from roughly 62 ms
  average to roughly 5 ms average in the live origin benchmark.
- Made public content-addressed HTTP routes serve raw blobs/ciphertext only:
  direct hashes and `nhash` no longer assemble logical files or accept
  decryption keys, while explicitly configured virtual hosts and approved
  mutable `npub` routes remain the plaintext-serving exceptions.

### Fixed

- Replaced a timing-based Nostr mirror upload test delay with an explicit
  fake-server hold/release gate so threaded Rust test runs no longer depend on
  a lucky 250 ms scheduling window.

## 0.2.69 - 2026-06-16

### Changed

- Added an underfull first-publish Git pack checkpoint for medium repos so
  `git-remote-htree` avoids hundreds of loose Git-object uploads below the
  normal deterministic checkpoint interval.
- Improved `htree add --local` on large LMDB stores by using bulk local ingest,
  link-aware indexing, relaxed per-blob fsyncs, and one final store sync,
  roughly doubling the measured 4 GiB local-add throughput versus the prior
  optimized path while preserving the default chunk size.

### Fixed

- Avoided writing orphan external LMDB pack files when a batch upload repeats
  blobs that are already present, reducing retry/replay write amplification.
- Served tree-host `HEAD` requests from metadata and streamed full-file `GET`
  responses so large release assets do not require full blob reads before the
  first byte is sent.
- Added force and shallow file-server push modes, and made release publishing
  seed both artifact DAGs and mutable release roots to the public upload host.
- Recovered release publishing when a mutable release tree had previously been
  pointed directly at a release directory instead of the versioned parent tree.

## 0.2.68 - 2026-06-12

### Fixed

- Refreshed gateway mutable-root caches during hashtree and Homebrew release
  publishing so live URL gates do not validate stale release roots.
- Updated Homebrew release smoke checks and install instructions for Homebrew's
  explicit trust requirement on non-official taps.

## 0.2.67 - 2026-06-09

### Fixed

- Made mutable npub/htree HTTP reads refresh stale cached tree roots from
  local/upstream relays before serving files, while falling back to stale roots
  when refresh misses so origin caches no longer pin releases forever.

## 0.2.66 - 2026-06-08

### Changed

- Collapsed `git-remote-htree` checkpoint pack install output into one
  aggregate `Loading git packs` progress line, using terminal same-line updates
  when possible and throttled pipe-safe heartbeats otherwise.

## 0.2.65 - 2026-06-08

### Changed

- Made `git-remote-htree` enumerate Git pack metadata directly from
  `.git/objects/pack` and load loose-object prefixes concurrently, reducing
  fresh clone object-tree enumeration without adding a current-tip tail pack.
- Kept fetches pack-aware by installing checkpoint packs before the loose
  object local check, so objects already covered by installed packs are not
  fetched or written again.

### Fixed

- Refused to publish a mutable Git root when the local repo tree is incomplete,
  Blossom upload replication is degraded, or the uploaded root is not readable
  from a configured write server.
- Treated local-daemon roots as fallbacks unless they came from a live Nostr
  source, so stale daemon cache entries cannot hide a newer relay root.

## 0.2.64 - 2026-06-08

### Changed

- Made `git-remote-htree` clone and push progress line-oriented with bounded
  update intervals, so Git/SSH sideband output no longer repeats
  carriage-return transfer fragments while long pack downloads still report
  periodic byte progress.

## 0.2.63 - 2026-06-08

### Fixed

- Fixed new tag/ref pushes over existing remote history so they choose an
  existing remote ref as the delta base, preserving checkpoint-pack clone
  behavior instead of relisting the whole repository as loose objects.
- Rebuilt missing checkpoint-pack roots from deterministic checkpoint
  boundaries without adding a current-tip tail pack.

## 0.2.62 - 2026-06-08

### Changed

- Let `git-remote-htree` use the multi-server loose-object download concurrency
  when a local daemon read server is followed by remote read servers, while
  keeping direct/single-server reads on the conservative direct-origin default.

## 0.2.61 - 2026-06-08

### Fixed

- Fixed `git-remote-htree` clone and push recovery when the local htree daemon
  resolves a stale or mismatched repository root/key. Daemon roots are now
  validated before being cached, and clone fetches retry via relays if the
  daemon root fails while loading the git object tree.
- Made encrypted hashtree decode failures while reading remote git refs surface
  as integrity errors instead of silently advertising an empty remote.

## 0.2.60 - 2026-06-08

### Changed

- Changed `git-remote-htree` pack checkpoints from overlapping full packs to a
  deterministic chain of checkpoint pack ranges. Later checkpoint packs now
  exclude the previous checkpoint tip, so independent publishers with the same
  history can reuse earlier pack blobs instead of reuploading the same history
  inside a different pack.
- Made checkpoint pack generation independent of local Git pack reuse by
  disabling object and delta reuse and pinning pack settings.

### Fixed

- Kept untracked `.gitignore`d files out of checkpoint packs and added
  regression coverage for that path.
- Made `git-remote-htree` storage unit tests use an explicit filesystem backend
  instead of inheriting the developer machine's configured storage backend.

## 0.2.59 - 2026-06-08

### Changed

- Increased `git-remote-htree` clone fetch concurrency for direct Blossom reads
  and recorded the clean-cache clone performance results.
- Preserved Git pack checkpoints across delta pushes and avoided rechecking or
  reuploading objects already represented by inherited packs.
- Moved product-aware updater helpers into `hashtree-updater` so CLI update
  flows can share the same manifest and target selection logic.

### Fixed

- Fixed pack-backed delta pushes so inherited pack objects are not emitted as
  broken loose objects while current tree objects remain available for new refs.
- Fixed pack-only base reuse during pushes so unchanged visible files can be
  merged from the cached root without requiring all old loose Git objects.

## 0.2.58 - 2026-06-07

### Changed

- Updated the Rust Nostr dependency stack to `nostr`/`nostr-sdk` 0.44 and
  migrated event builders, relay messages, filters, timestamps, and query APIs.

### Fixed

- Fixed encrypted large-directory traversal so chunk fanout nodes use their own
  link keys across recursive, parallel, and streaming walks.
- Fixed missing intermediate tree chunks to surface as missing data instead of
  resolving paths as absent.
- Fixed git remote repair pushes to re-upload missing old-tree chunks to the
  servers that need them, while ignoring corrupt local cache entries.
- Fixed first raw-blob fetch responses to report the actual upstream source in
  `X-Source`.
- Fixed embedded reload expectations for non-P2P builds and LMDB map-size
  reopen sizing around existing environments.

## 0.2.57 - 2026-06-07

### Added

- Added Git pack checkpoints to `git-remote-htree` pushes and clone fetches so
  large repositories can install ordinary Git pack files instead of fetching
  every historical object as loose helper state.
- Added Blossom upload-check support and git push batching, reducing request
  count when many blobs are missing.

### Changed

- Made Git pack checkpoint generation deterministic so repeated pushes at the
  same checkpoint tip converge on the same pack blobs.
- Changed `git-remote-htree` same-tip pushes to advertise existing refs for
  `list for-push`, allowing Git to return `Everything up-to-date` without
  rechecking already-present Blossom blobs.
- Reduced clone-side tree-walk concurrency and retried transient Blossom cache
  misses so sparse LMDB-backed servers are not overwhelmed by many concurrent
  clone clients.
- Streamed Git pack checkpoint downloads directly into `.git/objects/pack` with
  byte-level progress before indexing.

### Fixed

- Fixed local daemon root cache lookups for git repos by using the `npub`
  identifier expected by the HTTP resolver, preserving the root decryption key
  after daemon restarts.
- Fixed `git-remote-htree` Blossom client setup to use the resolved helper
  config instead of reloading default write servers.

## 0.2.56 - 2026-06-06

Changes since the `0.2.54` release.

### Added

- Added a Blossom `POST /upload/batch` path that stores multiple uploaded blobs
  with one auth event and one owner-index batch write, including support for
  camelCase `contentType` payloads.

### Changed

- Changed Hashtree release publishing to require the complete macOS, Linux,
  and Windows CLI asset set for normal releases.
- Changed Hashtree release publishing to verify live versioned and `latest`
  URLs for every staged release file before continuing past the release gate.
- Changed post-publish install matrix failures to stop the release instead of
  only warning after publishing broken artifacts.
- Changed the install matrix smoke runner to use explicit non-login shells so
  the just-installed binaries and git helper are the ones being tested.
- Moved the reusable `cashu-service` crate out of the Hashtree workspace while
  keeping Hashtree's `htree-cashu` helper crate in the publish plan.
- Changed `htree release publish --draft` to update the sibling `draft`
  pointer while leaving the stable `latest` pointer unchanged.
- Changed `git-remote-htree` fast-forward pushes to merge deltas with the
  cached remote root directly, so normal pushes no longer report a
  cached-root repair before uploading the new tree.
- Changed Blossom owner listings to use a per-owner blob index for new writes,
  avoiding full owner-list rewrites on duplicate uploads while preserving
  compatibility with existing list records.

### Fixed

- Fixed the release path so `latest/install.sh` and platform asset 404s are
  caught immediately after publishing rather than surviving into the advertised
  quick-install command.
- Fixed `git-remote-htree` pushes when the previously published root is missing
  from configured file servers: the helper now preserves root metadata before
  ref hydration and probes existing remote blocks instead of blindly reuploading
  large histories.

## 0.2.54 - 2026-05-21

Changes since the `0.2.53` release.

### Changed

- Kept the daemon FIPS endpoint enabled by default in normal mode, including
  ordinary UDP and FIPS WebRTC endpoint transports, so HTTP blob misses can be
  satisfied through verified FIPS peer responses.
- Renamed the daemon FIPS miss-fetch setting to `fetch_from_fips_peers` while
  keeping `http_fips_fetch` as a legacy config alias.
- Updated the FIPS dependency to 0.3.16 so Linux musl release builds do not
  pull in gateway-only nftables bindings.
- Increased the CLI metadata LMDB map sizing from tiny fixed defaults to a
  storage-budget-derived allocation, avoiding map-full failures on larger
  stores while keeping same-process reopens stable.
- Moved blob access-time metadata writes off the foreground read path and
  bounded each background update batch.
- Added timeouts around synchronous S3 bridge operations so foreground storage
  calls do not hang indefinitely behind a degraded S3/R2 backend.
- Extended the TypeScript Blossom store with configurable read timeouts and an
  option to skip pre-upload existence checks when write endpoints handle
  duplicates.

## 0.2.53 - 2026-05-20

Changes since the `0.2.52` release.

### Changed

- Changed `git-remote-htree` repo-tree construction to emit live progress
  phases and counters while pushes build objects, refs, index entries, and
  working-tree files, so slow cached-root retries no longer appear stuck at
  `Building repo tree...`.
- Replaced the legacy Hashtree WebRTC adapter path with the FIPS transport
  direction and removed the old WebRTC stack from the Rust/TypeScript surface.

## 0.2.52 - 2026-05-20

Changes since the `0.2.51` release.

### Added

- Added `htree release publish --draft` for publishing versioned release
  entries without repointing the sibling `latest` pointer.

### Changed

- Changed `git-remote-htree` pushes so configured write servers act as
  best-effort replicas; a complete local publish can still succeed when remote
  Blossom replication is degraded.
- Changed Rust CID-link B-trees to store subtree link counts on internal
  directory entries, matching the TypeScript counted B-tree root format.
- Added Rust `BTree::count_stored_links(...)` for no-scan stored counts and
  `BTree::scan_links(...)` for explicit full scans. `BTree::count_links(...)`
  remains the compatibility scanning count.

## 0.2.51 - 2026-05-19

Changes since the `0.2.50` release.

### Added

- Added daemon and HTTP status metrics for Blossom upload queues, blob-read
  backpressure, and peer/router bandwidth so production storage pressure is
  visible without digging through logs.
- Added targeted R2 blob-import helpers plus resumable/import comparison paths
  that can stream through local storage instead of loading whole scans into
  memory.
- Added raw block storage APIs to the TypeScript worker so apps can store
  already-addressed blocks, optionally upload them to Blossom, and safely serve
  published raw blocks to peers.

### Changed

- Changed Blossom upload admission so optimistic uploads enter the bounded byte
  queue before the LMDB existence preflight, reducing push-visible latency while
  keeping the origin as the backpressure point.
- Changed Blossom and blob read paths to coalesce duplicate reads, bound
  blocking storage work, serve raw blob ranges efficiently, and cache immutable
  misses/hot reads at the edge.
- Reduced origin read/upload pressure with throttled Blossom HEAD/origin reads,
  transient R2 read retries, and cheaper duplicate optimistic upload handling.

### Fixed

- Fixed repeated no-op `git-remote-htree` pushes so exact fast-forward no-ops
  skip local repo-tree rebuilds instead of rewalking unchanged refs and tags.
- Fixed stalled Blossom uploads and TypeScript Blossom writes to abort rather
  than hang indefinitely.
- Fixed public Blossom ingest so signed Nostr snapshots and hashtree metadata
  pass the untrusted ingest filter while non-canonical import keys are skipped.
- Fixed WebRTC peer fetches so local HTTP peer discovery no longer stalls the
  worker path.

## 0.2.50 - 2026-05-14

### Added

- `htree stats` now reports storage size in human-readable units and, when the
  daemon is reachable, includes peer count plus in-memory mesh/relay traffic
  totals with the daemon uptime window.
- `htree peer` now prints aggregate peer-router traffic and mesh frame counters
  before the per-peer list.
- The daemon status API now exposes `daemon_started_at`, `uptime_seconds`, and
  aggregate `/api/peers` bandwidth counters so CLI traffic totals are clearly
  scoped to the current daemon process.

### Changed

- Reworded user-facing storage counters from internal "DAG" labels to
  "stored objects" and "pinned items".
- Release publishing now pushes updated release DAGs incrementally instead of
  re-uploading the whole release tree.
- The simulation harness gained additional pubsub baselines, shared peer-graph
  setup, tunable HTL budgets, and larger author workload distributions.

### Fixed

- `htree add` no longer runs storage eviction before the newly added root is
  pinned.
- Disabled Nostr/sync configurations now skip their background ref scans and
  keep request handlers responsive.
- Windows release artifact builds now run over SSH on `win11-dev` instead of
  relying on Parallels shared folders.

## 0.2.49 - 2026-05-06

### Added

- Added Rust wire protocol support for pubsub inventory and want messages:
  `PubsubInventory` (`0x0a`) and `PubsubWant` (`0x0b`).
- Added `PubsubDeliveryMode` so production mesh pubsub can choose between
  old full-frame interest push and inventory-first HTL flood delivery.

### Changed

- Changed production `hashtree-network` pubsub to default to HTL
  inventory/want delivery. Publishers flood small inventories and interested
  peers pull payload frames back along want routes, while the previous
  interest-push mode remains selectable for low-latency live-byte experiments.
- Extended the pubsub comparison harness with explicit production delivery
  modes and an `--only=` filter for large deterministic sweeps.

## 0.2.48 - 2026-05-04

### Fixed

- `tauri-plugin-hashtree-updater`: `download_and_install` defaulted
  `manifest_path` to `manifest.json` while `check` defaulted to
  `release.json`, so apps relying on the default would pass Check
  ("update available") and then fail Install with `manifest was not
  found at manifest.json`. Both code paths now default to
  `release.json` (the name `htree release publish` writes), and the
  config doc string was updated to match.

## 0.2.47 - 2026-05-04

### Added

- `htree update` refuses by default when the running binary lives under
  a known package-manager path (`~/.cargo/bin/`, `*/Cellar/*`,
  `*/homebrew/*`) and suggests the native upgrade command (`cargo
  install hashtree-cli --force` or `brew upgrade htree`) so the
  package manager's metadata stays in sync. Pass `--force` to bypass
  and replace the binary in place anyway.
- The auto-check notification picks the install-source-appropriate
  hint (`cargo install …`, `brew upgrade htree`, or `htree update`)
  based on the same path detection.
- The startup-hook read of the cached update result is bounded to
  50ms via a worker thread + `recv_timeout`, so a hung NFS/FUSE mount
  on `~/.hashtree` can't stall command startup.
- htree now opportunistically self-checks for updates in the background.
  Before each command (except `htree update`), it spawns a fire-and-
  forget task that — at most once per `updater.check_interval_hours` —
  resolves `releases/hashtree/latest` and either prints a one-liner to
  stderr ("htree update available: vX.Y.Z — run `htree update` to
  install") or, when `updater.auto_install = true`, replaces the running
  binary in place. Throttling is mtime-based on
  `~/.hashtree/data/last-update-check`. New `[updater]` section in
  `~/.hashtree/config.toml`:
  ```toml
  [updater]
  auto_check = true            # default on
  auto_install = false         # default off — opt in for hands-off
  check_interval_hours = 24
  ```

### Changed (breaking CLI)

- Renamed `htree update {check,download,install}` to a flat `htree install`
  with `--check` and `--download-only` mode flags. Reads better as a noun
  ("install <ref>") and frees `htree update` for self-updating htree
  itself.
- New `htree update [--check]` upgrades the running htree binary in place
  by resolving its own published reference (`releases/hashtree/latest`).
  Useful when htree was installed via `htree install` rather than
  `cargo install` (which has its own upgrade story).
- `htree install` now defaults `--to ~/.local/bin/<binary-name>` for the
  `binary` and `binary-archive` kinds when no destination is given.
  `app-bundle` and `appimage` still infer the destination from
  `current_exe()` since they typically self-replace.

## 0.2.46 - 2026-05-04

### Added

- Added `AssetKind::BinaryArchive` to `hashtree-updater`: a `.tar.gz` (or
  raw tar) containing one binary plus auxiliary files. The manifest's
  `executable` field names the entry to extract (eg `iris/iris`). The
  install dispatcher decompresses, finds the entry, and atomically writes
  it to the destination with the executable bit set. Cross-platform
  (no per-OS install support needed since it's just file extraction).
- Asset-kind inference now upgrades a plain archive to `BinaryArchive`
  when the manifest sets a non-empty `executable` field, so apps with the
  conventional `<name>-<target>.tar.gz` layout (eg iris-chat-rs) work
  without setting `kind` explicitly — only the `executable` hint.
- `htree update install` learned a `--archive-entry <path>` flag that
  overrides the manifest's `executable` field, useful for testing the
  install path before a publisher updates their `release.json`.

### Changed

- `flate2` and `tar` are now unconditional dependencies of
  `hashtree-updater` (were target-gated to macOS/Linux). Both crates are
  small and the binary-archive dispatcher needs them on every platform.

## 0.2.45 - 2026-05-04

### Added

- Added `hashtree-updater`, a reusable Rust updater crate that resolves signed
  `npub/tree/path` release roots, reads the existing `release.json` (the same
  file `htree release publish` consumers already write), selects the asset for
  the current platform, downloads chunks via hashtree (which authenticates
  every chunk against its CID), and emits Started/Progress/Finished events for
  UIs that want to render a progress bar.
- Added an `AssetKind` taxonomy (`binary`, `app-bundle`, `appimage`, `deb`,
  `rpm`, `nsis`, `msi`, `archive`) with platform install dispatchers: atomic
  file swap with executable bit for `binary`, `tar.gz` → `*.app` swap with
  AppleScript admin-elevation fallback for macOS `app-bundle`, gunzip + chmod
  in place for Linux `appimage`. Other kinds return `UnsupportedKind` so apps
  can fall back to opening the release page.
- Added filename inference for `target` and `kind` so the existing
  git.iris.to-style `release.json` schema (just `tag` + `assets[].name`)
  works without per-asset annotations: `…-linux-arm64.AppImage` →
  `linux-aarch64` + `appimage`, `…-macos-arm64.app.tar.gz` →
  `darwin-aarch64` + `app-bundle`, etc.
- Added `htree update {check, download, install}` to the CLI for plain Rust
  apps that prefer to shell out instead of linking the library. A
  `FetchingStore` adapter bridges the resolver/Fetcher pair into the
  updater's Store-generic API so chunks are pulled from WebRTC/Blossom on
  demand.
- Added `tauri-plugin-hashtree-updater`, a Tauri v2 plugin wrapping the core
  for desktop apps. Exposes `check()` and `Update.downloadAndInstall()` over
  IPC, with a Channel-based progress event stream and a TS guest API
  (`@hashtree/tauri-plugin-updater`). Auto-detects install destination from
  `current_exe()` so apps using the standard tauri-bundler layout don't need
  any path config — just the `htree://` reference.
- Documented the integration pattern (config schema, capability, JS API,
  copy-pasteable ~70 line prefs/banner helper, recommended UX) in the new
  `tauri-plugin-hashtree-updater/README.md`.

### Fixed

- Plugin now falls back to `NostrResolverConfig::default()`'s built-in 3-relay
  set when the app's `tauri.conf.json` doesn't list any, instead of silently
  passing an empty relay list and surfacing every check as "release root not
  found".
- Plugin maps `ReleaseNotFound` and `ManifestNotFound` from `check()` to
  `Ok(None)` so frontends can render a quiet "no releases yet" / "you're up
  to date" instead of leaking the technical error.
- Removed the redundant per-asset `sha256` and `size` fields from the
  manifest schema — the resolved root CID already authenticates every byte
  through the hash chain, so re-checking was a no-op and a footgun for
  stale manifests.

## 0.2.44 - 2026-05-02

### Fixed

- Fixed `git-remote-htree` cached-root retries so repeated pushes reuse the already-open git blob
  store instead of reopening the shared LMDB cache.
- Fixed LMDB map sizing to align to the host page size instead of assuming 4 KiB pages.
- Fixed Nostr relay query paths in `hashtree-cli` to bound blocking query batches and social-graph
  candidate fanout.
- Fixed a flaky static HTTP clone test by running `git clone` from a stable working directory.

## 0.2.43 - 2026-04-27

### Fixed

- Fixed Nostr mirror and relay startup memory spikes by bounding Blossom diff uploads, full-history
  crawl retention, relay index scans, and per-subscription local query batches, then trimming
  transient allocator pages after large mirror and relay batches.
- Fixed multi-author, multi-kind Nostr relay queries to use the author-kind index instead of broad
  kind scans that decoded large unrelated event sets.
- Improved Nostr relay diagnostics with process memory snapshots and compact filter summaries around
  trusted local queries and upstream subscription fanout.
- Improved Docker rebuild behavior so source edits reuse Cargo's persistent target cache instead of
  forcing dependency rebuilds through placeholder crate artifacts.
- Fixed owned Blossom uploads so they respect the configured durable storage limit: when protected
  owned blobs fill the local store, new owned uploads now fail with a storage-limit error instead
  of growing the LMDB blob store past the quota.
- Fixed LMDB blob-store stats so quota checks use transactionally maintained counters instead of
  scanning blob data on every check, avoiding slow starts and uploads on large stores.
- Fixed hashtree metadata LMDB environments to start with a small map instead of reserving a
  10GB map for tiny test and CLI stores, while still reopening larger existing environments.

## 0.2.39 - 2026-04-24

Changes since the `0.2.38` release.

### Added

- Added native WebRTC peer-state persistence for known peer hints and peer transfer statistics, so restarted daemons can prioritize previously useful peers and reconnect directly when their observed addresses are still valid.

### Changed

- Changed directed WebRTC-over-Nostr signaling to carry a sender-signed NIP-59 seal inside the encrypted relay envelope, keeping direct connection details private from relays while deriving the peer identity from the verified seal signer.

### Fixed

- Fixed native peer restart behavior so peers with persisted direct addresses can reconnect and fetch over WebRTC after relay rendezvous is unavailable, with e2e coverage for relay shutdown after initial discovery.

## 0.2.38 - 2026-04-22

Changes since the `0.2.37` release.

### Improved

- Improved `git-remote-htree` cached-root fast-forward pushes so large repos reuse unchanged working-tree subtrees, index metadata, and object-prefix directories from the previous published root instead of recursively hydrating the whole old repo into the local cache before every push.

### Fixed

- Fixed repeated `git-remote-htree` pushes against large published repos such as `iris-chromium` so the helper no longer falls into a long-running cached-root object-decrypt walk after already reducing the Git-object enumeration to the real delta.
- Fixed long cached-root merge phases to print an explicit progress label (`Reusing unchanged paths from cached remote root...`) instead of going quiet after the initial fallback line.
- Fixed mutable `htree release publish` updates so repointing `latest` survives missing local LMDB blobs by hydrating needed DAG chunks on demand, while keeping the release updater on the cheap parent-path fetch instead of silently reloading the entire release history first.

## 0.2.37 - 2026-04-22

Changes since the `0.2.35` crates.io release.

### Added

- Added tracked author mirroring plus end-to-end mirror coverage in `hashtree-cli`, so mirrored author trees and the renamed `htree mirror` flow are exercised against real daemon/test environments instead of only helper seams.
- Added an explicit embedded-daemon reload API for native hosts, making it possible for browser and app integrations to refresh transport and server settings without rebuilding the host-side bridge object.

### Improved

- Improved the embedded browser/native daemon runtime so host-managed relay, Blossom, and transport settings are honored consistently across startup and reload.
- Improved `git-remote-htree` repeated fast-forward pushes by hydrating existing remote objects from the cached hashtree root and enumerating only the Git-object delta instead of rewalking the full reachable history on every push.
- Improved the worker/mesh runtime with the current streaming/provider-bridge updates that keep the shared P2P stack aligned across the Rust and TypeScript hosts.

### Fixed

- Fixed repeated `git-remote-htree` fast-forward pushes so the helper can reuse cached remote objects and fall back cleanly when the delta set is incomplete instead of forcing a full-history local import on every push.

## 0.2.35 - 2026-04-20

Changes since the `0.2.34` crates.io release.

### Added

- Added a reciprocity-aware upload scheduler in `hashtree-network` so peers that have previously seeded us are preferred when outbound response bandwidth is contested.
- Added Blossom push helpers, storage compaction coverage, and broader social-graph/profile-index maintenance coverage across the CLI and Nostr crates.

### Improved

- Switched default read-side peer selection to weighted reply-likelihood ordering across the Rust and TypeScript mesh stacks.
- Improved CLI mirror, crawler, storage-maintenance, and relay-backed indexing behavior for larger profile/event histories.

### Fixed

- Fixed Rust package version wiring so the `0.2.35` workspace resolves cleanly from a fresh checkout.
- Fixed the worker/mesh upload path so capped WebRTC upload bandwidth is still enforced while reciprocal peers receive priority under contention.

## 0.2.34 - 2026-04-17

Changes since the `0.2.33` crates.io release.

### Added

- Added a hashtree-first app-building guide and library-side query surfaces for relay-backed event stores and collection sources, so apps can stream and query mutable data without rebuilding ad hoc Nostr HTTP layers.
- Added a portable PWA export helper plus broader live export coverage for manifest metadata, route metadata, and link-hinted assets.
- Added a Cashu send-payment helper and broader mirror/event-index maintenance improvements, including root republish support and expanded mirror history coverage.
- Added heavier media-oriented `hashtree-sim` transport scenarios plus a Tokio virtual-clock mode for `VirtualSteps`, so slow first-byte, throughput, and stall behavior can be tuned without waiting on wall-clock sleeps.

### Improved

- Improved mesh request handling so slow or hedged sources are tracked as timeouts instead of synthetic misses, making peer selection and retry behavior more stable on cold media reads.

### Fixed

- Fixed mutable root resolution so relay lookups only use the encoded tree name and resolve slash-separated subpaths inside the published tree instead of treating them as Nostr `d` tags.
- Fixed PWA export metadata preservation so released shells keep the intended manifest fields, route metadata, and asset links.
- Simplified the repo release pipeline by removing the obsolete sibling Iris packaging path from hashtree releases.
- Fixed mesh-store read accounting so late or slow responders no longer poison route quality as hard misses when the request was still in flight.

## 0.2.33 - 2026-04-13

Changes since the `0.2.32` crates.io release.

### Added

- Added `htree load` plus fetch progress reporting for loading published app/site payloads through the CLI.
- Added assist-mode `hash_get` support and an embedded daemon runtime for native hosts, making it easier to embed hashtree capabilities without a separate daemon process.
- Added reusable worker client/bootstrap surfaces so app hosts can embed the relay-backed worker runtime directly.

### Improved

- Improved adaptive mesh block scheduling and unified mesh query routing across the Rust and TypeScript stacks, reducing recursion and making upload/request prioritization more predictable.
- Neutralized and documented the generic worker runtime surface so app hosts can consume it without Iris-specific naming.

### Fixed

- Fixed Windows release verification flow during the cross-platform release pipeline.
- Fixed worker runtime issues around detached cached buffers and primary-store mesh recursion.

## 0.2.32 - 2026-04-10

Changes since the `0.2.31` crates.io release.

### Improved

- Added a real install-matrix smoke runner that exercises the documented install flow across the native host, Docker Linux targets, a Windows VM, and Homebrew with bounded Docker timeouts and per-platform logs where supported.
- The Windows VM artifact builder now stages the Rust workspace through a tar archive instead of bulk-copying the repo tree from the Parallels shared folder, which avoids the previous shared-folder copy failures on release builds.

### Fixed

- Fixed `git-remote-htree` on Linux environments where LMDB initialization returns `ENOSYS` by falling back to filesystem-backed local storage instead of failing the remote-helper startup.
- Improved the Windows VM smoke path to use the built-in `curl.exe` and `tar.exe` tools instead of the noisy `Invoke-WebRequest` path.
- Release publishing now runs the live install matrix after publishing the release tree and Homebrew tap, but only warns on failures so it does not strand an otherwise published release.

## 0.2.31 - 2026-04-10

Changes since the `0.2.30` crates.io release.

### Added

- Added a shared `.collection-manifest.json` contract for published collection roots, so peers can inspect `schemaVersion`, item/projection formats, and optional schema references directly from the root.

### Improved

- Aligned Rust and TypeScript `hashtree-collection` / `hashtree-nostr` roots around the same published collection manifest metadata and added interop coverage that compares the actual emitted metadata payloads.

### Fixed

- Fixed release publishing safeguards so the generated installer is only exposed when the full archive set is present, and release validation now derives the canonical upload npub from the documented install command.
- Fixed macOS release builds on Apple Silicon hosts so Intel macOS archives can still be produced for the published release set.

## 0.2.30 - 2026-04-10

Changes since the `0.2.29` crates.io release.

### Improved

- Bootstrapped new local identities with a default social-graph entrypoint follow, so fresh `htree socialgraph` usage starts from a reachable root instead of an isolated local user. This bootstrap is local-only and can be disabled with `nostr.bootstrap_follows = []`.
- Seeded the default aliases file with `siriusbusiness` for the binary releaser npub and updated the Rust docs to describe the new alias/bootstrap behavior.
- Forced local contact-list files into the social-graph state when the CLI starts or opens the graph, so newly seeded follows appear in stats immediately without requiring a separate publish step.

## 0.2.29 - 2026-04-10

Changes since the `0.2.28` crates.io release.

### Added

- Added the new `hashtree-collection` crate to the published Rust crate graph, including collection index lifecycle support, search indexes, schema hooks, federated search helpers, and explicit reindex support.

### Improved

- Switched the Rust social-graph dependencies from pinned GitHub revisions to the published `nostr-social-graph` crates.io releases.
- Improved `htree socialgraph stats` so the root prints as an `npub` and the reachability count is labeled more explicitly.
- Trimmed the default relay set by removing `wss://offchain.pub`.

### Fixed

- Fixed `htree cat` for published tree paths by resolving file targets inside encrypted directories with their decrypt keys intact and by rejecting bare directory CIDs unless a file path is specified.

## 0.2.28 - 2026-04-07

Changes since the `0.2.27` crates.io release.

### Fixed

- Fixed shared LMDB cache pressure handling so bounded stores evict stale blobs on the write path instead of only after later cleanup passes. This keeps `git-remote-htree` and other shared-store writers from wedging and requiring a fresh `HTREE_DATA_DIR` workaround.
- Added a real `git-remote-htree` regression test that fills a bounded shared LMDB cache and verifies new tree writes evict stale entries instead of letting the cache grow past its budget.

## 0.2.27 - 2026-04-06

Changes since the `0.2.26` crates.io release.

### Fixed

- Fixed LMDB-backed local stores so configured size increases actually grow the map instead of leaving `git-remote-htree` and other bounded stores stuck at the default 10 GiB mapping.
- Fixed local eviction behavior around uploads and mutable tree refs: `htree add` now runs quota eviction before and after indexing, superseded published roots are unpinned when refs move, and `git-remote-htree` evicts stale local blobs before building new trees so full stores recover without manual cleanup.
- Fixed `hashtree-cli` test coverage for the current peer-router state shape so targeted CLI test runs compile cleanly again.

## 0.2.26 - 2026-04-06

Changes since the `0.2.25` crates.io release.

### Improved

- Moved the production mesh runtime, transport orchestration, peer/session logic, and Cashu-aware request plumbing out of `hashtree-cli` and into `hashtree-network`, so Bluetooth, multicast, Wi-Fi Aware, and WebRTC now sit behind the shared mesh runtime instead of a CLI-owned stack.
- Tightened release coverage by wiring a real Rust E2E smoke job into CI and by keeping the offline LAN Docker verification path aligned with the locked Rust dependency graph used for releases.

## 0.2.25 - 2026-04-04

Changes since the `0.2.24` crates.io release.

### Improved

- Clarified `htree mount` semantics so the first argument is always a published hashtree target, while explicit mountpoints can reuse an existing empty directory or create a missing one for stable Drive-style folders such as `~/Hashtree`.

### Fixed

- Fixed `htree mount` path handling by rejecting filesystem-like targets that previously triggered ambiguous local publish behavior, and by refusing explicit non-empty mount directories that would otherwise hide user files behind the FUSE mount.

## 0.2.24 - 2026-04-04

Changes since the `0.2.23` crates.io release.

### Improved

- Improved `htree mount` path handling so missing absolute mount paths are created automatically and long-lived mounts warn when they are placed under temporary directories.

### Fixed

- Fixed mounted FUSE filesystems on macOS so Finder writes succeed more reliably and `statfs` now reports usable disk and inode availability instead of a zero-capacity filesystem.
- Fixed `hashtree-cli` test coverage for non-`fuse` builds by keeping fuse-only test imports gated behind the `fuse` feature.

## 0.2.23 - 2026-04-03

Changes since the `0.2.22` crates.io release.

### Improved

- Improved mount lifecycle behavior so `htree mount` waits for a live FUSE session before advertising, republishing mounted roots preserves sibling entries, and remounting stale targets recovers more predictably.
- Improved Linux release engineering by building FUSE-enabled artifacts and smoke coverage inside privileged Docker, which matches the shipped Linux release environment more closely.

### Fixed

- Fixed nested Git ref export in `git-remote-htree`, so published `.git` trees materialize branch paths correctly for viewers and dumb-HTTP clones.
- Fixed CI and non-`fuse` test coverage so `hashtree-cli` keeps fuse-only imports gated correctly and relayless mesh integration tests no longer pick up unrelated multicast peers from parallel jobs.

## 0.2.22 - 2026-04-03

Changes since the `0.2.20` crates.io release.

### Improved

- Improved `htree mount` ergonomics: mountpoints can be derived from targets automatically, owner aliases or hex pubkeys are normalized to `npub` targets before resolution, and mounted-root publishing preserves sibling entries while reducing redundant publishes.
- Added `htree mounts` plus an active-mount registry so running mounts can be listed from the CLI in human or JSON form.
- Kept `fuse` optional for crate consumers while still wiring release binaries and CI to build the mount-capable binaries explicitly, and split oversized server and git-remote helper modules into smaller internal components.

### Fixed

- Fixed the FUSE adapter for current `fuser` callback signatures so mkdir, unlink, rmdir, rename, and write operations dispatch cleanly on newer builds.
- Fixed the embedded daemon and non-`p2p` build path so `hashtree-cli` continues to work with `--no-default-features` after the FUSE-defaults change.
- Added FUSE smoke coverage and active-mount lifecycle cleanup so mounted sessions are registered only while live and stale mount records are pruned automatically.

## 0.2.20 - 2026-04-01

Changes since the `0.2.18` crates.io release.

### Improved

- Extracted the shared mesh routing core into the Rust networking crates and aligned the CLI, simulation, and TypeScript interop paths around the same signaling model.
- Improved `git-remote-htree` and `hashtree-cli` pull-request interoperability, including better published-repo handling and PR listing coverage.
- Improved Iris release defaults on supported platforms so Bluetooth-enabled builds and app release packaging behave more consistently.

### Fixed

- Fixed Iris Android release networking and Zapstore publishing regressions that were blocking the release pipeline.
- Fixed several mobile Bluetooth startup and interoperability issues across the Rust networking stack.

## 0.2.18 - 2026-03-31

Changes since the `0.2.17` crates.io release.

### Improved

- Unified the shared mesh signaling path across `hashtree-cli`, `hashtree-sim`, and the TypeScript interop tests, so production and simulation now exercise the same router-layer protocol and hedged retrieval scheduling.
- Renamed the reusable networking crate from `hashtree-webrtc` to `hashtree-network`, which better matches its scope: mesh routing, signaling, peer links, and store composition rather than just transport bindings.

### Fixed

- Fixed Rust ignored integration and doctest coverage so the full workspace, including network-gated tests and cross-language WebRTC checks, passes from the merged `master` tip.

## 0.2.17 - 2026-03-31

Changes since the `0.2.16` crates.io release.

### Fixed

- Fixed GitHub CI by refreshing the shared TypeScript lockfile and removing lint failures that had started failing the `ts` workflow.
- Fixed GitHub Iris desktop release builds by removing the cross-app `apps/iris` import on `iris-files` source-only TypeScript config and assets.
- Fixed flaky Rust workspace failures in multicast root queries and embedded daemon integration tests, so the GitHub Rust and release workflows complete reliably.

## 0.2.16 - 2026-03-31

Changes since the `0.2.15` crates.io release.

### Improved

- Unified slash-containing mutable tree paths across the Rust server, service-worker-facing helpers, and release publishing flow, so repo releases and encoded `htree` paths resolve consistently.
- Improved embedded daemon and background Nostr service behavior with cleaner shutdown, better default Blossom fallback handling, and broader profile-index/social-graph coverage.
- Improved the native Iris shell transport stack with more robust Bluetooth session handling, origin-isolated `htree` child webviews, and tighter user-facing deep-link/PWA path handling.

### Fixed

- Fixed release publishing defaults to use repo-scoped `releases/<repo>` trees and removed the obsolete standalone `hashtree-nostr-bridge` crate by merging its crawler into `hashtree-nostr`.
- Fixed Rust test hangs caused by embedded daemon background services outliving Tokio runtime shutdown.

## 0.2.15 - 2026-03-31

Changes since the `0.2.14` crates.io release.

### Added

- Added signed tree snapshot permalinks in the Nostr stack, giving published trees a stable signed snapshot path for linking and sharing.
- Added the `cashu-service` crate to carry the shared Cashu helper and wallet primitives used by `htree` and `htree-cashu`.
- Added dumb-HTTP Git metadata export in `git-remote-htree`, so static HTTP gateways can serve cloneable taps and repositories from published `.git` trees.

### Improved

- Bulk-built Nostr event indexes and improved steady-state ingest behavior in `hashtree-nostr`, reducing indexing overhead on larger relay or mirror workloads.
- Expanded social-graph tooling in `htree` with profile-index rebuild support and author allowlist URL input for indexing jobs.
- Restored Bluetooth/Nostr publish receipts, capped receipt logs, and cleaned up BLE polling and routing behavior in the daemon transport path.
- Automated Homebrew tap publication from the Rust release flow, with an explicit opt-in path for chaining crates.io publishing from the same release command.

## 0.2.14 - 2026-03-28

Changes since the `0.2.13` crates.io release.

### Added

- Added the new `hashtree-merge` crate with deterministic path-based merge primitives and wired it into the publish chain.
- Added `htree repos` and improved repo listing/source-link handling for hashtree-first repositories.
- Added offline LAN multicast signaling, Bluetooth mesh transport work, Wi-Fi Aware nearby-bus scaffolding, and transport usage tracking in the daemon/CLI stack.

### Improved

- Switched the local Nostr relay to B-tree-backed event indexes, split trusted public and ambient indexes, and added faster planning for `ids`, authors, kinds, replaceables, parameterized replaceables, and tag queries.
- Improved Nostr relay correctness coverage for filter matching, limits, `COUNT`, replaceables, `since`/`until`, and search behavior.
- Batched Nostr ingest writes through buffered store flushes and LMDB batch commits, substantially reducing publish-side write amplification.
- Improved cold root resolution, daemon root handling, filesystem blob sharding, and LMDB quota/default-store behavior.
- Tightened git publish ordering so roots are published after blob upload and improved progress/reporting in `git-remote-htree`.

### Fixed

- Fixed publish blockers in the Rust crate graph and aligned published repository/homepage links with the current hashtree remote.
- Fixed several Bluetooth/native relay startup and htree loading issues in the Rust networking stack.
- Fixed Nostr manifest/index handling around by-id compatibility transitions and relay query edge cases found by the new tests.
