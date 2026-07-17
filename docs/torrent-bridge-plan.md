# Torrent Bridge Plan

Date: April 6, 2026
Scope: Read-through BitTorrent bridge for serving hashtree files and directories from torrent seeders without requiring a full torrent download or permanent local retention

## Goal

Add a torrent-backed content source that lets the daemon:

- expose directory and file views backed by `.torrent` metadata
- fetch only the pieces needed for a requested file or byte range
- verify downloaded data before serving it
- keep disk usage bounded and optionally discard fetched data immediately after use
- materialize hot files or whole trees into real hashtree content only when useful

This is aimed at archive-scale mirrors where the source already exists as torrents. A plausible use case is a local bridge for Minerva-style retro game archives, with emulator binaries or frontends published separately via normal hashtree flows.

## Key Constraint Found During Initial Research

The Minerva torrents sampled on April 6, 2026 appear to be BitTorrent v1 only, not v2 or hybrid:

- no `meta version`
- no `file tree`
- no per-file SHA-256 merkle roots
- standard v1 `pieces` with piece sizes such as `32768` and `16777216`

This matters:

- v1 torrents are still usable for lazy streaming from seeders
- v1 torrents do not let us derive stable hashtree file roots from metadata alone
- a true immutable hashtree mirror therefore requires at least one content hashing pass

For future v2 or hybrid torrents, the TypeScript BEP52 utilities in [`bep52.ts`](/Users/sirius/src/hashtree/ts/packages/hashtree/src/bep52.ts) are already relevant.

## Recommended Delivery Model

Treat this as two layers, not one:

1. Virtual torrent-backed serving
2. Optional materialization into real hashtree content

The first layer provides immediate value and does not require us to pre-hash entire archives. The second layer can progressively convert the useful subset into canonical hashtree trees.

## Non-Goals For The First Version

- Do not try to mirror an entire archive into permanent local storage by default.
- Do not require complete torrent download before first serve.
- Do not invent fake immutable hashtree CIDs for content that has not actually been hashed into hashtree yet.
- Do not make public hashtree publication of copyrighted ROM collections the default behavior.
- Do not start with magnets only; `.torrent` import is the simpler and more deterministic first step.

## Current Seams In The Codebase

The main Rust daemon already has the right miss-and-fill structure:

- [`fetch_and_cache_blob`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/server/handlers/content.rs#L4)
  - fetches missing blobs from upstream sources and caches them locally
- [`ensure_blob_available`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/server/handlers/content.rs#L134)
  - coalesces concurrent misses for the same hash
- [`fetch_missing_chunk`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/server/handlers/content.rs#L174)
  - loops missing-chunk recovery back into the fetch path
- [`read_file_range_cid_with_fetch`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/server/handlers/content.rs#L314)
  - already supports range-driven recovery
- [`stream_file_range_cid_with_fetch`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/server/handlers/content.rs#L340)
  - already streams large responses incrementally
- eviction tests in [`eviction.rs`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/tests/eviction.rs)
  - confirm the storage layer already supports bounded retention semantics

This suggests a clean direction:

- implement a torrent-backed opaque `BlobRoute`
- add it alongside the FIPS composite route and Blossom terminal routes
- keep the reader logic largely unchanged

## Architectural Decision

Do not make the first implementation pretend torrents are ordinary hashtree stores.

Instead, add a dedicated virtual torrent namespace and only materialize real hashtree content when requested or explicitly promoted.

Reason:

- directory structure is available from torrent metadata immediately
- file bytes are available only after piece fetch and verification
- for v1 torrents, file-level hashtree hashes are unknown until we hash content ourselves
- forcing everything into the current immutable CID model too early will create awkward placeholder semantics

## Proposed User Model

Suggested commands:

```bash
htree torrent import path/to/archive.torrent --name minerva-mame-roms
htree torrent ls minerva-mame-roms
htree torrent serve
htree torrent materialize minerva-mame-roms "MAME - ROMs (non-merged)/pacman.zip"
```

Suggested URL model:

- virtual route for immediate access:
  - `/torrent/<catalog>/...`
- optional later mutable publication after materialization:
  - regular `htree://npub/tree/path`

This keeps the first phase honest:

- torrent-backed content is clearly virtual and local
- materialized content becomes normal hashtree content

## Internal Components

### 1. Torrent Catalog Index

Persist imported torrent metadata in a local index:

- torrent name
- infohash
- announce URLs
- piece length
- per-file path
- per-file byte length
- per-file offset within the torrent
- piece span covering each file

This index powers directory listing and file lookup without any file data present locally.

### 2. Torrent Piece Fetcher

Add a read-only leecher focused on demand fetching:

- request only pieces needed for the current file or range
- verify piece hashes before use
- coalesce concurrent requests for the same piece
- support tracker and DHT peer discovery later, but start with `.torrent` + tracker flow

The fetcher should expose:

- `read_range(file_id, start, end) -> bytes`
- `read_file(file_id) -> stream`

### 3. Piece Cache

Use a bounded local cache with explicit retention policy:

- cap total bytes
- mark everything unpinned by default
- allow immediate discard mode for low-disk environments
- optionally keep a small hot cache for piece reuse

Desired behavior:

- if disk space runs out, old pieces are evicted
- subsequent reads simply re-fetch from seeders

### 4. Materializer

When a file is requested often, or on explicit command:

- stream file bytes from the torrent fetcher
- chunk into normal hashtree blobs
- build the real hashtree file node
- store it locally
- optionally attach it into a mutable local tree

This separates "can serve now" from "is canonical hashtree content now".

## Implementation Phases

### Phase 1: Metadata Import And Virtual Listing

Add:

- `htree torrent import <file.torrent> --name <catalog>`
- local metadata database for imported torrents
- directory listing endpoint or CLI output from imported file lists

Acceptance:

- imported torrent contents can be browsed without downloading file data
- nested directories and file sizes are correct

### Phase 2: On-Demand File And Range Reads

Add:

- torrent piece fetcher
- piece verification
- virtual HTTP serving for `/torrent/<catalog>/<path>`
- byte-range support

Acceptance:

- large files can be read via HTTP range requests
- only needed pieces are fetched
- no full-torrent download is required

### Phase 3: Bounded Retention

Add:

- explicit cache quota for torrent pieces
- unpinned-by-default semantics
- cache eviction metrics and logs

Acceptance:

- low cache settings still allow repeated reads with re-fetch
- daemon does not grow unbounded on archive browsing

### Phase 4: Materialize Selected Files

Add:

- `htree torrent materialize <catalog> <path>`
- optional automatic materialization for hot files

Acceptance:

- materialized files become normal hashtree content
- subsequent reads can prefer local hashtree blobs over torrent fetches

### Phase 5: Mutable Mirror Trees

After enough files are materialized:

- build local mutable trees that point only to real hashtree content
- optionally publish those trees to normal hashtree remotes

Acceptance:

- published trees contain only real hashtree content
- no virtual torrent placeholders leak into canonical published trees

## Rust Integration Direction

Recommended first placement is under `hashtree-cli`, not `hashtree-core`.

Likely files:

- `rust/crates/hashtree-cli/src/torrent/mod.rs`
- `rust/crates/hashtree-cli/src/torrent/index.rs`
- `rust/crates/hashtree-cli/src/torrent/fetch.rs`
- `rust/crates/hashtree-cli/src/torrent/cache.rs`
- `rust/crates/hashtree-cli/src/torrent/http.rs`

Likely call sites:

- server routing in [`server/mod.rs`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/server/mod.rs)
- upstream fetch generalization in [`server/handlers/content.rs`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/server/handlers/content.rs)
- CLI plumbing in:
  - [`app/args.rs`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/app/args.rs)
  - [`app/mod.rs`](/Users/sirius/src/hashtree/rust/crates/hashtree-cli/src/app/mod.rs)
  - likely a new `app/torrent.rs`

## v1 Versus v2 Strategy

### v1 torrents

Use them as streaming sources, not as direct immutable roots.

Properties:

- piece verification is SHA-1 per piece
- file-level hashtree structure is unknown until we hash content ourselves
- good enough for virtual serving and later materialization

### v2 or hybrid torrents

Treat these as much closer to hashtree-native imports.

Properties:

- per-file merkle structure exists
- SHA-256 block trees align conceptually with hashtree
- potential for metadata-only import of file integrity structure

Even then, hashtree and BEP52 do not have identical node layout semantics, so the safest rule is still:

- use torrent metadata to prove/fetch file bytes
- build actual hashtree trees explicitly

## Emulator And Frontend Strategy

Recommended split:

- publish emulator binaries, web frontends, and metadata through normal hashtree flows
- keep ROM payload access behind the local torrent bridge

This avoids coupling emulator distribution to archive legality questions and keeps the public hashtree layer focused on software we can more comfortably publish.

## Dependency Strategy

Prefer a small, auditable implementation over a large opaque dependency.

Two acceptable paths:

1. Minimal in-process read-only torrent engine
2. Thin experimental adapter to an existing local torrent client for validation only

Production preference:

- avoid a hard dependency on a heavyweight external daemon if a scoped internal implementation is practical
- keep the first version leecher-only
- no seeding, upload management, or torrent UX complexity unless later needed

## Testing Plan

### Deterministic local tests

Add fixtures and tests for:

- torrent metadata import
- file path to offset mapping
- piece boundary math
- range reads spanning multiple pieces
- partial reads at start and end of file
- cache eviction under tiny quotas
- repeated read after eviction triggers re-fetch
- concurrent readers coalesce on the same piece

### End-to-end tests

Use a small local torrent swarm fixture:

- one seed process with known files
- daemon imports the `.torrent`
- HTTP range requests verify exact returned bytes
- ensure the leecher never downloads the entire torrent unless requested

### Materialization tests

- materialized file hash matches direct hashtree import of the same bytes
- repeated reads prefer local materialized content when present

## Open Questions

1. Should the first HTTP surface be a dedicated `/torrent/...` route or a mount-like path under existing virtual host machinery?
2. Should materialization happen only on explicit command, or also opportunistically for small files?
3. Do we want a "zero retention" mode that spills only to memory and never writes piece cache to disk?
4. Is tracker-only import enough for the first version, or do we need magnet/DHT support immediately?
5. Do we want a future "pay seeders" path, or is this strictly archival/public swarm access?

## Recommended First Slice

Build the smallest honest version:

1. import `.torrent`
2. browse file list
3. serve one file path with HTTP range support
4. fetch only required pieces
5. verify pieces
6. evict aggressively under quota

If that works well, then add explicit materialization into real hashtree content.

## External References Consulted

- Minerva FAQ:
  - <https://minerva-archive.org/faq/>
- Minerva torrent index:
  - <https://cdn.minerva-archive.org/torrents/>
- Sample torrents checked on April 6, 2026:
  - <https://cdn.minerva-archive.org/torrents/Minerva_Myrient%20-%20No-Intro%20-%20Analogue%20-%20Analogue%20Pocket.torrent>
  - <https://cdn.minerva-archive.org/torrents/Minerva_Myrient%20-%20Internet%20Archive%20-%20retro_game_champion.torrent>
  - <https://cdn.minerva-archive.org/torrents/Minerva_Myrient%20-%20MAME%20-%20ROMs%20(non-merged).torrent>
