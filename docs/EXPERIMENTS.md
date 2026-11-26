# Experiments

This file records performance and behavior experiments without identifying data. Do not store pubkeys, secrets, IP addresses, private hostnames, exact private repo names, or raw content hashes here unless explicitly requested.

## 2026-11-26 - LMDB, `PoolStore`, HTTP blob serving, and Nostr relay hot-path optimization

We profiled and benchmarked a live multi-terabyte `htree` daemon on an 8-core / 16-thread Linux x86_64 host (60 GiB RAM, NVMe catalog/member LMDB tier plus mirrored HDD external-blob and read-only fallback tiers, ~128.9 million blobs / ~5.78 TB logical bytes) across three matched release stages:
- **Stage A (Baseline)**: current upstream release binary under existing configuration.
- **Stage B (Codebase-only)**: optimized release binary under the identical existing configuration.
- **Stage C (Codebase + host/daemon tuning)**: optimized release binary with `HTREE_LMDB_NO_READ_AHEAD=1`, `MALLOC_ARENA_MAX=4`, expanded zero-copy blob caches (`HTREE_BLOB_BODY_CACHE_BYTES=1 GiB`, `HTREE_BLOB_SIZE_CACHE_ENTRIES=262144`), origin `fetch_from_fips_peers = false` (keeping FIPS peer serving enabled), removal of a self-referencing Nostr relay entry, `vm.swappiness=1`, `vm.vfs_cache_pressure=50`, and HDD block readahead lowered from 8 MiB to 128 KiB.

Codebase changes evaluated:
- **`hashtree-lmdb` eviction & reads**: replaced full-table `Vec<Vec<u8>>` allocation in `LmdbBlobStore::evict_to_target` with O(1) all-pinned short-circuiting and streaming cursor batches of stack `[u8; 40]` keys; skipped redundant `metadata` B-tree lookups in `get_sync` for inline and packed blobs; dropped LMDB `rtxn` before blocking external-file reads in `get_sync` and `get_range_sync`; switched Unix external blob/pack reads to `FileExt::read_exact_at` (`pread`) with single `metadata().len()` verification.
- **`PoolStore` locking, catalog writes, and O(1) stats**: cached raw manifest bytes + decoded `PoolManifest` in `RuntimeMembers` so unchanged `refresh_members()` calls take only a shared read lock (zero deserialization, zero SHA-256, zero write-lock contention) while remaining immediately reactive to cross-process manifest updates; added `get_member_and_gate`, eliminated `adaptive` mutex acquisition on single-member reads, batched location checks / write-member selection / candidate existence checks in `put_many`, diffed `by_member` updates and gated `temperature_state` cleanup in `set_location_txn` (reducing `Pending -> Stored` finalization from 6 B-tree ops to 1), and added O(1) `PoolStore::members_summary()` and `PoolStore::stats_bounded(50_000)`.
- **HTTP server, cache, and batch I/O**: switched `BlobCache` body storage and `inflight_blob_reads` to zero-copy `bytes::Bytes`; wrapped per-request cloned `AppState` collections/credentials in `Arc`; collapsed plain `GET /<sha256>` (no `Range`, not `HEAD`) from two blocking dispatches (`blob_size` + `get_blob`) to one; removed unused per-request client-IP string allocation; offloaded `/api/stats` to `run_blob_metadata_read` with bounded pool stats; parallelized `POST /blob/batch` reads with bounded concurrency (16, preserving request order and truncation); gated Blossom batch replication payload cloning behind replication eligibility; fixed `PoolStoreWithFallbacks::get_range_sync` not to re-query primary on fallback misses; raised `BlobIoLimits::metadata_read_limit` ceiling from 4 to 8.
- **Nostr relay & social graph**: cached a shared `OnceLock<Arc<rustls::ClientConfig>>` WebPKI TLS connector for upstream WebSocket relays; parsed incoming upstream `NostrRelayMessage` once per frame; replaced per-query Tokio runtime creation in `HistoricalNostrIndex` with `Handle::current().block_on`; guarded per-query `/proc/self/status` snapshots behind `DEBUG` level; rate-limited hot-path `malloc_trim(0)` to 30 s intervals; wrapped `SocialGraphStore`'s `DistanceCache` in `Arc`.

| Metric (live multi-TB workload) | Stage A (Baseline) | Stage B (Code only) | Stage C (Code + config/OS) | Net Change |
| --- | ---: | ---: | ---: | ---: |
| `GET /health` p50 (20x) | 0.30 ms | 0.15 ms | 0.15 ms | 2.0x faster |
| `GET /api/status` p50 (10x) | 0.41 ms | 0.20 ms | 0.20 ms | 2.0x faster |
| `GET /api/stats` (128.9M blobs) | >5,000 ms timeout | 0.19 ms | 0.19 ms | >25,000x faster |
| `htree storage pool status` | >45 s hang (20.1 GiB faulted) | 15 ms | ~15 ms | >3,000x faster |
| `GET /<missing>.bin` (cold) | 5,503 ms (HTTP 503) | 5,546 ms (HTTP 503) | 18.85 ms p50 (HTTP 404) | 292x faster; 0 5xx |
| Live HTTP `5xx` (60 s window) | ~15–20+/min | 18 (incl. 3 probe) | 0 (1,013 total reqs / 81 s) | Eliminated |
| `HEAD /<cold-random>.bin` (20x) | 48.6 ms p50 / 130.6 ms max | 59.8 ms p50 / 140.2 ms max | 36.1 ms p50 / 79.8 ms max | 1.35x p50 / 1.6x max |
| `POST /upload/check` (1,000 cold) | 248–263 ms | 331 ms | 90.1 ms (warm 2.6 ms) | 2.8x faster |
| `POST /blob/batch` (50 cold random) | 2,509.8 ms | 847.4 ms | 619.2 ms (warm 0.7 ms) | 4.1x faster |
| NVMe read throughput (`iostat`) | 110.6 MB/s (62.3 KB/req) | 45.7 MB/s (48.6 KB/req) | 2.9 MB/s (10.3 KB/req) | 38x lower read BW |
| Process disk reads (`/proc/io`) | 37.6 MB/s | 47.1 MB/s | 2.2 MB/s | 17x fewer bytes read |
| Process disk writes (`/proc/io`) | 12.4 MB/s | 12.3 MB/s | 2.1 MB/s | 5.9x fewer bytes written |
| HDD RAID1 `%util` / `w_await` | 96.5–97.8% / 23 ms | 99.8% / 22.8 ms | 70.9% / 11.7 ms | -27% util, 2x lower await |
| Process `perf stat` CPU (5 s) | — | 382.3 ms (0.08 CPU) | 228.5 ms (0.05 CPU) | -40% vs Stage B |
| Process `VmRSS` / `RssAnon` / swap | 11.8 GiB / 154 MiB (2.45 GiB + 532 MiB swap over 7 d) | 2.25 GiB / 104 MiB / 0 | 0.92–1.19 GiB / 51–56 MiB / 0 | Bounded RSS, 0 swap |

Note: live traffic varies between 5-second sampling windows, so disk and CPU counters reflect representative steady-state observations alongside the deterministic synthetic HTTP probes. All 124 unit tests and 46 integration tests (excluding subprocess helpers) in `hashtree-lmdb`, plus targeted `hashtree-cli` unit/server/relay runs (71 and 19 tests across touched modules) and `storage_quota_report` (including new regression tests for streaming pinned eviction, inline/loose/packed reads and corruption detection, cross-process pool manifest cache reload, bounded pool stats and member summaries, concurrent ordered batch downloads, single-dispatch plain GET caching, shared TLS connector reuse, and throttled allocation trimming) passed.

## 2026-11-26 - `git-remote-htree` clone, fetch, push, and `ls-remote` optimization

We measured end-to-end Git workflows (`git push`, `git clone`, `git fetch`, `git ls-remote`) on macOS ARM64 with matched release `git-remote-htree` binaries against the same local `htree start` LMDB Blossom daemon behind an instrumented async HTTP/1.1 proxy (20 ms RTT, plus a plain-Blossom 404 mode blocking `/blob/batch`, `/upload/check`, and `/upload/batch*`) and a local RFC 6455 Nostr relay. Both binaries ran in alternating order (`baseline`, `candidate`, `candidate`, `baseline`) across two seeded repositories:
- **Loose-object repo** (below the 256-object pack checkpoint threshold): 120 files across 12 directories, 21 commits, 235 objects, 25 branches/tags (26 `ls-remote` refs including `HEAD`).
- **Packed-checkpoint repo**: 480 files across 40 directories plus 10 binary assets (~8 MiB total), 36 commits, 784 objects.

Changes evaluated:
- **Eliminated hidden progress-reporter stalls (`PeriodicReporter`)**: fixed unconditional 5 s `sleep()` awaits in loose-object discovery and download and a `Notify::notify_waiters()` lost-wakeup race in pack installation that stalled incremental packed fetches by 10 s (15 s total per fetch).
- **Single process-wide Tokio runtime and shared HTTP/Blossom clients**: replaced per-call Tokio runtime creation (with 500 ms shutdown waits) with a lazy `OnceLock<Runtime>`, reused a shared daemon `reqwest::Client`, and reused `NostrClient`'s `BlossomClient` when building `CachedStore`.
- **Batch reads and LMDB commits**: added `CachedStore::prefetch_missing` (`POST /blob/batch` with plain-Blossom 404 auto-detection and batched `put_many` LMDB writes), level-by-level batched ref fetching in `fetch_refs_from_hashtree`, single `.git/objects` directory listing, pack/loose object batch prefetching, higher tree-walk/download concurrency (32), and a single `spawn_blocking` loose-object writer with cached 2-hex prefix directories.
- **Push, multi-ref, and relay lookup improvements**: aligned open Nostr query windows to 3 s per attempt (still open past EOSE across 2 attempts), cached clean `RootNotObserved` from `list for-push` within the same helper process, batched multi-ref pushes (`git push --tags` / `--all`) into a single tree build/upload/publish (also fixing ancestor-branch validation failures), computed local PR merge candidates before querying Nostr relays, used 1 `POST /upload/check` for 32-hash server-coverage sampling with plain-Blossom fallback, remembered `/upload/batch*` 404/unsupported status to avoid wasted batch bodies on plain Blossom servers, and capped pending upload memory at 16 MiB.
- **Local Git object & index building**: pipelined single-process `git cat-file --batch-check` / `--batch`, zero-copy zlib header inspection for blob sizes, parallel multi-core object compression (`write_raw_objects_batch`), `BufferedStore::new_optimistic`, and O(log N + k) `.git/index` subtree reuse from the previous root.

| Flow (20 ms RTT) | Baseline median (2 runs) | Candidate median (2 runs) | Speedup | HTTP reqs, before → after | CPU median ms, before → after | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| `fetch` delta (packed repo) | 15,311 ms (15,315 / 15,308) | 302 ms (316 / 289) | 50.6x | 22 → 11 | 49.0 → 41.6 | Fixes 5 s + 10 s stalls |
| `fetch` delta (loose repo) | 5,867 ms (5,860 / 5,874) | 371 ms (450 / 293) | 15.8x | 45 → 11 | 59.2 → 55.0 | Fixes 5 s stall + batch fetch |
| `clone` packed (plain Blossom) | 5,937 ms (5,932 / 5,941) | 580 ms (616 / 544) | 10.2x | 91 → 93 | 132.2 → 124.8 | Plain-Blossom fallback |
| `clone` loose (full Blossom) | 7,402 ms (7,556 / 7,249) | 789 ms (1,145 / 432) | 9.4x | 437 → 16 (-96%) | 265.6 → 120.4 (-55%) | Batch read + batched LMDB |
| `clone` packed (full Blossom) | 5,941 ms (5,976 / 5,906) | 643 ms (809 / 477) | 9.2x | 91 → 14 (-85%) | 132.2 → 123.0 | Pack + loose batch prefetch |
| `push --tags` (12 tags) | 4,282 ms (4,309 / 4,256) | 496 ms (557 / 435) | 8.6x | 427 → 10 (-98%) | 588.2 → 150.2 (-74%) | Relay conns 14 → 1; 1 root build |
| `clone` loose (plain Blossom) | 7,282 ms (7,377 / 7,186) | 988 ms (1,293 / 682) | 7.4x | 437 → 413 | 269.0 → 131.3 (-51%) | Auto-skips unsupported batch |
| `push` initial (loose, 235 obj) | 33,100 ms (33,137 / 33,062) | 6,705 ms (6,808 / 6,601) | 4.9x | 8 → 6 | 102.1 → 83.0 | Relay conns 7 → 3 |
| `push` initial (packed, 784 obj) | 33,691 ms (33,673 / 33,708) | 7,233 ms (7,381 / 7,085) | 4.7x | 10 → 8 | 629.6 → 554.2 | Relay conns 7 → 3 |
| `push` initial (plain Blossom) | 36,118 ms (36,112 / 36,124) | 9,600 ms (9,588 / 9,612) | 3.8x | 570 → 562 | 132.8 → 107.5 | Skips 404 batch bodies (1.11 MB → 48.8 KB, -95.6%) |
| `ls-remote` (26 refs) | 753 ms (759 / 746) | 241 ms (296 / 186) | 3.1x | 32 → 6 (-81%) | 27.0 → 23.4 | Level-batched ref BFS |
| `push` delta (loose, 1 file) | 1,111 ms (1,123 / 1,099) | 515 ms (615 / 415) | 2.2x | 67 → 10 (-85%) | 120.4 → 126.0 | Relay conns 3 → 1 |
| `push` delta (packed, 1 file) | 620 ms (590 / 649) | 409 ms (424 / 393) | 1.5x | 42 → 10 (-76%) | 118.9 → 89.4 (-25%) | Relay conns 3 → 1; index reuse |

Every cloned and fetched repository passed `git fsck --full --strict` with identical `HEAD` and `HEAD^{tree}` hashes. All 278 unit and integration tests in `git-remote-htree` (including open-subscription late-event lookup, PR merge detection, P2P roundtrip, diff push, missing-chunk repair, visibility modes, and multi-ref ancestor-branch push) passed.

## 2026-11-26 - TypeScript peer retry bounds and corrupt-peer cooldown on 10,000-node layouts

`TcpBlobTransport.get` previously ignored caller abort signals and treated downstream `AggregateError` results as retryable single-hop failures, amplifying multi-hop errors across hops (`2^htl`) and repeatedly querying peers that returned corrupt bytes.

The updated transport honors an optional caller `AbortSignal` without allocating composite `AbortSignal.any` wrappers, restricts single-session retries to direct link errors (skipping both `PeerBlobHashMismatch` and downstream `AggregateError`), and defers directly observed corrupt peers for 30 seconds (bounded to 256 peers) whenever healthy alternatives exist while preserving uncertainty on misses and still allowing single-peer lookups.

We developed the change on two layouts (`3217,3253`) and fixed it before testing twelve previously unused 10,000-node layouts (`3701,3733,3769,3803,3847,3881,3919,3967,4003,4049,4093,4133`) in four alternating serial pairs of three layouts on macOS ARM64. Each version ran 4,608 reads (2,304 direct, 2,304 five-hop) with the same graph, placements, faults, and 700 ms deadlines.

| Workload | Verified / reachable (both) | Protocol MB, before → after | Attempts, before → after | Query CPU s, before → after | Successful p50 / p95 ms (both) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Direct, all | 1,069 / 1,069 | 4.07 → 4.06 | 7,406 → 7,405 | 0.51 → 0.48 | 32 / 48 |
| Five-hop, all | 373 / 373 | 3,219.86 → 381.43 | 12,611,458 → 4,675,034 | 216.48 → 84.93 | 146 / 406 |
| Five-hop, steady | 177 / 177 | 73.36 → 73.36 | 1,595,836 → 1,595,836 | 28.49 → 30.58 | 146 / 178 |
| Five-hop, churn | 113 / 113 | 259.74 → 106.67 | 5,822,262 → 2,388,375 | 101.09 → 41.44 | 150 / 690 |
| Five-hop, adversarial | 83 / 83 | 2,886.76 → 201.40 | 5,193,360 → 690,823 | 86.90 → 12.91 | 146 / 168 |

Both versions verified identical reachable reads and distinct queries (635/635 direct, 310/310 five-hop) with zero unexpected misses and identical latency percentiles. Adversarial deadlines fell from 685 to 0 (all unreachable adversarial reads completed cleanly as incomplete instead of timing out), corrupt replies rejected fell from 650,184 to 41,666 (-93.6%), and five-hop aggregate traffic dropped from 62.25 to 17.42 Mbps. Across the four alternating serial pairs, total process CPU fell from 219.82 to 88.25 seconds (-59.9%) and max resident memory stayed bounded (637.3 → 671.3 MiB).

```sh
pnpm --dir ts --filter @hashtree/fips-transport build
seeds=3701,3733,3769,3803,3847,3881,3919,3967,4003,4049,4093,4133
node ts/packages/hashtree-fips-transport/scripts/mesh-scale/run.mjs baseline "$seeds" <baseline-commit>
node ts/packages/hashtree-fips-transport/scripts/mesh-scale/run.mjs candidate "$seeds"
```

## 2026-11-26 - End-to-end CLI directory add, Blossom batch push/fetch, and fault recovery

We measured four user-facing Rust CLI/Blossom flows on macOS ARM64 with matched release binaries (`htree`) against a real local `htree start` LMDB Blossom daemon behind an instrumented async HTTP/1.1 proxy injecting 20 ms RTT and deterministic faults. Workloads: a seeded 500-file nested encrypted directory (~6.9 MiB, 551 blobs), a 10-file incremental update to that directory, a 32 MiB multi-chunk encrypted file, and an 8 MiB encrypted file for fault injection. Both builds ran in alternating order (`baseline`, `candidate`, `candidate`, `baseline`) against the same baseline daemon binary, plus a plain-Blossom fallback run blocking `/upload/check`, `/upload/batch*`, and `/blob/batch` with HTTP 404.

Changes evaluated:
- Coalesced directory add writes (`upload_dir_with` and `upload_dir_cached_with_progress`) via `BufferedStore` (16 MiB flush threshold) instead of one durable LMDB commit per small file/node.
- `htree push` and auto-push (`blossom_push.rs`): pre-filter present blobs via `POST /upload/check`, stream bounded 8 MiB batches via `POST /upload/batch-binary` (avoiding wasted batch bodies when `/upload/check` returns 404), and eliminate duplicate local store reads.
- `Fetcher` (`fetch.rs`) and `BlossomClient` (`hashtree-blossom`): default fetch concurrency 1 → 16, `POST /blob/batch` for small/medium blobs with hedged multi-server `GET` (150 ms hedge delay) and transient retry (up to 3 attempts on 408/429/5xx/transport errors), in-memory node decoding skipping redundant leaf reads/decrypts, batched LMDB cache writes, explicit error return on missing chunks, bounded LRU cache on `BlossomStore` (64 MiB default), and bounded ordered chunk read-ahead in `HashTree::get_stream` (up to 8 chunks / 8 MiB).

| Flow (20 ms RTT, full Blossom server) | Baseline (2 runs) | Candidate (2 runs) | Requests, before → after | Bytes / correctness |
| --- | ---: | ---: | ---: | --- |
| `htree add <dir>` (durable LMDB, 500 files) | 14,424 / 13,109 ms | 1,923 (cold) / 196 ms | local | identical root CID |
| `htree push` cold (500-file encrypted dir) | 13,719 / 10,946 ms | 532 / 216 ms | 561 → 2 | 6.92 → 6.98 MB (framed) |
| `htree push` incremental (10/500 files changed) | 1,704 / 1,688 ms | 174 / 166 ms | 559 → 2 | 6.36 MB → 103 KB (-98.4%) |
| `htree get <dir>` (500 files) | 33,749 / 38,060 ms | 1,800 / 2,764 ms | 551 → 47 | 500/500 SHA-256 verified |
| `htree get <32 MiB file>` | 2,499 / 3,344 ms | 1,290 / 799 ms | 17 → 17 | SHA-256 verified |
| Fault: 3 s stalled primary + 15 ms secondary (8 MiB) | 15,356 / 15,752 ms | 892 / 658 ms | 5 stalled / 0 healthy → 5 / 5 | SHA-256 verified |
| Fault: transient 503 during `htree load` (8 MiB) | exit 0, incomplete offline | exit 0, complete offline | 1 injected 503 | retries & verifies offline |
| Fault: permanent 404 child during `htree load` | exit 0 (silent miss) | exit 1 (explicit error) | missing child | preserves valid chunks |

On a plain Blossom server returning 404 for `/upload/check`, `/upload/batch*`, and `/blob/batch` (20 ms RTT):
- Cold push fell from 15,805 ms (561 requests, 6.92 MB) to 7,763 ms (562 requests, 6.96 MB, zero wasted batch bodies).
- Incremental push fell from 2,885 ms to 1,724 ms (560 requests).
- Directory get (500 files) fell from 41,302 ms to 1,572 ms (552 requests, 500/500 SHA-256 verified).
- 32 MiB file get fell from 3,508 ms to 485 ms (17 requests, SHA-256 verified).

## 2026-11-18 - TypeScript peer retries on fresh 10,000-node layouts

Transport 0.5.2 starts the same peers in parallel as 0.5.1. Each stream
reports completion once. The first verified reply wins. A retry contacts
peers whose streams failed, skipping peers that already answered missing
or returned bytes with the wrong hash. A corrupt reply leaves the result
uncertain until a peer supplies valid data. A new lookup can try every peer.

We developed the change on two layouts and fixed it before testing twelve
previously unused layouts. Each version ran 4,608 reads with the same graph,
placements, faults and 700 ms deadlines. Counts use the production TypeScript
lookup over simulated links. CPU includes the Node.js scheduler and
verification. Protocol bytes count requests and replies before carrier framing.

| Workload | Verified / reachable (both) | Protocol MB, 0.5.1 → 0.5.2 | Query CPU s, 0.5.1 → 0.5.2 | Successful p50 / p95 ms (both) |
| --- | ---: | ---: | ---: | ---: |
| Direct, all | 1,084 / 1,084 | 4.82 → 3.98 | 0.39 → 0.35 | 30 / 48 |
| Five-hop, all | 323 / 323 | 5,043.98 → 3,270.72 | 365.92 → 177.87 | 144 / 370 |
| Five-hop, steady | 151 / 151 | 72.91 → 72.91 | 36.08 → 25.17 | 136 / 190 |
| Five-hop, churn | 114 / 114 | 389.98 → 254.91 | 177.53 → 82.87 | 152 / 446 |
| Five-hop, adversarial | 58 / 58 | 4,581.09 → 2,942.90 | 152.30 → 69.82 | 144 / 184 |

Both versions delivered the same number of distinct blobs in every case.
All cache entries passed hash verification. Five-hop attempts fell from
17.85 million to 12.63 million and aggregate traffic from 91.57 to 63.17 Mbps.
Across four serial pairs of three layouts with alternating run order, total
process CPU fell from 368.99 to 180.74 seconds and maximum resident memory
from 724 to 663 MiB. Direct steady reads used 0.169 and 0.182 CPU seconds
respectively across 768 reads. CPU measurements are host-dependent.
On the earlier churn layout where 0.5.1 missed one reachable read, 0.5.2
also delivered seven of eight.

To reproduce from the TypeScript workspace, build the transport and use the
same seeds with both commands. To reproduce the process-level measurements,
run three consecutive seeds per process and alternate which version runs
first for each group. The baseline revision is the 0.5.1 release.

```sh
pnpm --filter @hashtree/fips-transport build
seeds=3217,3253,3299,3329,3371,3407,3449,3491,3541,3583,3617,3659
node packages/hashtree-fips-transport/scripts/mesh-scale/run.mjs baseline "$seeds" 9ee330b87b0e18e738a2f708eaf08e2287917e13
node packages/hashtree-fips-transport/scripts/mesh-scale/run.mjs candidate "$seeds"
```

## 2026-11-18 - TypeScript hash retrieval on 10,000-node layouts

The baseline TypeScript transport and the proposed four-stream policy ran
the same twelve fresh layouts. Each version handled 4,608 reads. This
includes 2,304 direct-peer reads and 2,304 five-hop mesh reads. Both cases
cover steady links, churn with stalled and departing peers, and simulated
false misses and corrupt replies. Both drop results after 700 ms.

The direct-peer case limits each roster to four neighbors and puts the first
replica of each even-numbered query on an adjacent peer. The five-hop case
uses the Rust simulator’s sparse placements and topology. Incoming
requests share forwarding work at equal hop limits. A waiting reader can
take over when the owner is cancelled. TypeScript limits simultaneous
attempts and can continue through the full roster. Rust uses a total
attempt budget for each search. The results compare the TypeScript
versions within each case.

| Workload | Verified / reachable | Distinct verified / reachable | Protocol MB | Aggregate Mbps | Query CPU s | Successful p50 / p95 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Direct, all | 1,075 → 1,075 / 1,075 | 641 → 641 / 641 | 4.84 → 4.77 | 0.69 → 0.73 | 0.37 → 0.57 | 30 / 48 → 30 / 48 |
| Five-hop, all | 474 → 370 / 475 | 362 → 286 / 363 | 4,897.57 → 1,322.17 | 89.70 → 53.66 | 334.51 → 252.26 | 128 / 420 → 196 / 528 |
| Five-hop, steady | 222 → 198 / 222 | 173 → 156 / 173 | 72.46 → 71.92 | 26.78 → 8.83 | 34.16 → 107.30 | 130 / 180 → 208 / 554 |
| Five-hop, churn | 163 → 97 / 164 | 121 → 69 / 122 | 391.73 → 41.77 | 11.53 → 5.07 | 159.35 → 57.21 | 142 / 532 → 301 / 522 |
| Five-hop, adversarial | 89 → 75 / 89 | 68 → 61 / 68 | 4,433.37 → 1,208.48 | 247.42 → 146.32 | 141.00 → 87.75 | 118 / 178 → 152 / 524 |

The five-hop candidate saved 73.0% of traffic and delivered
104 fewer verified reads. Direct reads retained their delivery
count and tail latency. Every five-hop layout delivered fewer reads on the
candidate. The port remains an experiment.

The simulator calls the production TypeScript lookup methods and replaces
TCP streams with deterministic links. It counts encoded requests and replies
including corrupt payloads, before carrier framing. Simulated durations
include work still draining at the end of a batch. Direct reads drained sooner,
which raised their aggregate rate even as their byte count fell. CPU covers the Node.js
simulator, including its scheduling and hash checks. The query column
excludes creating the graph and auditing caches. All returned data and
cache entries passed hash verification.

Total process CPU was 337.5 → 257.9 seconds,
and peak resident memory was 760 → 2,685 MiB.
We ran four serial pairs of three layouts in alternating order on macOS
ARM64 with Node.js 26.12.1. Reachable and distinct reachable counts matched the
Rust fixture on all 48 reference cases. A replay of the pinned candidate
reproduced the delivery, traffic and latency counters on six cases.

Build the transport and core packages from the TypeScript workspace, then
run the fixture. The candidate comparison uses the local experimental
revision when supplied. Seeds can be passed as a comma-separated list.

```sh
pnpm --filter @hashtree/core build
pnpm --filter @hashtree/fips-transport build
node packages/hashtree-fips-transport/scripts/mesh-scale/run.mjs baseline
node packages/hashtree-fips-transport/scripts/mesh-scale/run.mjs candidate 2281,2333,2377,2423,2473,2521,2579,2621,2671,2729,2777,2833 3f2b18df6badb65777420632bfb8609480fbec78
```

## 2026-11-18 - Fresh 10,000-node layouts with a fixed resource budget

We tested whether a routing change could deliver more verified reads while holding
traffic and CPU steady. Each layout ran 64 reads with steady links, 64 with churn
and 64 with false misses and corrupt replies. Both versions used the same graph,
holders, five-hop limit, four-attempt limit and 700 ms deadline. The router on
master at the start of the experiment was the baseline for both comparisons.

The first candidate started deep searches sooner, spaced the last two hops further
apart and deferred neighbors that had recently asked for the same hash. It delivered
81 of 430 reachable reads on the twelve development layouts, up from 78. We then
fixed the candidate and ran 16 previously unused layouts.

The second candidate remembered recent terminal misses and incoming queries for
the same hash. It used an untried last-hop peer when available. With one forwarding
hop left, it started the next probe at two thirds of the usual interval. We selected this
candidate before running a separate set of 24 new layouts.

| Layouts | Router | Verified / reachable | Distinct verified / reachable | Protocol MB | Aggregate Mbps | Query CPU, s | Whole-run CPU, s |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 16 | Baseline | 165/478 | 144/457 | 131.13 | 3.946 | 41.68 | 58.20 |
| 16 | Changed timing | 152/478 | 131/457 | 127.21 | 3.824 | 39.48 | 56.23 |
| 24 | Baseline | 264/826 | 208/658 | 193.85 | 3.898 | 49.25 | 67.19 |
| 24 | Terminal misses and earlier hedges | 265/826 | 209/658 | 193.36 | 3.933 | 51.13 | 69.09 |

On the 16 layouts, ordinary deliveries fell from 138 to 125. Adversarial
deliveries stayed at 27. For comparison, the routing implementation shipped in
transport 0.5.1 delivered 147 of the same 478 reachable reads and used 123.98 MB.
On the separate 24 layouts, ordinary deliveries rose from 198 to 200 and
adversarial deliveries fell from 66 to 65. Three layouts gained reads, two lost
reads and 19 tied. The highest successful per-case p95 latency moved from 680
to 694 ms on ordinary workloads and from 688 to 659 ms on adversarial workloads.
The current routing policy and library version were retained.

Protocol traffic counts requests and replies, including rejected data, before
carrier framing. Aggregate Mbps divides those bytes by simulated elapsed time.
Query CPU measures the test thread from the start of the read batches through
their completion, including placements and reachability checks. It excludes
building the 10,000 nodes and the final cache audit. Whole-run CPU is process
user plus system time. Both were measured with serial debug binaries on macOS
ARM64. For each four- or six-layout group we ran both versions twice in opposite
orders and summed the per-group means. CPU timing varies with host load.
Distinct queries count a concurrent eight-reader burst once. Every delivered
payload and every cache in the adversarial cases passed hash verification.

From the Rust workspace, this runs the 16-layout baseline. The test also reports
query CPU microseconds on Linux and macOS. Use seeds 613 and 641 for a short run.

```sh
HASHTREE_MESH_SIM_SEEDS=613,641,683,719,757,797,839,877,919,953,991,1031,1069,1109,1151,1193 \
  cargo test --locked -p hashtree-fips-transport --lib provider_route::mesh_scale::mesh_10000_nodes \
  -- --exact --nocapture --test-threads=1
```

The separate confirmation used seeds 1231, 1277, 1321, 1361, 1409, 1451, 1493,
1531, 1579, 1621, 1663, 1709, 1753, 1789, 1831, 1873, 1913, 1951, 1993,
2039, 2081, 2129, 2179 and 2221.

## 2026-11-18 - Avoiding duplicate hash queries during a mesh search

A transport remembers up to 128 authenticated inbound requests for one second.
When the router is about to send a hash back to a peer that recently requested it,
it can use an untried eligible peer for that attempt. The comparison happens
at dispatch, so a hedge can use information that arrived after the lookup
started. The initial attempt window can make substitutions. Later windows keep
their reserved order, and the original peer remains eligible when every
alternative has asked for the same hash. The limit stays at four attempts.
A completed miss or error also releases a slot while a slower peer is pending.

We ran 4,992 reads per version on the production routing code in the in-memory
carrier. The 100-node comparison uses seeds 17 and 29. For 1,000 and 10,000
nodes it also uses 71 and 113 plus eight fresh layouts (197, 239, 293, 359,
419, 467, 523 and 587). Within each profile, the two versions use the same placements, faults,
five-hop limit and 700 ms deadline. Ordinary includes
steady links and churn. Adversarial peers return false misses or corrupt blobs.

| Nodes | Workload | Verified reads / reachable | Distinct verified queries / reachable | Aggregate Mbps | Successful p95, ms |
| ---: | --- | ---: | ---: | ---: | ---: |
| 100 | Ordinary | 203/223 → 212/223 | 161/174 → 163/174 | 1.94 → 2.02 | 372–506 → 368–500 |
| 100 | Adversarial | 90/99 → 91/99 | 69/78 → 70/78 | 1.91 → 2.03 | 464–513 → 436–476 |
| 1,000 | Ordinary | 571/1,140 → 633/1,140 | 473/860 → 507/860 | 1.85 → 1.97 | 502–698 → 510–694 |
| 1,000 | Adversarial | 199/392 → 216/392 | 164/315 → 174/315 | 3.83 → 4.11 | 496–677 → 496–644 |
| 10,000 | Ordinary | 55/348 → 59/348 | 55/285 → 59/285 | 1.60 → 1.69 | 283–675 → 363–668 |
| 10,000 | Adversarial | 18/82 → 19/82 | 18/75 → 19/75 | 8.20 → 8.76 | 311–694 → 316–676 |

Distinct queries count a concurrent eight-reader burst once. Reachable means
an honest path to an original holder exists within the hop limit. Each p95
range spans the cases with at least one delivery. Small successful samples
at 10,000 nodes give individual reads considerable weight. Mbps sums protocol
bytes, including corrupt payloads, over simulated durations and excludes carrier framing.

| Nodes | Total protocol traffic, MB | Link attempts | Root deadlines | Other incomplete reads | Simulator CPU seconds |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 7.23 → 7.36 | 37,320 → 38,087 | 56 → 38 | 35 → 43 | 2.58 → 2.89 |
| 1,000 | 61.49 → 64.67 | 658,970 → 689,127 | 1,176 → 903 | 358 → 552 | 6.32 → 6.75 |
| 10,000 | 94.58 → 99.90 | 825,161 → 865,767 | 1,806 → 1,448 | 425 → 778 | 8.29 → 9.58 |

CPU is the median measured user plus system time for 384 reads at each size on macOS
ARM64. Four serial debug-binary samples per build used seeds 17 and 29,
first in baseline, candidate, candidate, baseline order and then with the builds
in the opposite roles. Both builds reused immutable test identities
across cases and used binary peer addresses in the simulated links. All routers,
stores, faults and routing observations were constructed afresh for each case.
Delivery counts and latency percentiles repeated exactly in all timing trials. In a few
100-node cases an extra request added 36 bytes. The traffic tables use the first trial.
The host was shared, and these process times include fixture and router work.
At 10,000 nodes individual CPU samples spanned 7.10–11.63 seconds before the
change and 7.55–11.48 seconds after it.

| Fresh layouts only | Ordinary verified reads / reachable | Adversarial verified reads / reachable |
| ---: | ---: | ---: |
| 1,000 nodes | 385/758 → 428/758 | 131/257 → 134/257 |
| 10,000 nodes | 42/216 → 43/216 | 13/46 → 13/46 |

At 10,000 nodes, all five additional deliveries were distinct queries. Four occurred
on the original layouts and one on the eight fresh layouts. Total traffic increased
by 5.32 MB. At 1,000 nodes, bytes per verified read fell from 79.9 to 76.2 kB.

On the four earlier 10,000-node layouts, an experimental eight-peer terminal
window delivered 26 of 168 reachable reads and used 40.28 MB. The previous
four-attempt policy delivered 18 using 30.40 MB. The published change keeps
the four-attempt limit. The simulation verified every delivered payload and
the contents of every cache in the adversarial cases.

From the Rust workspace, the following runs the validation layouts at the largest size.

```sh
HASHTREE_MESH_SIM_SEEDS=197,239,293,359,419,467,523,587 cargo test --locked \
  -p hashtree-fips-transport --lib provider_route::mesh_scale::mesh_10000_nodes \
  -- --exact --nocapture --test-threads=1
```

## 2026-11-18 - Hash queries on meshes up to 10,000 nodes

The production FIPS provider route now skips the peer that just requested the
same hash. With more than two hops left, another peer can use the freed attempt.
In the last two hops, the search keeps its original window to limit traffic.
A one-attempt search always gets an eligible peer when one exists. A directly
observed hash mismatch defers that peer for 30 seconds on the observing node.
A timeout or explicit miss does not start a cooldown. Each route tracks up to
256 peers, and a search with deferred alternatives reports an incomplete
result. The explicit peer roster is deduplicated when it changes.

We compared this policy with the previous deadline-aware scheduler using the
same five-hop limit, four-attempt budget, 700 ms deadline, placements and
link conditions described below. We ran 384 reads per version at 100 nodes
(seeds 17 and 29) and 768 each at 1,000 and 10,000 nodes (also seeds 71 and
113). Each 64-read case includes concurrent bursts for one hash. Ordinary
combines steady links and churn. Adversarial nodes returned
false misses or corrupt payloads. Every delivered blob matched its requested
hash, and the adversarial runs verified the contents of every cache.

| Nodes | Workload | Verified / reachable, before → after | Aggregate Mbps | Successful p95, ms |
| ---: | --- | ---: | ---: | ---: |
| 100 | Ordinary | 200/223 → 203/223 | 1.77 → 1.94 | 392–590 → 372–506 |
| 100 | Adversarial | 82/99 → 90/99 | 3.74 → 1.91 | 470–526 → 464–513 |
| 1,000 | Ordinary | 132/382 → 186/382 | 1.58 → 1.83 | 540–661 → 502–668 |
| 1,000 | Adversarial | 33/135 → 68/135 | 6.45 → 3.93 | 557–674 → 576–628 |
| 10,000 | Ordinary | 11/132 → 13/132 | 1.42 → 1.58 | 283–630 → 283–630 |
| 10,000 | Adversarial | 4/36 → 5/36 | 7.17 → 7.90 | 373–447 → 311–488 |

Reachable means an honest path to an original holder exists within five hops.
The p95 column gives the range of per-case percentiles among verified reads.
Cases with zero successes have no latency percentile. At 10,000 nodes several
percentiles represent a single delivery. Bandwidth counts aggregate blob-wire
bytes over simulated time and excludes carrier framing and retransmissions.

| Nodes | Workload | Probe attempts, before → after | Blob-wire bytes | Root deadlines | Other incomplete reads |
| ---: | --- | ---: | ---: | ---: | ---: |
| 100 | Ordinary | 26,927 → 26,868 | 4,499,418 → 4,920,858 | 53 → 51 | 3 → 2 |
| 100 | Adversarial | 9,537 → 10,454 | 4,703,628 → 2,311,787 | 8 → 5 | 38 → 33 |
| 1,000 | Ordinary | 152,022 → 162,583 | 8,764,442 → 10,013,272 | 348 → 305 | 32 → 21 |
| 1,000 | Adversarial | 42,918 → 54,099 | 17,213,041 → 10,435,484 | 79 → 78 | 144 → 110 |
| 10,000 | Ordinary | 188,188 → 211,563 | 7,955,179 → 8,851,461 | 477 → 489 | 24 → 10 |
| 10,000 | Adversarial | 53,835 → 63,407 | 19,458,320 → 21,543,725 | 103 → 123 | 149 → 128 |

At 1,000 nodes, total blob-wire traffic fell from 25.98 MB to 20.45 MB
across the two workloads. Corrupt replies received fell from 3,587 to 1,725 and the
two additional seeds reproduced the delivery gain (ordinary 57/187 → 92/187,
adversarial 16/63 → 32/63). At 10,000 nodes corrupt replies rose from
4,179 to 4,590, and only 18 of 168 reachable reads finished. All remaining
root reads in these experiments ended at a deadline or with an incomplete
search error.

At 1,000 nodes on the original two seeds, always refilling the return slot
verified 99 ordinary and 42 adversarial reads using 12.30 MB in total.
Always preserving the window verified 78 and 31 using 8.55 MB. The selected
policy verified 94 and 36 using 10.42 MB. Limiting every forwarded search
to three attempts verified 64 and 24.

CPU is the median of two user-plus-system process times for the original
two seeds (384 reads per size). Each debug test binary ran directly under
`/usr/bin/time -p` in baseline, updated, updated, baseline order on macOS
ARM64 with Rust 1.99.0. Compilation was excluded. CPU includes overlay
setup, simulated links and all routers. All modeled counters and latency
percentiles repeated across both timings.

| Nodes | Baseline CPU seconds | Updated CPU seconds |
| ---: | ---: | ---: |
| 100 | 1.88 | 1.53 |
| 1,000 | 6.22 | 5.57 |
| 10,000 | 28.50 | 27.37 |

Run the full three-size workload from the Rust workspace. The seed override
takes a comma-separated list. To reproduce the baseline, apply just the
updated `provider_route/mesh_scale.rs` test file to the parent revision.
That keeps its original carrier and forwarding policy.

```sh
cargo test --locked -p hashtree-fips-transport --lib \
  provider_route::mesh_scale:: -- --nocapture --test-threads=1
HASHTREE_MESH_SIM_SEEDS=71,113 cargo test --locked \
  -p hashtree-fips-transport --lib provider_route::mesh_scale::mesh_10000_nodes \
  -- --exact --nocapture --test-threads=1
```

## 2026-11-18 - Sparse blob retrieval at 100 and 1,000 nodes

Run the workload from the `rust/` directory.

```sh
cargo test --locked -p hashtree-fips-transport --lib \
  provider_route::mesh_scale:: -- --nocapture --test-threads=1
```

The fixture builds sparse overlays of 100 and 1,000 nodes. For each size it
runs seeds 17 and 29 under three profiles: steady links, slow/stalled links
with peer departures halfway through, and adversarial replies. In the last
profile, a seeded subset of about 20% of nodes either declines every request
or replies with a corrupt 4 KiB blob. Each case makes 64 reads in batches of
eight, including bursts for one shared hash, single-copy and three-copy blobs,
and absent hashes. The topology, placement, and faults are paired across revisions.

Each node uses the production router, forwarding and provider selection,
using explicit peer rosters and an in-memory carrier. Requests use a 700 ms
search deadline, five Hashtree hops and four provider attempts per window.
Graph reachability counts paths through live, honest nodes to an original
holder within five hops, before considering attempt and time budgets. The
carrier uses the production TCP/FIPS hash check on received data. Adversarial
cases verify the contents of every cached blob as well as all delivered blobs.

`Ordinary` combines the steady and churn profiles. Latencies are per-case
percentiles of verified reads, shown as a range across cases. Bandwidth is
aggregate blob-wire bytes divided by summed simulated durations, in Mbps.
It includes corrupt payloads and excludes transport framing and retransmits.

| Nodes | Workload | Verified / reachable | p50 latency, ms | p95 latency, ms | Mean aggregate Mbps |
| ---: | --- | ---: | ---: | ---: | ---: |
| 100 | Ordinary | 151/223 → 200/223 | 314–490 → 220–377 | 632–656 → 392–590 | 1.19 → 1.77 |
| 100 | Adversarial | 48/99 → 82/99 | 426–450 → 290–316 | 636–659 → 470–526 | 2.30 → 3.74 |
| 1,000 | Ordinary | 28/195 → 75/195 | 460–568 → 369–526 | 624–688 → 587–661 | 0.80 → 1.64 |
| 1,000 | Adversarial | 8/72 → 17/72 | 440–460 → 363–378 | 568–686 → 629–674 | 3.62 → 6.82 |

| Nodes | Workload | Deadlines | Other incomplete | Link attempts | Blob-wire bytes |
| ---: | --- | ---: | ---: | ---: | ---: |
| 100 | Ordinary | 105 → 53 | 0 → 3 | 18,421 → 26,927 | 3,214,212 → 4,499,418 |
| 100 | Adversarial | 80 → 8 | 0 → 38 | 6,362 → 9,537 | 3,166,221 → 4,703,628 |
| 1,000 | Ordinary | 228 → 164 | 0 → 17 | 41,762 → 77,634 | 2,235,658 → 4,514,991 |
| 1,000 | Adversarial | 112 → 29 | 8 → 82 | 11,704 → 20,874 | 4,910,428 → 8,760,574 |

An idle search now starts its next candidate as soon as the previous
attempt finishes without data. While a candidate is pending, a hedge starts
after 50–100 ms, with pace based on time left, candidates and remaining hops.
At 1,000 nodes link attempts per verified delivery fell from 1,492 to 1,035,
alongside the increase in total traffic.
The ordinary workload still leaves 120 of 195 graph-reachable reads
unresolved at 1,000 nodes. The updated adversarial runs at that size rejected
1,839 corrupt replies, and all returned and cached bytes verified.

Advancing idle searches alone with 100 ms hedges delivered 39/195 at 1,000
nodes. Fixed 50 ms and 25 ms hedges delivered 77/195 and 78/195, at 78,347
and 79,774 link attempts. Deadline-aware pacing keeps the longer interval
when the budget permits it. Across the listed cases the remaining root
results were deadlines or incomplete-search errors, with zero explicit
misses observed.

Process CPU time includes the entire simulator workload at each size, with
all three profiles. On macOS ARM64 the median user plus system CPU time was
1.49 → 1.91 seconds at 100 nodes and 4.71 → 6.10 seconds at 1,000 nodes.
Two samples per build ran serially in baseline, candidate, candidate, baseline
order. All modeled counters and latency percentiles repeated exactly.
Build the library tests with `cargo test --locked -p hashtree-fips-transport
--lib --no-run` at the harness-only revision and the scheduler revision.
Run each printed debug binary directly to exclude Cargo and compilation.

```sh
/usr/bin/time -p "$test_binary" provider_route::mesh_scale::mesh_1000_nodes \
  --exact --nocapture --test-threads=1
```

## 2026-11-18 - Mesh route exploration with stalled links

The `mesh_topology` probe uses the production `BlobRouter` and blob codec.
It trains one known provider, then makes 12 uncached 4 KiB reads through two
stalled links and two healthy links. Both runs use two in-flight slots and a
5 ms hedge delay. Exploration runs on every read to stress the policy.

| Metric | Previous router | Updated router |
| --- | ---: | ---: |
| Verified deliveries | 8/12 | 12/12 |
| Request deadlines expired | 4 | 0 |
| Link attempts | 16 | 20 |
| Link attempts per delivery | 2.00 | 1.67 |
| Blob-wire bytes | 33,400 | 49,956 |

Exploration now promotes one candidate per cost tier and keeps the usual
ranking for backups. Free routes still run before paid routes.
Recording actual starts lets the probe advance past a stalled request that was
cancelled when a hedge won. All four links were sampled. Each run sent eight
requests to stalled links. The extra bytes are the four additional verified
replies and their requests. These counts exclude carrier overhead.

## 2026-11-11 - Browser Cold Loads: Chunk Reads, Shared Blob Reads, Root Event Bursts

Question: a cold repository load in a browser app spent most of its time in
three shared TypeScript paths. How much does each cost, and what removes it?

A browser profile of a cold load against a public blob server showed 64 KiB
chunks of one range arriving strictly one at a time at about 30 ms each (a
2 MiB range took about 1.3 s; the same chunks requested together took about
0.3 s), the same small blob downloaded up to 11 times in parallel with 26 of
about 180 downloads being duplicates, and about 630 tree-root events for an
owner with about 150 trees taking 1.3-1.5 s to reach the page while the worker
was about 40% idle. The rows below reproduce each cause in Node on an Apple
silicon Mac, three runs each, parent revision against this change. Other
builds were running on the host (load average 15-50), so treat the absolute
values as upper bounds; the two in-memory rows report process CPU time for
that reason.

| Operation | Before | After |
| --- | ---: | ---: |
| 2 MiB range of a plain file, 64 KiB chunks, 30 ms per store read | 1,030-1,045 ms | 92-101 ms |
| 8 MiB whole plain file, same store | 4,012-4,421 ms | 285-293 ms |
| 2 MiB range of an encrypted file, same store | 1,144-1,262 ms | 100-181 ms |
| 8 MiB whole encrypted file, same store | 4,544-5,162 ms | 380-400 ms |
| 5,000 whole reads of a 100-chunk file from memory, CPU time | 857-961 ms | 910-934 ms |
| 2,000 ranges of 4 KiB from a 64 MiB file in memory, CPU time | 349-394 ms | 359-395 ms |
| Downloads for 11 concurrent relay-worker reads of one blob | 11 | 1 |
| Peer requests for 6 concurrent identical worker reads | 6 | 1 |
| 630 root events, 150 trees: median time to forward an event | 1,234-1,817 ms | 1-2 ms |
| Same burst: last event forwarded | 2,253-3,210 ms | 2-3 ms |
| Same burst: last new root reported | 2,253-3,210 ms | 12-22 ms |
| Same burst: store write transactions | 808-843 | 1 |
| Same burst: root update notifications | 178-213 | 150 |

1. `readFileRange` and whole-file reads awaited one `store.get` per chunk.
   They now read ahead of the reader: at most 16 store reads in flight for one
   file read, shared across nested nodes, and at most 8 MiB of declared chunk
   bytes outstanding, so 2 MiB chunks read four at a time. Bytes are still
   assembled strictly in link order, the first missing chunk in file order is
   still the one reported, and links of unknown size fall back to on-demand
   reads. The read rows use a `MemoryStore` behind a 30 ms timer per read;
   with no store latency the read-ahead costs nothing measurable. The explicit
   `prefetch` option of `readFileStream` is unchanged.
2. The relay worker ran one blob-router search, and so one download, per
   caller. Concurrent reads of a hash now share one search; the other worker
   entry shares per hash and read options, so a read restricted to some
   sources never receives bytes found by a broader one. Nothing is kept after
   a read settles: a miss or an error is shared only with callers that were
   waiting on that same read. Each caller gets its own copy of the bytes,
   because several paths transfer their buffer to another thread. The
   download rows count requests seen by a stubbed blob server and peer
   provider in the worker test harnesses.
3. Each tree-root event waited for a signed-event snapshot write and a root
   record write before it was forwarded, and so did every repeated and
   superseded delivery. Events are now forwarded first. An event that is older
   than the cached root, or is the event the cached root came from, is dropped
   without store work; otherwise the newest event per tree updates the
   in-memory root and is reported, and snapshot and root records follow in
   grouped writes (one IndexedDB transaction per round with the Dexie store).
   The burst rows deliver each tree's current root four times plus one
   superseded root for every fifth tree, all in one tick, against a store that
   runs one 2 ms write transaction at a time.

The burst also exposed an ordering fault in the previous code: with
deliveries overlapping on store writes, a superseded root could be reported
after the newer one and could be the one left in the store (the 178-213
notifications above include those). Root updates are now serialized per tree,
and tests cover out-of-order delivery during a pending write, same-second
ties, and removal during a pending write.

Not measured here: real IndexedDB transaction cost, and the end-to-end cold
load in a browser, which belongs to the consuming app after it adopts these
versions. The worker also moved from nostr-pubsub 0.5.20 to 0.5.28; its
in-memory event store speedup does not apply to the worker, which stores
events in hashtree blocks, so no ingestion gain is claimed for that move.

## 2026-11-10 - Streamed Encrypted Files and Directory Adds

Question: after the core moved to versioned encryption and one range reader,
which of the earlier read-path and `htree add` changes still pay for
themselves?

Each row compares the parent revision with this change on an Apple silicon
Mac, three runs each, default release profile. Other builds were running on
the host. The library rows use `MemoryStore` and a deterministic pseudo-random
buffer; the `htree add --local` rows are wall time of the command with a fresh
data directory per run and include the first, cold run.

| Operation | Before | After |
| --- | ---: | ---: |
| Encrypted 64 MiB file served as 256 KiB pieces | 315-323 ms | 40-41 ms |
| Encrypted 256 MiB whole-file `get`, fanout 16 | 167-211 ms | 171-193 ms |
| `put_stream` of 4,000 encrypted 3 KiB files | 21-22 ms | 21-23 ms |
| `htree add --local`, 300 files of 1 MiB | 803-1,932 ms | 257-536 ms |
| `htree add --local`, 4,000 files of 3 KiB | 450-950 ms | 441-678 ms |

1. Serving an encrypted file by CID did one range read from the root per
   256 KiB piece, decrypting the whole leaf chunk each time. The stream now
   keeps the current decrypted leaf (`HashTree::read_file_chunk_at`) and
   slices pieces from it. The first row drives both loops against the library;
   the HTTP path adds store I/O to both, so the server-level ratio is smaller.
2. Directory adds hash and encrypt files of 32 KiB and larger on up to four
   threads. Small files stay sequential: their cost is the serialized store
   commit, and the 3 KiB row shows no gain.
3. Whole-file reads append into one buffer instead of concatenating per tree
   level, and `put_stream` reads into the chunk buffer instead of a zeroed
   chunk-size scratch buffer. Neither is measurable here. An earlier
   measurement of the same two changes on a 2-vCPU x86-64 Linux VM, against
   the pre-versioned-encryption core, showed roughly 510 to 300 ms and 290 to
   45 ms; that was not repeated on this base.

The same change set moves the CID range stream's and the logical-file route's
tree reads from async workers to the bounded blocking pool, and reuses one
runtime for synchronous S3 calls. Neither was measured.

Not measured: the HTTP server end to end, disk or network stores, S3, Linux
and Windows on this base.

## 2026-11-10 - Git Repo Tree Build Writes

Question: where does `git-remote-htree` spend time turning imported git
objects into a repo tree, and does zlib compression dominate?

`benches/git_storage.rs` now times object compression and the tree build
separately. It writes the 671 source files under `rust/` (11.4 MB) as blob
objects and builds the tree, once per backend. Each figure below is the range
of three runs of the release bench binary on an Apple silicon Mac, parent
revision versus this change. Other builds were running on the host, so treat
small differences as noise.

| Phase | Before | After |
| --- | ---: | ---: |
| Compress 671 objects (zlib, default level) | 140-217 ms | 139-148 ms |
| Build tree, LMDB backend (the default) | 3.36-4.15 s | 11-59 ms |
| Build tree, filesystem backend | 181-305 ms | 173-211 ms, one run 912 ms |

Compression was not the cost. The LMDB build was: `GitStorage` wrote every
blob and directory node with its own `put`, and the LMDB store commits one
transaction per `put`, a few milliseconds each on this machine. `LocalStore` also did
not forward `put_many`, so a batch would still have been written one
transaction at a time.

Tree builds now write through `BufferedStore` and flush to the local store in
batches of about 16 MiB, and once more before a build returns (also when it
fails, so a retry finds the blobs already stored). `LocalStore` forwards
`put_many` to its backend.

The same change removes copies the build made of everything it held: the whole
compressed-object map was cloned for every build, each object was cloned again
into its prefix bucket, and pack files and packed object ids were cloned. The
map is now copied only if an object has to be seeded from the base root, and
the rest is borrowed. That effect on peak memory was not measured.
`validate_root_contains_direct_refs` resolves `.git/objects` once instead of
once per ref, and skips the per-ref lookups when a pack checkpoint makes them
irrelevant.

Not changed: the zlib level. A different level changes the bytes of every
loose object, so already-published repos would stop deduplicating.

Not measured: a real push or clone end to end, Linux (where a commit costs
less than macOS `F_FULLFSYNC`, so the ratio will be smaller), repos large
enough to cross the 16 MiB batch boundary, and peak memory.

## 2026-10-06 - Automatic ARM Crypto Acceleration

The core's previous SHA2 0.10 and AES-GCM 0.10 dependencies selected software
SHA-256, AES and POLYVAL on ARM64 unless additional features and compiler flags
were supplied. The ordinary release build did not enable those flags. The core
now uses SHA2 0.11, AES-GCM 0.11 and HKDF 0.13, whose backends detect ARM CPU
capabilities automatically. Existing x86 detection and portable software
fallbacks remain available. This is a dependency update with four nonce
constructor adaptations, without custom cryptography or new runtime settings.
AES-GCM enables only `aes` and `alloc`; keys and random nonces continue to use
the existing random-number source. Other crates retain their existing SHA2
dependency. The new dependencies require Rust 1.85, now declared for the core;
the verification below used Rust 1.99, not an independently tested minimum.

The reproducible `hashtree-lmdb` example `crypto_backend_bench` measures the
production `HashTree::put`/`get` path over MemoryStore and `put_stream`/`get`
over a fresh disk-backed LMDB store. Each sample uses the same deterministic
16 MiB input with distinct chunks and the default 2 MiB chunk size. LMDB import
includes reading the source file and a final forced sync. Input generation,
store creation and comparison of recovered bytes are outside the timers. Reads
have warm OS caches; this is not a cold-storage or network benchmark.

Matched release binaries on an Apple M4 Pro used the same example, toolchain,
build settings and unchanged dependency versions outside the new crypto stack.
After a crypto warm-up, the binaries ran in baseline/candidate/candidate/baseline
order, with three fresh-store repetitions per invocation: six samples per
version and case. Other work from this task paused during the timed runs.
Medians are shown below; ratios are baseline time divided by candidate time.

| Operation, 16 MiB | Previous ms | Updated ms | Ratio |
| --- | ---: | ---: | ---: |
| Memory encrypted put | 148.17 | 16.03 | 9.25x |
| Memory encrypted get | 89.27 | 3.41 | 26.18x |
| Durable LMDB encrypted import | 169.15 | 35.46 | 4.77x |
| Warm LMDB encrypted get | 90.10 | 4.43 | 20.32x |
| Memory plaintext put | 30.79 | 7.06 | 4.36x |
| Durable LMDB plaintext import | 49.43 | 25.97 | 1.90x |
| Memory plaintext get | 1.11 | 1.07 | 1.04x |
| Warm LMDB plaintext get | 1.51 | 1.43 | 1.05x |

All measured reads reproduced the original bytes. Plaintext and encrypted root
identities matched across both builds and both backends. Plaintext read times
were effectively unchanged, as expected for reads without decryption. These
results support the backend update for CPU-bound ARM workloads; they do not
predict WAN throughput, production Pool latency, cold-cache behavior or x86
speedups.

The optimized core suite passed 255 tests, including documentation examples
and the existing fixed SHA-256/CHK key/ciphertext compatibility vectors; two
existing vector-generation helpers remained ignored. The core library and
interop tests also passed (152 tests, two ignored helpers) with SHA-256,
SHA-512, AES and POLYVAL explicitly forced to their portable software backends.
This verifies fallback correctness separately from the measured automatically
accelerated build. Formatting passed. Strict Clippy on Rust 1.99 encountered
17 `double_must_use` diagnostics from unchanged `async_trait` expansions in the
blob-route and store traits; focused core Clippy passed with only that lint
allowed. Those unrelated traits were left unchanged.

Reproduce with `cargo run --release --locked -p hashtree-lmdb --example
crypto_backend_bench -- 16 3` from `rust/`. For comparisons, build the same
example against each dependency set first, then alternate the saved binaries
without concurrent compilation. The example prints individual measurements
and synthetic root identities so comparisons can also check byte compatibility.

## 2026-10-04 - Coalesce Append Payloads with Projection Writes

The incremental Nostr writer flushed event payloads separately before constructing
their index projections, even when an 8 MiB write buffer was enabled. The signed
seven-commit fixture reproduced fourteen backing-store batches. The payload
flush is now an intermediate threshold hint: small payloads can share the next
projection or final batch. The final flush still precedes a returned root, and
the crawler validates and synchronizes that root before saving its checkpoint.
A zero threshold retains unconditional flushing. Initial builds and repair
paths are outside this change.

This intentionally changes internal failure timing and which unreferenced blobs
may remain after a failed commit. It does not acknowledge a root or advance a
checkpoint on failure. The existing buffer threshold covers retained payloads
and index nodes together; as before, one stage can exceed the threshold before
the next hint.

The source-bound RED test reached the expected fourteen-versus-seven assertion
after root and blob checks. With the change, all four focused Nostr cases, two
guarded Pool cases and two buffer cases passed. The seven-commit fixture now
uses seven batches, retaining the same 4,625 new blobs and 11,244,096 bytes.
Tests cover default/initial-build behavior, read refusal, threshold/final and
partial write refusal, uncached retry, replacements, tombstones, and exact
history/content after reopening the database. Six actual CLI catch-up cases
and the isolated filesystem-full checkpoint/resume case also passed.
The exact release binary repeated those checks and passed the legacy repair
interoperability witness.

Matched native Linux test binaries used the same diagnostic markers, dependency
lock, one CPU, production no-readahead setting, 8,192 historical events and 49
incoming notes. Six cold-preparation safety cases and four alternating
baseline/candidate/candidate/baseline runs passed. Background storage activity
continued. Each cell below contains the two measured append times:

The measured path includes the deployed legacy author-ID reconciliation before
append. Canonical direct-build diagnostics omit that step and require separate
timing qualification.

| Storage / tags per note | Previous (seconds) | Combined (seconds) |
| --- | --- | --- |
| Warm / 0 | 0.587, 0.600 | 0.277, 0.304 |
| Warm / 32 | 0.902, 0.831 | 0.638, 0.756 |
| Cold / 0 | 3.633, 3.535 | 2.734, 7.572 |
| Cold / 32 | 4.119, 5.195 | 4.511, 4.387 |

Backing write calls fell from two to one for the untagged append and three to
two for the tagged append. Attributed physical writes fell consistently:
approximately 21.0% / 10.9% in the warm untagged/tagged cases and 11.4% / 5.4%
in the cold cases. Cold physical reads stayed at 7,745,536 / 24,518,656 bytes.
Roots, retained history and logical stored bytes matched across both versions.

Warm append times improved in both repetitions. Cold latency remains
inconclusive: the slow candidate run already spent 2.846 seconds in unchanged
existing-ID lookups before reaching the changed flush, while other runs spent
roughly 0.2–0.3 seconds there. Shared-storage latency therefore remains a material
limit on interpreting the timing results. This supports fewer transactions and
physical writes, not a general cold-crawl speedup claim.

## 2026-10-04 - Qualify Cold Pool Read Attribution

The diagnostic fixture is now available behind the CLI's explicit
`archive-io-diagnostics` test feature. It is excluded from ordinary test runs:
cold admission requires an isolated Linux process, a real filesystem and a
controlled resource window. Production code and the 256-change parallel-update
trigger are unchanged.

Native Linux qualification passed six mapping/eviction safety cases, each in a
fresh process, followed by the 8,192-history-event cold Pool fixture. It appends
49 signed events with either zero or 32 tags, using the production 64 MiB cache,
256-event commits and 8 MiB write coalescing. Warm/cold pairs produce identical
roots, all nine logical projections, retained history/signatures, cache counters
and submitted write work.

That qualification used the deployed legacy append lineage, which includes
repair reconciliation. Canonical catch-up directly calls `build`; its integrated
fixture follows that actual path and labels rows `canonical-direct-build`.
The qualified mapping/eviction modules are unchanged, but canonical cold timings
and counters require their own Linux run. Do not mix either lineage's receipts
or import legacy repair runtime code merely to compile this diagnostic.
The source-linked canonical harness passes all nine local tests, including the
four complete warm append cases and retained-signature verification. Its
diagnostic-disabled mode retains only the four original cache tests.

The first fixture version failed correctly: reopening its evicted Pool warmed
80 of 652 catalog pages, exceeding its original 10% admission limit. The corrected
setup records the reopen footprint, syncs the quiescent synthetic Pool, drops
resident pages from the exact owned file mappings and requests file-only cache
eviction. Both catalog and member must have **zero resident pages** immediately
before a cold append; warm controls require at least 95%. File preparation
rejects symlinks and multiply linked files. Mapping reset requires the held
file's exact device/inode, one read-only shared mapping, offset zero, aligned
bounds and at most 2 GiB. Tests preserve the same mapped bytes and unrelated
file residency, and reject writable or ambiguous mappings. No global cache
drop, production data or other process mapping is used.

| Incoming tags | Backing gets | Logical returned bytes | Cold storage read bytes | Warm storage read bytes |
| --- | ---: | ---: | ---: | ---: |
| 0 | 237 | 1,293,913 | 45,838,336 | 0 |
| 32 | 1,599 | 11,058,618 | 45,838,336 | 0 |

Both cold cases admitted `[0, 0]` resident pages and recorded 89,528 input
blocks during actual get calls. Reopen had warmed 80 catalog pages and 2,844 or
3,100 member pages before the reset. The 45,838,336-byte cold read equals the
combined catalog and member file lengths. Most get latency occurred in the four
reconciliation reads and existing-event lookups, before later tag updates.
The synthetic process used default read-ahead, while the live writer was
independently verified to have `HTREE_LMDB_NO_READ_AHEAD=1`. The whole-file reads
therefore do not establish a production read-path bug.

A second qualification reused the exact frozen legacy binary in fresh processes
with that flag explicitly set. All seven Linux checks passed again, including
zero-page cold admission and unchanged warm/cold roots, history and logical I/O.

| Incoming tags | Total cold storage reads | Get-call input blocks | Write-call input blocks |
| --- | ---: | ---: | ---: |
| 0 | 7,745,536 bytes | 7,632 | 7,496 |
| 32 | 24,518,656 bytes | 37,032 | 10,856 |

The whole-file pattern disappeared with production's existing mode. In addition
to reads serving gets, internal write operations accounted for about 3.84 MB and
5.56 MB of cold input. Single-run wall times remain unsuitable for a speedup
claim on the shared host; no production setting was changed.

The next focused candidate is bounded parallel traversal of independent
existing-ID lookup branches. Those lookups currently await each touched subtree
serially, and the cold parity fixture attributes 53/55 gets to that stage.
This is a hypothesis, not an implemented optimization: preserve the 256-change
update trigger and all write behavior; retain exact lookup results, errors and
read set, cap workers, and drain every worker before returning. Require a quiet
one-core no-read-ahead comparison with identical archive roots/history and no
memory-pressure regression before considering deployment. Do not remove member
metadata or corruption checks merely to avoid their reads.

This qualification is **not** an accepted performance comparison: another VM
was compiling on the shared host. Timings and throughput differences cannot be
attributed to a candidate. File eviction also does not clear controller caches
or reproduce production archive size, fragmentation and memory reclaim.

Per-projection process I/O measures non-overlapping intervals; per-thread fault
and block counters surround actual get/write calls, including reads internal to
Pool writes. First-hash and repeated-read time are separate. Reopen/setup,
checkpoint sync, rebuild and signature validation are excluded from append
timing. Unsupported OS counters are unavailable rather than reported as zero.

Run `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --bin htree
--features archive-io-diagnostics append_io_cold_pool_first_reads -- --nocapture`
on Linux in a fresh process after the mapping/eviction helper checks. Keep a
10 GiB fixture free-space floor, the separate host floor, one-core quota,
2 GiB memory-high and 4 GiB maximum, and record interference/resource evidence.
The default history size is 8,192; only 32,768 is accepted as an optional larger
case via `HTREE_APPEND_IO_HISTORY`. Do not expand before the default gates pass.

## 2026-10-04 - Reject the Small-Update Parallelism Candidate

An experimental candidate lowered the B-tree parallel-update trigger from 256
to 32 changes. It was not accepted for integration or deployment. The existing maximum of four workers, commit size, write coalescing,
cache bounds and archive format remain unchanged. This affects both link and
string indexes. Regression checks compare 31, 32, 49 and 256 link changes with
single-worker results, including exact roots, superseded nodes and readable old
roots; a 49-change string update checks the same root and history behavior.
They observe actual worker threads and at most four simultaneous reads, without
requiring multiple available CPU cores. The 32-change check fails with the old
trigger and passes with the candidate.

All 458 index/core/collection/Nostr library tests and 42 CLI catch-up tests pass;
two pre-existing library tests remain ignored. An optimized native Linux screen
then compared fresh processes and disposable Pools under a one-core CPU quota,
2 GiB memory-high and 4 GiB memory-max. A small harness imports the unchanged
production cache and diagnostic modules, linking the actual index, Nostr and
Pool crates. Both builds have identical dependency locks and fixture sources.

| Backend | Incoming tags | Existing trigger | Candidate trigger |
| --- | ---: | ---: | ---: |
| MemoryStore | 0 | 16.704 ms | 13.161 ms |
| MemoryStore | 32 | 114.230 ms | 132.499 ms |
| LMDB Pool | 0 | 734.910 ms | 365.179 ms |
| LMDB Pool | 32 | 950.564 ms | 1,128.262 ms |

This first pair is mixed: the tag-rich Pool append is 18.7% slower, despite
identical roots, retained history/signatures, cache counters and submitted I/O.
No competing compiler was sampled. Peak memory is about 338 MiB in both runs,
with no memory-high/max/OOM events or swap use. Total CPU usage is 6.952 versus
6.941 seconds; quota throttling is 139 versus 201 milliseconds. These are whole
fixture resource measurements, not append-only costs. The warm, compact fixture
does not reproduce production read pressure or reclaim.

Backing-write calls account for 708 versus 345 milliseconds in tag-free appends
and 871 versus 1,054 milliseconds in tag-rich appends. Thus almost all of the
observed Pool timing difference is in write-call time, while this scheduling
change does not reduce submitted write work. One pair does not establish a
causal speedup or slowdown. A reversed-pair attempt detected another compiler
during its first run and was excluded; its baseline half was not run.

The candidate is not accepted for deployment: the Linux screen provides no
reliable overall improvement, and the earlier macOS debug results do not
justify rollout. Production remains unchanged. Any follow-up needs a clean
comparison and representative storage pressure rather than a larger claim
from the current synthetic measurements.

## 2026-10-04 - Attribute Small Catch-up Appends to Index and Pool Work

A diagnostic-only fixture uses the production append, reconciliation, 64 MiB
read cache, 256-event commits and 8 MiB write coalescing. It adds 49 signed notes
from a new author to one fixed tree containing 8,192 signed historical notes.
Incoming notes have either no tags or 32 tags each. The historical notes have
eight tags each; incoming timestamps are interleaved with historical timestamps.
MemoryStore and a disposable, physically guarded LMDB Pool produce identical
roots. Every old projection, all nine final projections and every retained event
signature are checked against the original tree and an independent rebuild.

Test-local storage wrappers record reads, bytes, writes and elapsed time. Reads
of known projection roots mark intervals without adding production hooks.
An interval includes work until the following root read, including intervening
flushes and final manifest work. Backing-write time is also recorded separately;
summed read latency can overlap on parallel workers. Pool-internal catalog,
member and verification reads are included in Pool call time but are not counted
as cache misses. This is not a physical disk-throughput measurement.

| Incoming tags per event | Backing cache misses | Written blobs | Submitted write bytes | Pool append |
| --- | ---: | ---: | ---: | ---: |
| 0 | 228 | 494 | 1,353,792 | 1.219 s |
| 32 | 1,686 | 3,407 | 12,656,597 | 6.175 s |

The tag projection accounts for 1,459 of the 1,686 misses in the tag-rich case.
All observed misses are first reads of distinct hashes; there are no backing
read-after-write calls or `has` calls in either case. Reconciliation restores
and scans zero note IDs. This reproduces substantial index fan-out for a small
event count, without an unbounded author-history scan. Increasing the cache
would not eliminate these first reads in this fixture. Few backing write calls
still represent thousands of per-blob Pool operations.

A temporary one-line comparison lowered the B-tree parallel-update trigger
from 256 changes to 32, retaining the existing maximum of four workers. These
are scoped operating-system threads, not merely concurrently polled futures;
Pool reads remain synchronous within each worker. The production source was
restored after the comparison.

| Backend | Incoming tags | Existing trigger | Trial trigger | Lower wall time |
| --- | ---: | ---: | ---: | ---: |
| MemoryStore | 0 | 0.926 s | 0.600 s | 35.3% |
| LMDB Pool | 0 | 1.219 s | 0.853 s | 30.1% |
| MemoryStore | 32 | 4.223 s | 3.646 s | 13.7% |
| LMDB Pool | 32 | 6.175 s | 5.658 s | 8.4% |

Output root hashes, read/hit counts, submitted blob counts/bytes and all history
and signature checks match between triggers. The trial overlaps small projection
updates; it does not reduce their I/O volume. The large tag projection already
uses parallel workers under the original trigger, limiting the total benefit.

These are individual paired macOS debug-build observations with warm OS caches,
compact synthetic historical trees and no production-like memory reclaim or
single-core quota. They do not predict cold production latency or justify a
deployment. The smallest follow-up candidate is the lower trigger with the same
four-worker bound, subject to a representative disposable Linux Pool trial and
working-set measurements. No native runtime setting or deployed source changed.

The diagnostic fixture is retained as experiment evidence and is not installed
in the canonical test suite. In a checkout containing it, reproduce with
`cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --bin htree append_io_attribution_49_events -- --nocapture`.
It retains a 10 GiB physical free-space floor for its temporary Pool and takes
about four minutes in this debug configuration. Setup, signing, seed copying,
full projection comparison and retained-signature verification are excluded
from the reported append times. The diagnostic test itself passed with both
triggers; formatting and the restored production-source hash were checked.

## 2026-10-04 - Catch-up Projection Write Coalescing

Catch-up previously flushed every independent index projection separately inside
each 256-event commit. A PoolStore batch includes pending-location catalog work,
member writes and verification, and stored-location catalog work. Small separate
batches repeat those transactions even though no intermediate projection root is
published.

The CLI now coalesces projection writes using an 8 MiB payload threshold. It
checks the threshold after each projection and unconditionally flushes at the
end of every event commit. Event payload flushing, physical-space admission,
256-event commit size, syncing before author checkpoints, repair behavior and
historical-root retention remain unchanged. `--index-write-buffer-mib 0`
restores per-projection flushing; the allowed range is 0 through 64 MiB and is
runtime tuning outside the saved policy identity. Other library callers retain
their previous default unless they explicitly opt in.

This is a threshold between projections, not a hard total-memory cap: a single
projection can exceed it, as it could before. Additional retained payload
between projections is below the threshold; metadata, allocator overhead and
the currently constructed projection add to process memory. Explicit flushes
never defer writes, including when the pending bytes are below the threshold.

The signed production-path fixture uses 2,051 historical events and 1,573
incoming items, including duplicate notes, overlapping timestamps, wide tag
replacement and tombstones. With order 32 and 256-event commits, backing write
batches fell from **65 to 14 (78.5% fewer)**. Both modes produced the exact same
content-addressed final root and retained exactly 4,625 new blobs containing
11,244,096 payload bytes. Historical projections and bodies remained readable
directly from the backing store. Refusing either a threshold flush or the final
coalesced write returns an error before the next 256-event batch begins, instead
of an uncommitted root; retry from the preserved historical root succeeds.

These are in-memory operation and payload counts, not filesystem allocation or
measured live throughput. Checkpoint logs expose cumulative `store_write_calls`
and `store_submitted_write_bytes` for subsequent live comparison; submitted
bytes include duplicates and do not measure physical disk writes.
All 408 core/collection/Nostr tests and 57 catch-up CLI tests passed on macOS;
two existing vector-generation helpers were ignored. The final boundary tests
also verify unconditional payload and index flushing for each of seven event
commits. Linux physical-space gates and live throughput remain rollout gates.

## 2026-10-03 - Bounded Catch-up Immutable Read Cache

Catch-up now shares a positive-only immutable blob cache across index commits
and author checkpoints. Its default payload budget is 64 MiB with at most 16,384
entries; `--read-cache-mib` permits 0 (disabled) through 256 MiB without changing
the saved catch-up policy. Only successful reads whose bytes match their content
hash enter the cache. Missing, failed and corrupt reads are not reused. Writes,
including optimistic batches, still reach the guarded backing store. Explicit
deletion invalidates affected entries; this private wrapper is used only by the
single append writer, without concurrent garbage collection.

A deterministic fixture exercises the real `NostrEventStore` append path with
256 signed notes, order 8 and 16-event commits. After seeding 128 notes, appending
the remaining notes in eight batches required 586 backing reads with the cache
disabled and 377 with it enabled: **35.7% fewer backing reads**. Both modes
produced the exact same content-addressed root, and every note remained readable
from its appropriate original or final root. This measures read calls in memory,
not production wall time or disk bytes; the small fanout deliberately exercises
multiple tree levels. Production fanout, 256-event commit size, fetch concurrency,
physical-space guards, block syncing, coverage and checkpoint order are unchanged.

Per-author checkpoint logs include cumulative cache hits, misses, payload bytes
and entry count. Compare counter deltas and invocation wall time for a live
acceptance trial, alongside author/event counts and existing fetch/append timing.
The byte limit excludes bounded LRU metadata and transient returned byte copies.
The 41 catch-up unit tests and 16 local-relay CLI integration tests passed on
macOS, including cache-disabled checkpoint resume with the cache enabled,
repair-witness recovery, source failure and exact historical retention. Linux
physical-space fixtures and live throughput remain deployment acceptance gates.

## 2026-10-02 - Bounded Catch-up Fetch Pipeline

The CLI overlaps two author fetches while preserving one ordered writer. The
writer, fetched results and active requests together occupy at most two author
slots. A slot is reused only after its blocks are synced and its checkpoint is
persisted. Each slot keeps its own relay connections and capacity cache; a
required relay can therefore see two connections and two independent bounded
capacity probes per pass. Fetch failures remain errors, and successful earlier
authors can still reach their durable checkpoints before the error is returned.

Network jobs run on the existing Tokio worker pool. The CLI's storage work stays
on its calling thread, so a long synchronous append does not stop a prefetched
query from consuming timely relay replies. Owned jobs are aborted and drained on
ordinary failure; dropping the coordinator aborts them on cancellation. There
are no detached writer jobs or additional result queues.

The focused fixtures cover out-of-order results, backpressure, source reuse,
writer and fetch failures, cancellation, and a synchronous writer phase longer
than the next query's deadline. The signed local-relay integration fixture
compares overlapping fetches with one-author invocations under the same frozen
policy. Test execution and any measured speedup belong to the integration gate;
no live throughput improvement is claimed before that measurement.

Per-author `fetch_ms` can overlap the preceding author's append. `elapsed_ms`
includes any wait for the preceding author to commit, so summing these values no
longer measures process duration. Compare actual invocation wall times and
processed author/event counts. The two-slot bound limits accepted event payload
to twice the per-author cap, not total RSS: parsing, event clones and index
structures need additional memory.

## 2026-10-02 - Catch-up Timestamp Ties and Append Costs

The catch-up pagination guard treated a two-event history at one timestamp as
an observed relay cap of two. When the relay did not provide COUNT, that author
could remain incomplete even after both its broad and exact-second queries
returned the complete pair with EOSE.

For an ambiguous tie below the requested page limit, the source can now supply
the largest unique, signature-verified EOSE page observed under the same frozen
pass interval, kinds and requested limit. It may make one bounded query without
the author filter to obtain that measurement. The measurement is local to that
relay; another relay's events or declared maximum limit cannot substitute for
it. Capacity calls consume the existing page budget even when cached, and probe
events never enter the author's result or archive append.

A tied pair below a verified three-event capacity no longer looks saturated.
A uniform hidden cap of two still leaves that pair ambiguous and requires a
matching same-source COUNT. A tie filling the requested limit always requires
COUNT, regardless of other observed capacities. Signature, filter, EOSE,
deadline, byte/event limits, required-source coverage, checkpoint durability and
retention checks remain in force; probe failures remain incomplete coverage.

This is a source-relative observed uniform-cap heuristic. It does not prove
completeness against a relay that applies filter-dependent caps, omits events,
or otherwise misrepresents its data. COUNT is likewise source-reported evidence,
and an unmarked estimate is not a mathematical completeness proof.

Focused regressions cover the complete pair, a hidden third event under a
uniform cap, independent required sources, the exact frozen probe filter,
probe/count page budgets, probe errors, and full-limit ties. Test execution is
recorded by the integration gate; no throughput improvement is claimed here.

A deterministic optimized Rust fixture separately compared appends to an
order-64 historical tree. It used 2,051 historical events and 1,573 incoming
items, including duplicates, timestamp overlap, hot tags and replaceable events.
Every configuration preserved all nine logical projections and historical
event bodies, and duplicate replay added no blobs.

| Fanout | Commit events | Append milliseconds | New retained payload bytes |
| --- | ---: | ---: | ---: |
| 32 | 256 | 97 | 11,378,893 |
| 64 | 256 | 110 | 12,272,047 |
| 128 | 256 | 122 | 13,735,752 |
| 32 | 1,024 | 49 | 6,821,975 |
| 64 | 1,024 | 55 | 6,868,974 |
| 128 | 1,024 | 51 | 6,965,944 |

These are single in-memory production-path measurements, excluding signing and
verification, not physical disk allocation or live throughput. Wider fanout
reduced node counts but increased retained bytes, so the catch-up fanout stays
unchanged. Larger commits merit separate resource and policy validation; the
current change preserves the existing 256-event commit bound and fixes the
demonstrated pagination blockage. Per-author timings distinguish fetching,
appending, validation, syncing and checkpoint persistence in subsequent runs.

## 2026-09-21 - Complete Tree Traversal and Store Reads

A deterministic TypeScript core regression walks a directory with two links to
the same empty file using the real in-memory store. Reusing the bytes already
loaded by `walkBlocks` reduces encrypted traversal from two store reads per
unique block to one, matching plaintext traversal. This measures read calls,
not wall-clock throughput. Legacy plaintext nodes carrying an encryption key
still traverse their encrypted children.

Missing roots and descendants now throw instead of silently producing a partial
walk. Restoring either block makes the same traversal succeed. The seven focused
cases, all 374 core tests, and the TypeScript package build passed.

## 2026-09-16 - Archive Projection Batch Size and Disk Work

A live incremental Nostr archive projection was doing substantial copy-on-write
index work with 256-event commits. The process had a 4 GiB memory-high threshold,
a 6 GiB maximum, and swap disabled. Raising only the thresholds to 8/10 GiB did
not improve throughput in a three-minute sample. The host had over 45 GiB of
available memory; this was a bounded cache experiment, not an out-of-memory fix.

Keeping those bounds and explicitly selecting 1,024-event commits improved five
consecutive full batches to about 24 events/second, versus about 12 events/second
in the preceding small-batch sample. A subsequent 127-second full batch brought
the six-batch average to about 18 events/second. Superseded index nodes fell from
about 34 to 29 per event. Process I/O counters aligned to the slow full batch's
checkpoint boundaries measured 3.24 MiB written per event, versus approximately
3.8 MiB/event in the small-batch sampling window; the latter was not aligned to
exact checkpoint boundaries. These are live workload observations, not an
isolated benchmark or a universal recommended batch size.
The native binary, B-tree order, update concurrency and durability settings were
unchanged. Projection resumed its durable offset, remained below the 10 GiB hard
limit without OOMs, and the independent recent-post publisher kept succeeding.

The launcher retains its 256-event default and permits an explicit maximum of
1,024. Its argument/resume checks passed, as did six native event-store tests for
bounded commits, recovery and resumed projection equivalence. No Rust change
was necessary for this deployment.

## 2026-09-10 - Unavailable Pubsub Services and Idle Allocation

An Android application using the shared mesh libraries kept authenticated links
connected while some remote pubsub services were unavailable. Repeated service
connection attempts caused unnecessary background work. The adapter now admits
an initial attempt immediately, spaces repeated service attempts by three seconds,
and keeps established traffic immediate. Queued events and retained subscriptions
continue recovering. Peer policy is still evaluated afresh; this scheduling state
is not a trust penalty or a long-lived authorization cache.

The same failing application profile was retained for successive comparisons,
with matching configuration, identity, endpoint and two connected peers. The
normal sampler kept its 90-second warmup, 60-second measurement and 5% average
CPU limit. Approximate results from the successive candidates were:

| Candidate | Average CPU | Result |
| --- | ---: | --- |
| Original shared libraries | 14.51% | Above limit |
| One-second service retry | 5.95% | Above limit |
| Three-second retry and authorization short-circuit | 5.44% | Above limit |
| Combined retry, authorization and allocation changes | 4.9444% | Within limit |

The final window contained 11 intervals spanning about 60.47 seconds; the raw
mean was 4.94440784794%, with 9.98% peak and about 0.056 percentage point of margin.
The counters were contiguous and independently recomputed. This is one passing
window with little headroom, using the application's test shell and optimized
Rust, not a Hashtree CLI measurement, a shipping-artifact gate, or a battery-life
result. No individual CPU saving is attributed to a component change. Every
final shipping executable still needs its ordinary release and resource gates.

Separate production-path tests on the development host measured item-vector
reservation, excluding packet buffers and allocator overhead: singleton crypto
runs fell from 101,376 bytes to 792 bytes; a full 128-item run remained 101,376
bytes. A nine-packet continuation grew to 16 items, or 12,672 bytes. Real crypto
results, sequence counters and plaintext checks passed with the bounded sizing.
These byte counts describe that host's layout, not a measured Android allocation
size. Borrowing peer-link metadata keys removes copied strings while preserving
selected order, duplicate-link metadata and fresh policy changes.

The shared EventBus default query deadline is 500 ms, shorter than repeated
service-attempt spacing. Hashtree's production query path defaults to 5,500 ms;
custom shorter deadlines or recovery near the deadline can still yield a partial
or empty response needing a later request. Existing retained subscriptions cover
ongoing delivery. Neither query deadlines nor the CPU/bandwidth budgets were
relaxed to obtain the result.

The final dependency plan also includes a later FIPS rekey correction: when
cached peer coordinates have been cleared, rekeying requests coordinates through
the existing bounded discovery path and resumes after verified coordinates return.
The Android CPU comparison above predates that correction; it does not measure
its CPU effect or replace the final shipping runtime's required resource gates.

## 2026-09-08 - Timed Cache Expiry and FUSE Callback Safety

The public generic timed cache accepts caller-defined key types. A deterministic
regression stored an expired key whose destructor panics once, caught that panic,
then exercised ordinary lookups, insertions and capacity eviction. Before the
correction, a later eviction failed inside the LRU library because its list was
left inconsistent. Returning an owned entry with `pop_entry` detaches the entry
before either key or value is dropped. The same regression passed afterward,
along with strict Clippy checks for the changed CLI library and formatting.
Normal daemon string and hash keys do not have panicking destructors; this
establishes the exposed generic API's panic-safety defect, not a demonstrated
remote-input exploit.

The optional FUSE path passed a null operations pointer and zero size to
libfuse3. The compatible carrier supplies a fully initialized callback table
using upstream PR #390 and retains the existing checked timestamp conversion
and API. With libfuse3 selected, 48 carrier library tests,
18 FUSE feature tests and the CLI's FUSE feature compilation passed. Those
focused checks did not mount a filesystem. The retained carrier mount test and
CLI mount smoke run together in the existing privileged release gate; the
ordinary unprivileged workspace test lane excludes that carrier.

## 2026-09-08 - Bounded Blob Provider Retry Fairness

A real TCP/FIPS fixture placed the requested bytes only on a fifth provider.
The old selector retried the same first four providers, even after four
unrelated blob reads, and returned a miss again. The corrected selector keeps
an independent retry offset for each retained hash and returns the verified
bytes on the next request. A search with untried candidates now reports an
incomplete result instead of a confirmed miss.

The per-request limit remains four providers and can be reduced by the caller's
attempt budget. Retry state retains at most 256 hashes with least-recently-used
eviction; an evicted hash starts at the first window again. Fairness applies
while the hash remains retained and the candidate set is stable. Discovery
ranking and deduplication still precede selection, and peer configuration is
read afresh without retaining revoked identities. Transport deadlines, hedging
and cancellation remain unchanged.

The actual failing retry assertion was reproduced before the correction.
Two real provider integration tests, 20 unit tests and strict all-target Clippy
checks passed afterward. Intermediate fixture teardown and cache-expectation
failures were corrected and excluded from the passing result. This establishes
bounded retry coverage, not a CPU or saturation improvement, and does not prove
the cause of a separately observed multi-peer missing-child timeout.

## 2026-09-07 - Reuse Verified Source CI During Release

[Source CI run 34151612943](https://github.com/mmalmi/hashtree/actions/runs/34151612943)
and [release run 34152908294](https://github.com/mmalmi/hashtree/actions/runs/34152908294)
tested the same commit. The release repeated six source lanes (static,
TypeScript, Rust, peripheral Rust, FIPS, and Pool migration systemd); those gates
added about 15m02 before artifact builds could begin. TypeScript CI runs the same
install, unit test, FIPS package verification (including build/dist checks), and
lint commands as its release gate. The other five lanes invoke the same gate
script in both workflows.

The release workflow now reuses successful push CI only for the exact release
commit, repository, CI workflow, and an expected CI branch, with all nine required
CI jobs successful in the same attempt. Missing or invalid evidence runs the
existing gates. Build checkouts stay pinned to that commit, and actual Unix and
Windows release-package startup checks still run on every artifact build.

This prospectively removes the repeated source testing and its prebuild wait;
no faster compilation or new end-to-end release timing has been measured.
Focused selector fixtures and selection against the completed source run verify
the evidence check without repeating the full suites.

## 2026-06-17 - Upload Benchmark Fresh Seed Controls

Question: can the upload benchmark avoid confusing fresh-write tests with
deterministic duplicate replays?

Finding:
- `upload_queue_bench` already generated deterministic payloads from a seed and
  per-item index, but the CLI help did not expose `--seed`.
- In batch modes, `--requests` is the total blob/item count, while the actual
  HTTP request count is `ceil(requests / batch-size)`. The old output did not
  make that distinction obvious, which made replication byte expectations easy
  to overestimate.

Change:
- Added `--fresh-seed` to generate a reproducible seed string for fresh writes;
  the benchmark prints the generated seed.
- Updated help text to document `--seed`, `--fresh-seed`, and the
  request-count semantics.
- Added `planned_http_requests` and `planned_mib` to benchmark output.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-blossom --example upload_queue_bench -- --nocapture`
- `cargo build --manifest-path rust/Cargo.toml -p hashtree-blossom --release --example upload_queue_bench`
- Smoke: `upload_queue_bench --server https://upload.iris.to --mode batch-binary --requests 4 --batch-size 2 --concurrency 1 --size 1024 --fresh-seed --timeout-secs 30 --upload-http1-only` printed `planned_http_requests=2`, a generated seed, and `success=4`.

Interpretation:
- This is measurement tooling, not a daemon throughput improvement. It should
  make future public-write, duplicate-write, and write-behind replication tests
  less error-prone.

## 2026-06-17 - Home Uplink vs Replica Downlink Direction Check

Question: if the deep replica sits behind a 1 Gbps home downlink, can public
upload slowness still be a network bottleneck?

Deployment:
- Deployed current master with upstream Blossom fetch counters to the public hot
  origin and deep replica.
- Both daemons reported `htree 0.2.69`; `htree status` on both showed the new
  `Upstream / Blossom fetch` counters.

Samples:

| Path / shape | Result |
| --- | ---: |
| Home client to public upload hostname, binary batch, 32 requests, b16, c4, 256 KiB | 7.77 MiB/s |
| Home client to deep replica over nVPN, same binary batch shape | 4.03 MiB/s |
| Home client to public hot-origin host over uncompressed SSH stream | 11.18 MiB/s / 93.8 Mbit/s |
| Public hot-origin host to home client over uncompressed SSH stream | 85.81 MiB/s / 719.8 Mbit/s |
| Hot-origin host to local hot-origin container, duplicate binary batch replay | 209.98 MiB/s |
| Hot-origin raw uploads, 256 x 263177-byte blobs, c16 | 10.84 MiB/s accepted |
| Same hot-origin raw run, write-behind replication drain to deep replica | 64.25 MiB in 1.299s, about 49 MiB/s |

Status after the samples:
- Hot-origin upload-replication queue was empty: 0 queued, 0 in flight, 0 failed,
  0 reserved bytes.
- Deep replica queues were empty and recent status counters had no 5xx.
- Hot-origin read-through counters showed indeterminate upstream misses from
  background/hash traffic, but no explicit-miss cache hits during the write
  samples; this was not the write bottleneck.

Interpretation:
- The 1 Gbps home downlink is not contradicted by the slow public upload sample:
  home-client uploads primarily depend on the home uplink and public route to
  the hot origin.
- The deep-replica receive path drained a fresh write-behind sample far faster
  than the public upload plateau, so the deep replica's home downlink was not
  the active bottleneck in this run.
- The remaining public write ceiling for this client is before local hashtree
  storage: likely home uplink, public routing, or edge/origin ingress behavior.

## 2026-06-17 - Upstream Blossom Read-Through Counters

Question: after adding an explicit upstream Blossom miss cache, can operators
tell whether read-through misses are still amplifying origin work?

Change:
- `/api/status` now reports `upstream.blossom_fetch` counters for lookup
  attempts, verified hits/bytes, explicit all-upstream 404 misses,
  indeterminate misses, and explicit-miss cache hits.
- `htree status` renders the same counters in its human-readable output.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib ensure_blob_available_ -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib daemon_status -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli test_daemon_status_formats_queue_and_http_counters -- --nocapture`
- `cargo fmt --manifest-path rust/Cargo.toml --all --check`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib -- --test-threads=8`

Interpretation:
- This does not move bandwidth by itself. It makes the next production check
  sharper: if public writes are slow but these counters are quiet, the culprit
  is still ingress/routing; if explicit misses or indeterminate misses climb,
  cold-read routing needs attention.

## 2026-06-16 - Upstream Blossom Explicit-Miss Cache

Question: even when public upload ingress is the largest bottleneck, are there
smaller origin-side bottlenecks worth fixing under load?

Finding:
- Live hot-origin writes from inside the origin network were already much
  faster than public-client uploads, so the write ceiling is still before local
  storage for the tested upload shape.
- Logs nevertheless showed repeated read-through misses for the same absent
  hashes. Those misses do not explain the public upload ceiling, but they waste
  upstream HTTP requests and can amplify tail latency during cold-read bursts.

Change:
- Added a short in-process cache for explicit upstream Blossom `404 Not Found`
  misses.
- The cache is intentionally narrow: it only records a miss when every queried
  Blossom upstream answers 404. Timeouts, 5xx, hash mismatches, and FIPS/WebRTC
  peer non-responses stay indeterminate and are retried, so slow peers are not
  converted into fake absence.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib ensure_blob_available_ -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib server::handlers::tests -- --nocapture`
- `cargo fmt --manifest-path rust/Cargo.toml --all --check`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib -- --nocapture`

Interpretation:
- This is not a bandwidth fix; it is an amplification and stability fix. Public
  writes still need ingress/routing improvements for major throughput gains.
- The next useful software targets remain bounded upload-replication backlog
  behavior, git remote traversal costs for duplicate-heavy pushes, and hot/cold
  storage placement for huge local imports or random cold reads.

## 2026-06-16 - Git Upload Traversal Skips Definite Leaf Blobs

Question: can git push upload discovery avoid spending CPU decrypting and
trying to decode every leaf Git object as a possible hashtree directory?

Finding:
- Process samples from a large git push showed visible time in decrypt/decode
  work while traversing upload candidates.
- The queue only carried hash/key pairs, so `git-remote-htree` could not tell a
  definite leaf blob from a directory/file root when deciding whether to decode.

Change:
- Upload traversal now queues link metadata alongside hash/key.
- Positive-size `Blob` links at or below the default content chunk size are
  treated as leaves and skip tree decode. Roots, directories, files,
  zero-size blobs, and oversized blob links still decode, preserving legacy
  ambiguous tree shapes.

Verification:
- `cargo test -p git-remote-htree --lib -- --test-threads=8`
- `cargo test -p git-remote-htree -- --test-threads=8`
- Focused tests cover both the decode policy and a raw blob whose bytes look
  like an encoded tree node.

Interpretation:
- This trims local CPU work during git upload discovery, especially for
  duplicate-heavy or encrypted pushes with many small Git object blobs.
- It does not raise the public upload bandwidth ceiling; public writes can
  still be ingress-limited before LMDB or local storage.

## 2026-06-16 - Current Hot-Origin Upload and Replica Status Check

Question: after the hot-tier, duplicate-aware write, and git upload changes, is
the active public write bottleneck still ingress, and can operators see whether
background upload replication is draining?

Current bounded samples:

| Path / shape | Result |
| --- | ---: |
| Public `upload.iris.to`, 64 x 256 KiB, binary batch 16, c4 | 4.27 MiB/s |
| Public `cdn.iris.to`, same write shape | 4.22 MiB/s |
| Same client direct to Osiris origin IP with upload hostname/SNI, same write shape | 8.01 MiB/s |
| Same client direct to Vader nVPN daemon, same write shape | 5.79 MiB/s |
| Osiris host to local htree container, 128 x 256 KiB, binary batch 16, c4 | 101.75 MiB/s |
| Vader host to local daemon with large production store, same 128 x 256 KiB shape | 193.12 MiB/s |
| CDN read of the fresh public upload, 64 x 256 KiB, c16 first pass / second pass | 32.42 / 41.89 MiB/s |

Change:
- `/api/status` now exposes upload-replication scheduler visibility in addition
  to the existing byte-reservation semaphore: coalescer queued jobs and limits,
  upload concurrency, in-flight replica batches, accepted batches/blobs/bytes,
  fallback batches, failures, and skipped jobs.
- `htree status` renders a compact upload-replication line so operators can spot
  backlog or failures without reading raw JSON.

Verification:
- `cargo test -p hashtree-cli test_daemon_status_formats_queue_and_http_counters -- --nocapture`
- `cargo test -p hashtree-cli daemon_status_exposes_mesh_alias_with_transport_metadata -- --nocapture`
- `cargo test -p hashtree-cli upload_replication_coalesces_adjacent_binary_batches -- --nocapture`
- `cargo test -p hashtree-cli --lib -- --test-threads=8`

Interpretation:
- Database size can still matter for cold metadata/random-read paths, eviction
  scans, page-cache misses, and legacy startup/write scans. The current accepted
  binary-batch write path, however, is not showing a large-store penalty: both
  Osiris local hot-origin writes and Vader local large-store writes are far
  above the public/client ingress rates.
- The public write ceiling remains before LMDB/local storage. The new status
  counters do not improve throughput directly; they make the hot-origin plus
  background-replica architecture safer to operate under load.

## 2026-06-16 - Tiered LMDB External Pack Roots for Hot Storage

Question: when a large hashtree store uses tiered LMDB, can new hot writes place
external pack files on a fast filesystem while legacy reads keep resolving old
pack files from the existing deep store?

Finding:
- The existing tiered LMDB abstraction could route new hash/index records to a
  hot primary store and read misses from a legacy store.
- External pack files were configured through one global
  `HTREE_LMDB_EXTERNAL_BLOB_DIR`, so moving only the hot tier to faster storage
  would either keep writing blob bytes to the slow path or make legacy pack
  markers resolve against the wrong directory.

Change:
- Added explicit external-blob options to `hashtree-lmdb` constructors.
- Added tier-scoped external pack directory overrides:
  `HTREE_LMDB_HOT_EXTERNAL_BLOB_DIR` for the primary hot tier and
  `HTREE_LMDB_LEGACY_EXTERNAL_BLOB_DIR` for the legacy tier. Without those
  overrides, existing global-env behavior remains unchanged.

Storage samples from one host, fresh temp stores, 1024 x 256 KiB batch writes:

| Shape | Result |
| --- | ---: |
| Fast local filesystem, external pack fsync on | 329.84 MiB/s |
| Fast local filesystem, external pack fsync off | 417.53 MiB/s |
| HDD mirror via zvol-backed filesystem, external pack fsync on | 54.51 MiB/s |
| HDD mirror via zvol-backed filesystem, external pack fsync off | 189.81 MiB/s |

Interpretation:
- The raw sequential write path was not the main issue; the same zvol stack could
  stream a simple 1 GiB file near 900 MB/s. The hashtree object pattern with
  external pack fsyncs showed the real gap.
- The safe near-term migration is not a full disk detach. First deploy
  tier-scoped external pack roots, put the hot primary and its packs on the fast
  filesystem, keep the legacy tier and old packs in place, then verify reads and
  writes before any cold-store disk migration.
- This keeps local filesystems as the hot path and does not add R2/S3/object
  storage.

## 2026-06-16 - Edge Nginx Connection Headroom and CDN Read Check

Question: is the current public reverse proxy leaving obvious read/write
performance on the table through low connection limits or uncached read paths?

Checks:
- Confirmed the public reverse proxy for the upload/CDN hostnames streams
  upload request bodies to the hot hashtree origin with request buffering off.
- Confirmed CDN hash reads use an nginx cache for content-addressed
  `/<sha256>.bin` paths.
- Raised the live reverse proxy worker file limit and connection capacity from
  the container default of 1024 to `worker_rlimit_nofile 65535` and
  `worker_connections 8192`, with `multi_accept on`. The config tested cleanly
  and nginx was reloaded in place.
- Verified new nginx workers had `Max open files` set to 65535. Existing old
  workers with the previous limit were only draining after reload.

Live samples:

| Path / shape | Result |
| --- | ---: |
| CDN read, first pass, 64 x 256 KiB, c16 | 30.63 MiB/s |
| CDN read, warm pass, same sample | 47.78 MiB/s |
| CDN read, post-reload warm smoke, same sample | 51.59 MiB/s |
| Public outside-client write, post-reload, 64 x 256 KiB, b16 c8 | 3.33 MiB/s |
| Hot-origin host through public hostname, post-reload write, same shape | 20.67 MiB/s |

Interpretation:
- The nginx connection tune is useful under websocket/upload/CDN load because it
  removes an avoidable low file-descriptor and worker-connection ceiling. It is
  not a public write throughput fix by itself; outside-client writes remained
  ingress/client-path bound after reload.
- CDN reads are healthy for this shape, especially once warmed. The remaining
  write gap is still before LMDB and before the hot-origin daemon's local write
  path.

## 2026-06-16 - Current Ingress and Replica Drain Check

Question: after duplicate-aware LMDB writes, binary batches, hot-origin storage,
and git pack admission fixes, where is the current write bottleneck?

Checks:
- Audited the current LMDB batch path. It already uses `NO_OVERWRITE`, reports
  exact inserted hashes/bytes, treats duplicate single puts as no-ops, and
  enforces logical quota from actual inserted bytes rather than candidate batch
  bytes.
- Ran bounded `upload_queue_bench` samples with 256 KiB blobs and 16
  blobs/request. Public outside-client duplicate replay was intentionally the
  same body upload again, so it still measures request-body ingress rather than
  duplicate insert speed.
- Checked hot-origin and replica service health. The old storage-write
  healthcheck timer on the deep store remained masked/inactive, and no active
  benchmark process was running there.
- Tuned the live hot-origin write-behind replica upload concurrency from 4 to
  8 after a direct c4/c8/c16 sweep showed c8 was the best tested point. The
  c16 sample raised tail latency and did not improve throughput.

Live samples:

| Path / shape | Result |
| --- | ---: |
| Public outside client, first write, 128 x 256 KiB, b16 c8 | 7.49 MiB/s |
| Public outside client, duplicate replay, same payloads | 7.66 MiB/s |
| Hot-origin host direct to daemon, same shape | 96.67 MiB/s |
| Hot-origin host through public hostname, same shape | 31.57 MiB/s before tuning; 19.36 MiB/s post-tuning smoke |
| Hot-origin host to configured replica, 64 x 256 KiB, b16 c4 | 4.07 MiB/s |
| Hot-origin host to configured replica, 64 x 256 KiB, b16 c8 | 5.56 MiB/s before tuning; 5.39 MiB/s post-tuning smoke |
| Hot-origin host to configured replica, 128 x 256 KiB, b16 c16 | 5.12 MiB/s |

Interpretation:
- The outside-client public write ceiling is still before LMDB. Direct daemon
  writes on the hot-origin host are an order of magnitude faster than the public
  path, and duplicate replay through the public path is not meaningfully faster
  because the full request body still has to cross the public ingress path.
- Nginx timing for the public binary batches showed request time and upstream
  time moving together, which is consistent with streaming the client body to
  the daemon as it arrives rather than spending extra time after upload in local
  storage.
- Replica write-behind is a separate drain-capacity concern. Concurrency 8 was
  the best tested point for the configured replica link, so it reduces sustained
  backlog risk compared with 4, but it does not change client-visible public
  upload ingress. This remains a local-fileserver/hot-origin architecture, not
  an R2/S3/bucket admission path.

## 2026-06-16 - Git Pack Admission Uses Actual Loose Upload Bytes

Question: when deciding whether to keep an underfull git pack/tail-pack
checkpoint, are we comparing against the bytes that would actually be uploaded
without the pack?

Finding:
- The admission check used `git cat-file --batch-check=%(objectsize)`, which
  counts uncompressed object content. Hashtree loose Git objects are uploaded as
  zlib-compressed loose objects, including the Git loose-object header.
- This could accept a pack+idx that looked smaller than raw content while not
  actually saving public upload bytes against the loose-object path.

Change:
- The git remote helper now accounts for loose-object upload bytes by reading
  objects with `git cat-file --batch`, recompressing the exact loose-object
  payload shape, and comparing pack+idx bytes against that total.
- Existing `put`/batch upload protocol shapes are unchanged. This is a
  git-aware byte/fanout decision, not a new network endpoint and not an
  R2/S3/object-store admission layer.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree
  git_loose_object_upload_bytes_counts_compressed_loose_bytes -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree underfull
  -- --nocapture --test-threads=1`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree
  git_pack_checkpoint -- --nocapture --test-threads=1`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree tail_pack
  -- --nocapture --test-threads=1`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree --lib
  -- --nocapture --test-threads=1`

Interpretation:
- This does not directly raise the public MiB/s ceiling. It prevents
  byte-negative pack decisions and makes medium git pushes more honest about
  whether a pack/tail-pack reduces the bytes and object fanout sent through the
  already saturated public upload path.

## 2026-06-16 - Framed Upload Stream Prototype Rejected

Question: can one framed, chunked `POST` carrying many binary blobs beat the
existing `/upload/batch-binary` shape enough to justify another write endpoint?

Prototype:
- Implemented a temporary `POST /upload/stream-binary` extension using a compact
  binary frame: magic, blob count, then repeated hash, content-type length, data
  length, content-type, and data bytes.
- The server read the Axum body incrementally, verified each declared hash, and
  flushed decoded blobs through the existing batch storage/report path.
- The client used a plain Blossom upload auth event with no `x` or `x-batch`
  hash tags, because validating a compact batch digest for an unknown-length
  stream would require buffering the whole body before accepting any blob.
- Focused client/server tests passed. The prototype was deployed only as a
  temporary hot-origin image, benchmarked, then rolled back and removed from the
  worktree because it did not materially move the bottleneck.

Live samples:

| Path / shape | Existing binary batch | Framed stream |
| --- | ---: | ---: |
| Outside client to public hostname, 1024 x 256 KiB, 16 blobs/request, c16 | 8.77 MiB/s | 9.39 MiB/s |
| Outside client to public hostname, one 64 MiB body | n/a | 7.27 MiB/s |
| Outside client to public hostname, four 16 MiB bodies, c4 | n/a | 7.47 MiB/s |
| Outside client to public hostname, 8 MiB bodies, c8 | n/a | 8.21 MiB/s |
| Hot-origin Docker network, 1024 x 256 KiB, 16 blobs/request, c16 | 106.92 MiB/s | 108.20 MiB/s |
| Hot-origin host through public hostname, 512 x 256 KiB, 16 blobs/request, c16 | 24.43 MiB/s | 24.64 MiB/s |

Interpretation:
- The generic framed stream route does not earn its extra protocol surface. It
  is effectively tied with binary batch at the origin and from the hot-origin
  host through the public hostname. The small outside-client gain was not a
  step-function improvement and did not unlock larger public request bodies.
- The active write ceiling is still outside LMDB and the local blob writer:
  hot-origin local writes are about 100+ MiB/s for this shape, while the public
  hostname is much lower and varies by client path.
- Do not re-add a generic framed upload endpoint just to reduce request count.
  Future write work should focus on real public ingress improvements, reducing
  bytes/object fanout in git pushes, or pack/tail-pack admission with a measured
  material win. This does not require R2/S3/object-store admission.

## 2026-06-16 - Coalesced Blossom Write-Behind Replication

Question: can the hot origin keep public writes bounded under load by sending
fewer, fatter write-behind requests to the configured replica, without adding an
R2/S3/object-store admission layer?

Change:
- Added a per-server Blossom upload replica scheduler. Accepted local writes
  still reserve the existing bounded replica queue bytes before body cloning.
- The scheduler holds those existing byte permits while it coalesces adjacent
  raw or binary-batch replication jobs with the same target/key configuration.
- Defaults are intentionally small: up to 64 blobs, up to 16 MiB, or a 25 ms
  flush delay. Operators can tune with
  `HTREE_BLOSSOM_REPLICA_COALESCE_MAX_BLOBS`,
  `HTREE_BLOSSOM_REPLICA_COALESCE_MAX_BYTES`,
  `HTREE_BLOSSOM_REPLICA_COALESCE_FLUSH_MS`, and
  `HTREE_BLOSSOM_REPLICA_COALESCE_QUEUE_JOBS`.
- If the coalescer queue is full, closed, or disabled, the old immediate
  write-behind upload path is used as the fallback.

Verification:
- `cargo fmt --manifest-path rust/Cargo.toml --all --check`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  server::blossom::tests::upload_replication_coalesces_adjacent_binary_batches -- --nocapture`
- Existing focused write-behind tests for binary-batch replication, raw
  replication, raw duplicate suppression, and optimistic duplicate suppression.
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  server::blossom::tests -- --nocapture`

Live deployment:
- Deployed non-S3 build `23ccb874` to the public hot origin.
- The hot origin stayed healthy. `/api/status` showed no recent 5xx during the
  live public write/read sweeps.

Live public samples:

| Shape | Result |
| --- | ---: |
| Public `upload.iris.to` write, 512 x 256 KiB, 16 blobs/request, c8 | 8.83 MiB/s, 0 failures |
| Same run, sampled replica queue reservation | peaked around 21 MiB of 512 MiB, then drained |
| Public `upload.iris.to` write, 1024 x 256 KiB, 16 blobs/request, c16 | 10.38 MiB/s, 0 failures |
| Same run, sampled replica queue reservation | peaked around 46 MiB of 512 MiB, then drained |
| Public `upload.iris.to` read of the fresh 1024-blob sample, c16 | 60.24 MiB/s, 0 failures |

Interpretation:
- Coalescing write-behind replication does not move the public request-body
  ingress ceiling by itself. The public write path is still around the previously
  measured 7-10 MiB/s band for 4 MiB binary batch bodies.
- It does improve the under-load shape behind the hot origin: the replica queue
  stayed shallow, bounded, and drained instead of accumulating an unbounded
  backlog while public writes were accepted.
- The remaining large write-performance win still needs a better public bulk
  ingress shape or fewer uploaded objects/bytes per operation, not LMDB
  queue-size tuning and not R2/S3/bucket admission.

## 2026-06-16 - Batch Blob Read-Through

Question: can cold immutable tree reads avoid one upstream HTTP GET per missing
chunk without adding S3/R2/bucket storage or a second source of truth?

Change:
- Added a small hashtree read extension: `POST /blob/batch`.
- The request is JSON containing content hashes. The response is a compact
  binary frame containing only local blobs the server already has.
- The hot origin verifies every returned blob hash before caching it. Missing,
  unsupported, truncated, or invalid batch responses fall back to the existing
  single-blob path.
- Cold file prefetch now batches unknown range-overlapping child blobs before
  the normal `ensure_blob_available` fallback. Cached batch fills use the
  existing `put_cached_blobs_report` batch insert path, so the read optimization
  does not become one LMDB write transaction per returned blob.

Verification:
- `cargo fmt --manifest-path rust/Cargo.toml --all --check`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  blob_batch_download_serves_present_hashes_in_binary_frame -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  get_cid_with_fetch_uses_upstream_blob_batch_for_missing_file_chunks -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  get_cid_with_fetch_prefetches_missing_file_chunks_concurrently -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  fetch_missing_chunk_coalesces_concurrent_upstream_fetches -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  server::handlers::tests -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib -- --nocapture`

Live deployment:
- Deployed non-S3 build `48fec5c0` to the public hot origin and the deep
  upstream store.
- The deep store's local `/blob/batch` probe returned the expected empty binary
  frame.
- The hot origin stayed healthy with no recent 5xx during cold-read and
  write-smoke traffic.

Live public samples:

| Shape | Before prefetch | Concurrent prefetch | Batch read-through |
| --- | ---: | ---: | ---: |
| 4 concurrent cold reads of one fresh 5 MiB / 64 KiB-chunk encrypted file | about 34 s | about 12.4 s | 0.61-0.64 s |
| Same file, immediate warm read | about 0.19 s | about 0.18 s | 0.18 s |

Write sanity after deploy:

| Shape | Result |
| --- | ---: |
| Public `upload.iris.to` binary batch write, 128 x 256 KiB, 16 blobs/request, c8 | 8.50 MiB/s, 0 failures |

Interpretation:
- Cold read-through fanout was the read bottleneck for this tree shape. Batching
  upstream blobs reduced the public cold read from seconds to sub-second without
  adding a cloud object store or changing local storage semantics.
- The remaining cold-read cost is mostly metadata/root fetches, public network
  latency, and the first upstream batch response, not LMDB.
- Public writes remain in the previously measured 7-10 MiB/s band for this
  shape. The batch read change did not move the write ceiling; that still needs
  a better ingress path or a long-lived/framed upload shape if the target is
  modern bulk-write throughput.

## 2026-06-16 - Cold Read-Through Chunk Prefetch

Question: why was a hot origin with a deep read-through source still slow on
cold immutable tree reads?

Finding:
- A fresh upstream-only encrypted tree with a 5 MiB file split into 64 KiB
  chunks exposed a read-through bottleneck. Four concurrent public reads through
  the hot origin took about 34 seconds before the file was cached, while the
  warm read immediately after took about 0.18 seconds.
- Reusing one upstream HTTP client per server state did not materially change
  this result, so per-blob `reqwest::Client` construction was not the dominant
  bottleneck.
- The actual hot path was one missing chunk at a time: after the file node was
  local, the server still discovered and fetched leaf blobs sequentially through
  `HashTree::get`/`read_file_range_cid` missing-chunk retries.

Change:
- Server state now carries a shared upstream Blossom HTTP client so cold-miss
  reads can reuse connections.
- Full-file and range reads now best-effort prefetch range-overlapping file
  leaf chunks concurrently after the file node is local. The normal exact
  missing-chunk retry loop remains as the fallback for internal nodes, failed
  prefetches, or unusual tree shapes.
- The default cold-read prefetch concurrency is 32 and can be capped with
  `HTREE_COLD_READ_PREFETCH_CONCURRENCY`.

Verification:
- `cargo fmt --manifest-path rust/Cargo.toml --all --check`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  get_cid_with_fetch_prefetches_missing_file_chunks_concurrently -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  fetch_missing_chunk_coalesces_concurrent_upstream_fetches -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib
  server::handlers::tests -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib -- --nocapture`

Live public samples:

| Shape | Before | Shared client only | Concurrent prefetch |
| --- | ---: | ---: | ---: |
| 4 concurrent cold reads of one fresh 5 MiB / 64 KiB-chunk file | about 34 s | about 33 s | about 12.4 s |
| Same file, immediate warm read | about 0.19 s | about 0.18 s | about 0.18 s |
| Fetch completion log count during cold pass | about 122 | about 120 | about 86 |

Interpretation:
- This is a real cold-read improvement and removes most duplicate/sequential
  leaf-fetch waste for this shape.
- It is still not modern cold-cache throughput for small-chunk trees. The
  remaining bottleneck is object fanout over the upstream read path, not LMDB.
- The next step for large cold reads should be a batch blob fetch/read-through
  endpoint or a pack/framed read shape, so the hot origin can fetch many missing
  blobs from the deep store in one request instead of one HTTP GET per leaf.

## 2026-06-16 - Compact Batch Upload Authorization

Question: why did larger binary batch uploads fail at the public edge even when
the body size was within the hashtree batch decoder limit?

Finding:
- Batch upload auth signed one `x` tag per blob hash. At 128 blobs, that made
  the Nostr Authorization header large enough for proxy/header limits to fail
  the request before the origin handled the body.
- Smaller 4-16 MiB batch bodies remained reliable, but the header shape made
  larger controlled experiments noisy and prevented using bigger request bodies
  to amortize public ingress overhead.

Change:
- Multi-blob batch upload auth now signs one `x-batch` tag containing a SHA-256
  digest of the ordered uploaded blob-hash list. Single-blob uploads keep the
  existing `x` tag shape for compatibility.
- The server recomputes each uploaded blob hash from the request body, hashes
  that ordered list, and rejects the batch if it does not match the signed
  `x-batch` digest. Duplicate or mismatched bodies are not accepted by this
  compact auth path.
- This is still a local hashtree/Blossom upload path. It does not add R2, S3,
  bucket admission, or a separate cloud storage tier.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-blossom
  compact_hash_list -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli
  compact_batch_auth -- --nocapture`
- Deployed a non-S3 hot-origin image built from the compact-auth commit range.
  Public health stayed OK and the daemon reported an idle write-behind queue
  after benchmark traffic.

Post-deploy public samples:

| Shape | Result |
| --- | ---: |
| 128 x 256 KiB, 128 blobs/request, c1 | edge 520 before a successful upload |
| 64 x 256 KiB, 64 blobs/request, c1 | 2.62 MiB/s |
| 128 x 256 KiB, 16 blobs/request, c8 | 7.59 MiB/s |
| Warm read of the 16-blob batch sample through CDN, c8 | 52.07 MiB/s |
| Warm read of the same sample through upload hostname, c8 | 48.38 MiB/s |

Interpretation:
- Compact auth keeps Authorization headers small and is deployed, but it does
  not by itself make 32 MiB public request bodies reliable through the current
  edge path.
- The practical write path remains the 4 MiB batch target. Larger 16 MiB bodies
  succeed but are slower in this sample. Public reads remain healthy once warm.
- To exceed the current 7-10 MiB/s public write band, the next step is still a
  better ingress path or a long-lived/framed upload shape, not LMDB tuning and
  not cloud bucket admission.

## 2026-06-16 - Public Host Versus Origin-Local Upload Ceiling

Question: after removing Worker/Tunnel upload handling from the active public
path, is the remaining write ceiling still local storage, Cloudflare edge
logic, or the client-to-public-origin network path?

Setup:
- `upload.iris.to` and `cdn.iris.to` resolved to Cloudflare edge addresses for
  a normal proxied hostname. The public host had nginx listening on 80/443 and
  no active `cloudflared` service units, so the active public request path was
  Cloudflare proxy to nginx to the hashtree daemon, not a Worker or Tunnel
  upload handler.
- Public samples used the release-built local benchmark client from outside
  the public host. Origin-local samples used a release-built benchmark client
  inside the public host's Docker network pointed directly at the hashtree
  daemon.
- Shape was 128 x 256 KiB blobs unless otherwise noted.

Results:

| Path | Shape | Result |
| --- | --- | ---: |
| Public `upload.iris.to` write | batch-binary, 16 blobs/request, c8 | 7.35 MiB/s |
| Public `upload.iris.to` write | batch-binary, 32 blobs/request, c8 | 6.42 MiB/s |
| Admin tunnel to daemon from same outside client | batch-binary, 16 blobs/request, c8 | 8.38 MiB/s |
| Public `cdn.iris.to` read, first pass | c16 | 25.81 MiB/s |
| Public `cdn.iris.to` read, warm pass | c16 | 36.95 MiB/s |
| Origin-local daemon write | batch-binary, 16 blobs/request, c8 | 94.23 MiB/s |
| Origin-local daemon read | c16 | 899.17 MiB/s |

Interpretation:
- The daemon, LMDB hot tier, packed external blob storage, and local Docker
  network are not the current public write bottleneck for this shape.
- The active public write ceiling is now outside the daemon: client-to-public
  host network path plus Cloudflare proxy/TLS/request-body handling. From the
  same outside client, bypassing Cloudflare with an admin tunnel was only
  modestly faster than the public hostname.
- Larger public request bodies still did not help; 32-blob batches were slower
  than 16-blob batches in this sample.
- Future code work should prioritize sending fewer bytes and fewer objects for
  git pushes, or a genuinely different long-lived/framed upload admission
  shape. Future deployment work should prioritize a hot origin with better
  public ingress bandwidth if bulk writes need to exceed the current WAN/edge
  ceiling. Do not add R2/S3/bucket admission for this.

## 2026-06-16 - Git Batch Retries Before Adaptive Split

Question: when `git-remote-htree` sees a transient public-edge batch upload
failure, should it immediately split the batch or retry the same efficient
request shape first?

Finding:
- Adaptive splitting recovered from persistent oversized request bodies, but it
  split a multi-blob batch after the first failed attempt. For public ingress
  where a one-off 520 can happen under load, that turns a transient hiccup into
  smaller follow-up batches and lowers effective throughput.

Change:
- Multi-blob batch uploads now get a short retry window before adaptive split:
  one retry of the same batch shape, then split only if the failure persists.
- Single-blob fallback still uses the existing fuller retry budget.
- Unsupported batch endpoints still fall back without retrying.

Verification:
- Added a fake Blossom edge test with one transient binary-batch failure. It now
  succeeds with exactly two batch requests: the failed original request and a
  successful retry of the same original batch, with no per-blob PUT fallback.
- Existing persistent oversized-body test still passes and proves adaptive
  splitting remains available.
- `cargo fmt --manifest-path rust/Cargo.toml --all --check`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree
  test_push_to_file_servers_with_diff_retries_transient_batch_before_split -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree
  test_push_to_file_servers_with_diff_splits_edge_rejected_batch_body -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree
  helper::tests -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree -- --nocapture`

## 2026-06-16 - Raw Duplicate Uploads Do Not Replicate

Question: after batch duplicate replication was fixed, do raw `PUT /upload`
duplicates still create avoidable write-behind queue pressure?

Findings:
- The raw upload handler prepared the replica queue reservation and cloned the
  request body before it knew whether storage had inserted a new blob. The
  explicit "already exists" branch also scheduled write-behind replication for
  duplicates.
- The live public endpoint had steady raw `PUT /upload` traffic from a known
  local crawler. That traffic is acceptable, but duplicate raw uploads should
  not consume replica queue bytes or send duplicate bodies to the deep replica.

Change:
- Single cached/owned Blossom storage now exposes an inserted flag while keeping
  the existing hash-returning API for older callers.
- Raw upload write-behind replication is prepared only after storage reports a
  newly inserted blob. Existing-blob responses still return `200 OK`, and newly
  inserted raw uploads still replicate.
- Duplicate raw/cached/owned writes no longer refresh blob access metadata by
  default. Same-owner duplicate ownership writes use LMDB `NO_OVERWRITE`; a
  different owner can still claim an existing owned blob explicitly.

Verification:
- `cargo fmt --check`
- `cargo test -p hashtree-cli --lib
  server::blossom::tests::upload_blob_replicates_to_configured_blossom_target -- --nocapture`
- `cargo test -p hashtree-cli --lib
  server::blossom::tests::upload_blob_duplicate_does_not_replicate_to_configured_blossom_target -- --nocapture`
- `cargo test -p hashtree-cli --lib
  server::blossom::tests::optimistic_upload_existing_blob_does_not_replicate_duplicate -- --nocapture`
- `cargo test -p hashtree-cli --lib
  storage::tests::duplicate_blossom_writes_do_not_refresh_blob_last_accessed -- --nocapture`
- `cargo test -p hashtree-lmdb --lib -- --nocapture`
- `cargo test -p hashtree-cli --lib`

Live deployment check:
- Deployed build `d85bb578` to the hot origin and deep replica.
- A small public first-pass raw upload probe wrote 64 x 64 KiB successfully
  at 2.43 MiB/s. Replaying the same payloads over the public endpoint returned
  faster at 7.07 MiB/s, but concurrent accepted uploader traffic made the
  public replica queue too noisy to use as a duplicate-only signal.
- A direct single-daemon probe through a temporary admin tunnel removed that
  noise. First pass wrote 32 x 64 KiB successfully at 2.03 MiB/s; replaying the
  exact same payloads returned at 5.53 MiB/s and left the replica queue at
  `reserved_bytes=0` with no recent 5xx.
- No bucket, R2, or S3 hot path was added or required; this change only removes
  duplicate local metadata work and duplicate write-behind replication.

## 2026-06-16 - Replicate Only Inserted Batch Blobs

Question: with the front node acting as the hot origin, is write-behind
replication doing unnecessary work for duplicate-heavy public pushes and retries?

Findings:
- The public path is no longer primarily a Worker/Tunnel problem. A temporary
  direct-origin probe from the same outside client reached about 8.56 MiB/s at
  c4 and 9.84 MiB/s at c8 for 16-blob binary batches, close to the
  Cloudflare-proxied 9.40 MiB/s best sample.
- Local hot-origin capacity is much higher than the public path: direct htree
  loopback reached about 99.80 MiB/s at c4 and 72.95 MiB/s at c8 for the same
  request shape. Local nginx-to-htree reached about 44.93 MiB/s at c4 and 78.48
  MiB/s at c8. So the remaining public write ceiling is before local storage
  and mostly before local nginx/daemon CPU.
- Driving the hot origin locally at high throughput can grow the bounded
  write-behind queue while the background replica link drains more slowly. That
  makes duplicate/retry replication waste important even though the queue does
  not grow unbounded.

Change:
- Blossom batch upload storage now returns the exact inserted-hash report to the
  upload handler. Write-behind replication is reserved and scheduled only after
  storage succeeds, only when `inserted_bytes > 0`, and only for hashes newly
  inserted by that batch.
- Duplicate-heavy batch retries still return accepted descriptors and the
  correct `uploaded` count, but duplicate candidates no longer clone bodies into
  the replica queue or send duplicate write-behind batches.

Verification:
- `cargo fmt --check`
- `cargo test -p hashtree-cli --lib
  server::blossom::tests::upload_blob_batch_binary -- --nocapture`
- `cargo test -p hashtree-cli --lib
  server::blossom::tests::upload_blob_replicates_to_configured_blossom_target -- --nocapture`
- `cargo test -p hashtree-cli --lib`
- Deployed a bookworm-compatible `htree` binary from commit `95e3c06d`
  (`93039881c78bd9c94666ef1096540d1b11ce07f21356ec917782621d9a1073a3`) to
  the hot origin and large replica.
- Live public duplicate test: first 64 x 256 KiB binary batch write reached 8.78
  MiB/s and queued about 12 MiB for write-behind before draining to zero. A
  duplicate replay did not queue the full 16 MiB candidate batch, and a second
  replay after drain reserved 0 replica queue bytes.

## 2026-06-16 - Public Write Sweep and Adaptive Git Batch Split

Question: after moving public writes to the hot origin with background
replication, what request shape is currently optimal, and how should
`git-remote-htree` behave when the public edge rejects an oversized upload body?

Findings:
- A fresh outside-client sample against `upload.iris.to` wrote 128 x 256 KiB
  blobs as 32-blob binary batches at 7.99 MiB/s. Reads of the same fresh blobs
  were 45.38 MiB/s through `upload.iris.to` and 43.82 MiB/s through
  `cdn.iris.to`, so the active limiter remains public write ingress rather than
  read serving.
- A bounded public write sweep found the current sweet spot around 16 blobs per
  request: 16-blob batches reached 6.79 MiB/s at c2, 9.09 MiB/s at c4, and
  9.40 MiB/s at c8. Larger 32- and 64-blob request bodies were lower or had
  worse tails, and 128-blob request bodies returned edge 520 responses before
  the origin reported any 5xx.
- The hot origin queue remained bounded and drained after the sweep; the
  write-behind queue reported zero reserved bytes once the run settled.

Change:
- `git-remote-htree` now adaptively splits a failed multi-blob batch upload
  into smaller batch requests instead of retrying the same large body shape
  until the push fails. Single-blob failures still use the existing bounded
  retry path. This keeps the normal 4 MiB git batch target, but makes accidental
  large-target overrides or edge body-size failures recover without falling
  back to per-blob PUTs.

Verification:
- `cargo test -p git-remote-htree
  test_push_to_file_servers_with_diff_splits_edge_rejected_batch_body -- --nocapture`
- `cargo test -p git-remote-htree test_push_to_file_servers_with_diff_ -- --nocapture`

## 2026-06-15 - Git Push Upload Batch Recovery

Question: why did public `htree` git pushes still spend minutes processing
already-present blobs after the origin supported batch upload checks?

Findings:
- The public upload Worker was returning 404 for `POST /upload/check` and
  `POST /upload/batch`, while the self-hosted origin already supported those
  endpoints. That forced helpers into per-object HEAD/PUT fallback.
- After deploying Worker passthrough for those two endpoints, public
  `/upload/check` returned the origin bitset response and `/upload/batch`
  returned origin validation instead of a Worker 404.
- A later live push showed the remaining failure mode: if a batch upload
  transiently failed, the helper treated that like "batch unsupported" and
  fell back to individual uploads, which can saturate the origin's small write
  queue and make agents wait through a slow per-object loop.

Change:
- `git-remote-htree` targets smaller upload batches for public edge-backed
  paths, retries batch upload failures with bounded exponential backoff, and
  only falls back to individual uploads when the server explicitly lacks the
  batch endpoint. A later binary-batch experiment lowered the default target to
  4 MiB and added `HTREE_GIT_BATCH_UPLOAD_TARGET_BYTES` for origin/local
  experiments that can benefit from larger bodies. A follow-up git helper fix
  kept the first batch as an endpoint-support probe, then uploaded remaining
  single-server batches concurrently using configured Blossom
  `upload_concurrency` instead of serializing every batch request.

Verification:
- Focused helper tests passed with `cargo test -p git-remote-htree
  helper::push::tests`.
- A live small-delta htree push using the rebuilt helper completed without the
  old long retry loop: it discovered a few hundred blobs, skipped most as
  already present via server inventory, uploaded a few dozen new blobs, and
  published successfully.

## 2026-06-15 - Public Blossom Origin/Edge Split

Question: after binary batch upload support, why are public writes still far
below local storage speed, and which part of the path is the limiter?

Findings:
- The disruptive healthcheck timer that had been stopping the daemon during
  uploads was masked, inactive, and no benchmark process was running. A
  30-second live sample showed modest traffic, idle queues, no 5xx responses,
  and low daemon/tunnel CPU, so the slow push behavior was not an active spam or
  DDoS event.
- The safe origin write queue winner for public-shaped binary batches was
  `HTREE_MAX_CONCURRENT_BLOB_WRITES=4`: loopback origin write throughput for
  256 x 256 KiB blobs in 16-blob batches was 21.5 MiB/s at limit 2,
  28.0 MiB/s at limit 4, and 25.5 MiB/s at limit 8.
- External blob fsync is a major local cost but not the public bottleneck. With
  the same origin workload and write limit 4, `HTREE_LMDB_EXTERNAL_BLOB_SYNC=1`
  reached 27.9 MiB/s; disabling it reached 47.7 MiB/s. Public writes through
  the edge/tunnel path only moved from about 9.7 MiB/s to about 10.1 MiB/s, so
  disabling fsync is not enough to make public uploads modern-fast and weakens
  the durability point.
- The public Worker was not the limiter for batch writes. Direct public writes
  to the tunnel-backed CDN hostname and writes through the upload Worker were
  both around 9-10 MiB/s for the best 4 MiB request shape.
- Public batch-size/concurrency sweep after the origin tuning still favored the
  existing public shape: 16 blobs per request at roughly 12 concurrent requests.
  Smaller batches created too many requests, while larger bodies and higher
  concurrency worsened tail latency.
- Cloudflared `http2-origin` worked functionally but slowed the write sample
  to about 8.2 MiB/s. Forcing IPv6 edge connections and leaving edge address
  selection automatic both selected IPv6 edge connections and were slower than
  the current IPv4 setting. The tunnel was restored to IPv4 QUIC.
- Public reads are not LMDB-limited. A hot loopback origin read sample was over
  1 GiB/s, and a single public client read sample was roughly 55-60 MiB/s at
  8-32 concurrent reads. An earlier very slow read result was caused by running
  two independent 64-way public read clients at once.

Interpretation:
- LMDB is not the primary remaining problem on the measured hot paths. Large
  blobs are stored outside LMDB, with LMDB holding markers/metadata; origin
  writes are mostly external pack file writes plus fsync and one LMDB
  transaction.
- Public writes are currently bounded by the Cloudflare/tunnel request-body
  path. Origin storage tuning helps localhost and private/direct writers, but
  only leaks through as a few percent at the public endpoint.
- The live origin should keep `HTREE_MAX_CONCURRENT_BLOB_WRITES=4` with
  external blob fsync enabled until a separate durability design replaces
  per-request fsync.

Follow-up:
- To get public writes to 50+ MiB/s, change architecture rather than only
  tuning LMDB: either expose a direct public origin path with modern TLS/HTTP
  transport, or make the Worker an edge-admission layer that stores batch blobs
  into the same read-visible edge/cache path and asynchronously syncs the
  hashtree origin. Edge admission must also answer `/upload/check` consistently
  from the edge store; otherwise git pushes can publish roots before the
  canonical read path sees the blobs.
- For trusted internal writers, prefer direct/private origin routes with larger
  `HTREE_GIT_BATCH_UPLOAD_TARGET_BYTES` values when available. Do not add a
  second write server to config without preserving the helper's binary batch
  path for multi-server write configurations.

## 2026-06-16 - Plain Cloudflare Proxy Via Datacenter Reverse Proxy

Question: can public `upload.iris.to`/`cdn.iris.to` drop the Worker and Tunnel
hot path by using plain Cloudflare proxy/cache in front of a datacenter reverse
proxy that forwards to the deep storage origin over a private mesh link?

Setup:
- Retired an unused upload Worker hostname and confirmed the active
  `upload.iris.to` and `cdn.iris.to` hostnames had no Worker custom-domain
  bindings.
- Confirmed the Cloudflare account had no R2 buckets, so the active path is not
  using Cloudflare object storage.
- Configured a datacenter nginx origin with a 60 GiB immutable-blob cache for the
  CDN hostname, unbuffered upload proxying for the upload hostname, and upstream
  forwarding over a private mesh link to the deep storage origin.
- Changed `upload.iris.to` and `cdn.iris.to` from proxied Tunnel CNAME records to
  proxied direct origin records. Public hostnames are retained; identifying
  origin IPs, private hostnames, and raw hashes are omitted.

Verification:
- Forced-origin probes returned the hashtree UI through the upload and CDN
  hostnames, `POST /upload/check` returned the expected empty inventory response,
  and the CDN hostname preserved the extensionless 308 redirect to `/<hash>.bin`.
- After DNS cutover, public `GET /`, `GET /upload/check`, CDN extensionless
  redirect, and missing `.bin` lookup all reached the new path.
- A small public binary-batch write smoke through `upload.iris.to` succeeded
  with 128 x 256 KiB blobs at c4: 3.21 MiB/s, p95 4.66 s.
- A c12 public binary-batch write smoke succeeded with 256 x 256 KiB blobs:
  4.23 MiB/s, p95 12.07 s.
- Direct private-link writes from the datacenter proxy host to the deep storage
  origin were only modestly faster: c4 reached 5.27 MiB/s, c12 reached
  4.63 MiB/s, and 64-blob batches at c4 reached 5.51 MiB/s.
- The existing local datacenter hashtree container is not ready to become the
  hot origin: it is older than the current binary-batch server, rejects the
  binary batch path, and the JSON batch probe stalled long enough to abort.

Interpretation:
- Plain Cloudflare proxy/cache plus nginx is operationally simpler than the
  Worker/Tunnel write path, but a pure forwarding proxy does not reach modern
  write throughput because every upload body still waits on the private mesh hop
  to the deep storage origin.
- The next performance step is not another Worker or an R2 hot cache. It is a
  current datacenter hot hashtree/Blossom origin with bounded local disk use,
  read-after-write serving from the hot store, and background replication to the
  deep storage host.
- Until that hot-origin design exists, public writes remain transport-bound even
  though origin storage and LMDB are no longer the main limiter.

## 2026-06-15 - Blossom Upload Write Queue Limit

Question: is a `HTREE_MAX_CONCURRENT_BLOB_WRITES` value of 4 optimal for raw
Blossom `PUT /upload` traffic on the self-hosted origin?

Setup:
- Benchmarked the origin over an SSH loopback tunnel so public Worker and WAN
  latency did not dominate the result.
- Each run used generated upload keys, unique deterministic 256 KiB
  encrypted-looking bodies, public-write cache storage, and fixed client
  concurrency above the tested server write limit.
- Exact hostnames, pubkeys, raw hashes, private repo names, and IPs are omitted.

Results:

| Server write limit | Workload | Result |
| --- | --- | --- |
| 1 | 16 uploads, concurrency 16 | 31.94 s wall, 0.13 MiB/s, p95 31.87 s |
| 2 | 16 uploads, concurrency 16 | 25.99 s wall, 0.15 MiB/s, p95 25.92 s |
| 4 | 16 uploads, concurrency 16 | 38.94 s wall, 0.10 MiB/s, p95 38.85 s |
| 8 | 16 uploads, concurrency 16 | 0 successes; clients failed after about 111.6 s |
| 2 | 24 uploads, concurrency 24 | 19.04 s wall, 0.32 MiB/s, p95 18.00 s |
| 3 | 24 uploads, concurrency 24 | 28.21 s wall, 0.21 MiB/s, p95 27.30 s |
| 4 | 24 uploads, concurrency 24 | 28.17 s wall, 0.21 MiB/s, p95 27.40 s |

Interpretation:
- Higher raw write concurrency did not improve the origin. The high side
  saturated storage, hurt status responsiveness, and at 8 caused upload clients
  to fail.
- The best longer same-pass result was limit 2. Shorter 2/3/4 samples were
  noisy, but 2 gave the best throughput and tail latency once the sample was
  long enough to smooth warmup variation.
- The raw write limit mainly protects fallback/single-upload traffic. Git pushes
  should normally use `/upload/batch`; the server now gates batch storage with
  the same write limiter and exposes a write queue timeout so batch uploads get
  bounded retryable backpressure instead of bypassing the limiter.

Change:
- Set the live origin override to `HTREE_MAX_CONCURRENT_BLOB_WRITES=2`.
- Added a reusable Blossom upload queue benchmark example.
- Added `HTREE_BLOB_WRITE_QUEUE_TIMEOUT_MS` with a 30 s default, surfaced it in
  `/api/status`, and made `/upload/batch` acquire the blob write permit.

Verification:
- Linux and local `hashtree-cli` Blossom/status tests passed.
- The live origin reported `blob_writes.limit=2` and
  `blob_writes.queue_timeout_ms=30000`.
- A post-deploy smoke upload over origin loopback succeeded, `/upload/check`
  returned an empty inventory response, and `/upload/batch` reached the origin
  route and returned validation for an empty batch.

## 2026-06-07 - Git Clone Blossom Download Concurrency

Question: does a larger `git-remote-htree` loose-object Blossom download concurrency make a clean-cache public clone faster through the Iris Blossom read path?

Setup:
- Each cold run used a fresh hashtree config directory, fresh LMDB data directory, and fresh Git clone destination.
- Reads were pinned to the Iris CDN plus upload Blossom endpoints unless noted otherwise.
- The remote was a public Git repository published through hashtree. Pubkey, exact repo name, raw object hashes, and temp paths are omitted.
- The helper printed verbose fetch-stage timings. The published tree had no usable Git pack checkpoint, so clone fetched roughly 21k loose Git objects.

Results:

| Variant | Read path | Object download concurrency | Enumerate | Local check | Download + write | Wall |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Before change | CDN + upload | 20 | 5.38 s | 0.52 s | 170.62 s | not captured |
| Tuned low | CDN + upload | 32 | 4.35 s | 0.54 s | 138.68 s | 153.02 s |
| Tuned default | CDN + upload | 64 | 7.21 s | 0.56 s | 131.76 s | 150.15 s |
| Too high | CDN + upload | 128 | 25.48 s | 0.57 s | 131.21 s | 166.55 s |
| Origin only | upload only | 64 | 5.13 s | 0.58 s | 164.98 s | 181.69 s |
| Warm LMDB baseline | local cache | 20 | 0.79 s | 0.56 s | 30.41 s | 41.00 s |
| Warm LMDB writer experiment | local cache | 64 downloads, 4 writers | 0.88 s | 0.52 s | 36.11 s | 48.16 s |
| Pack-backed root | CDN + upload | 1 Git pack | 0.87 s | ~0 s | 43.87 s pack install | 54.40 s |
| Pack-backed delta root | CDN + upload | 1 Git pack + 30 loose objects | 9.33 s | 0.02 s | 21.94 s pack install + 0.30 s loose write | 41.67 s |
| Pack-only current root | CDN + upload | 1 Git pack, 0 loose objects | 0.80 s | ~0 s | 29.37 s pack install | 39.61 s |
| Corrected force-pushed checkpoint root | CDN + upload | 1 Git pack + 667 loose objects | 7.49 s | 0.02 s | 36.29 s pack install + 4.04 s loose write | 57.33 s |

Interpretation:
- Raising loose-object download concurrency from 20 to 64 reduced the cold `download + write` stage by about 39 seconds, roughly 23%.
- 128 concurrent object downloads did not improve the transfer stage and made the run slower overall, so the useful range on this path was around 32-64.
- Upload-only reads were slower than the CDN plus upload path in this run, so the CDN leg was useful rather than an obvious miss penalty.
- The tuned default now keeps 64 for multi-server CDN-style read paths but drops to 16 when there is only one read server or a loopback local daemon is first. That avoids making a direct Blossom origin fight a 64-request burst when its default blob-read limit is lower.
- The warm-cache run shows a roughly 30 second floor from local LMDB reads plus writing many loose Git objects into `.git/objects`.
- Parallelizing the local loose-object writer made the warm-cache case slower, so the single-writer path stayed in place.
- After publishing a pack-backed root, a clean-cache clone installed one Git pack, wrote zero loose objects, and `git count-objects` reported 0 loose objects and about 21k packed objects.
- After a later small delta push, a clean-cache clone still installed the checkpoint pack but also enumerated roughly 1.9k current-root entries, prepared about 1.6k Git object mappings, wrote 30 loose objects, and reported about 21k packed objects. The remaining cold-clone cost was dominated by pack transfer/install plus root enumeration, not loose historical object download.
- After tightening pack-backed delta pushes to avoid loose pack-covered blobs, a clean-cache clone of the current root enumerated only the pack metadata, prepared zero loose objects, wrote zero loose objects, and reported about 21k packed objects.
- A later corrective force push replaced a bad intermediate root and produced a checkpoint-style root with 667 loose recent objects plus one pack. The clean-cache clone succeeded and reported 667 loose objects, roughly 20k packed objects, and one pack. The failed intermediate root had treated unchanged trees that existed only as loose objects in the base root as if they were covered by the inherited pack; filtering inherited pack-covered candidates against the cached base root's loose `.git/objects` entries fixed that missing-tree failure.

Follow-up:
- A Git pack checkpoint would likely beat loose-object tuning for initial clone, because it would replace about 21k small object fetches and writes with a small number of large sequential artifacts.
- If multi-server behavior becomes a bottleneck on less-cached content, test hedged per-object reads across read servers rather than sequential server fallback.
- Push-side follow-up: pack-backed delta pushes now avoid re-importing unchanged current blobs as loose Git objects when the inherited checkpoint pack already covers them, while still importing current tree objects needed to rebuild the browsable view. The old-tree coverage probe also ignores the previous root blob itself, because that root is not referenced by the new root; a missing previous-root blob alone should not force a full old-tree reupload walk. Future work should still make old-tree coverage proofs cheaper and more reliable, especially for fresh installs and degraded Blossom coverage probes.
- Normal no-op push after the corrected root returned immediately as already up to date. Future non-force delta pushes should be measured separately from corrective force pushes, since force pushes intentionally bypass the normal checkpoint delta base and can publish a larger recent-loose-object frontier.

## 2026-05-18 - Blossom Upload Proxy Baseline

Question: after moving the public upload path from direct Worker/R2 writes to a Worker proxying a self-hosted hashtree origin, how much latency is visible to push clients?

Setup:
- Client generated a throwaway signing key for the run.
- Bodies were random encrypted-looking bytes.
- Target was the public upload Worker endpoint, through its configured origin proxy.
- Run used 12 concurrent `PUT /upload` requests per body size.
- No pubkeys, auth events, IPs, or content hashes were retained.

Results:

| Body size | Responses | Total wall time | Min | P50 | P95 | Max |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 KiB | 12 x 202 | 919 ms | 260 ms | 524 ms | 881 ms | 881 ms |
| 64 KiB | 12 x 202 | 698 ms | 243 ms | 476 ms | 676 ms | 676 ms |

Interpretation:
- The endpoint already returned `202 Accepted`, but the Worker waited for the origin server to accept the upload before responding.
- The likely client-visible cost was the Worker-to-origin hop plus origin validation and LMDB existence preflight, not the final LMDB body write.
- A safer optimization is to keep the origin as the backpressure point, acquire the bounded optimistic upload queue before responding, and avoid the LMDB existence preflight on the queued optimistic path.
- A Worker-only fire-and-forget mode should remain experimental because it can hide origin queue-full/storage failures from the pushing client.

Follow-up after moving the LMDB existence preflight behind bounded optimistic queue admission:

| Body size | Responses | Total wall time | Min | P50 | P95 | Max |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 KiB | 12 x 202 | 251 ms | 107 ms | 139 ms | 210 ms | 210 ms |
| 64 KiB | 12 x 202 | 178 ms | 112 ms | 134 ms | 157 ms | 157 ms |

Result:
- The p50 response time dropped from 524 ms to 139 ms for 1 KiB bodies and from 476 ms to 134 ms for 64 KiB bodies.
- The endpoint still answered only after the origin accepted each body into the bounded optimistic upload queue.
- A post-Worker-deploy smoke pass with 12 concurrent 1 KiB uploads returned 12 x 202 in 202 ms total, with 146 ms min, 148 ms p50, 168 ms p95, and 168 ms max.

## 2026-05-18 - Git Remote No-Op Push Rebuild

Question: after the upload admission fix, is a no-change `git push` to a hashtree remote reasonably fast?

Setup:
- Local repository was already at the same branch tip as the hashtree remote.
- Push used the normal public hashtree remote helper path.
- No pubkeys, relay account data, IPs, raw hashes, or exact repository names were retained.

Results:

| State | Total wall time | Local object walk | Upload result |
| --- | ---: | ---: | --- |
| Before helper fix | 133.91 s | Full rebuild of 19,342 Git objects | 5 new hashtree blobs |
| Repeat before helper fix | 19.86 s | Full rebuild of 19,342 Git objects | 3 new hashtree blobs |
| After helper fix | 4.75 s | No rebuild; remote refs/root only | No repo tree/blob upload |

Interpretation:
- The slow no-op pushes were not caused by the Blossom upload origin after the server fix.
- The remote helper was rebuilding the local repo tree because it had an unchanged pushed branch but preserved direct refs that pointed at objects not loaded into the in-memory tree.
- Exact no-op branch pushes now return before local object listing and repo-tree rebuild. Force pushes still take the normal publish path.

## 2026-05-23 - Bulk Artwork Thumbnail Repair

Question: when a large public media catalog already stores full-size artwork in hashtree, what bottlenecks appear while backfilling small thumbnails into the same catalog?

Setup:
- The repair walked roughly 160k song entries and generated thumbnails for album covers plus artist photos/logos.
- Existing media was read from a local hashtree daemon by content hash whenever the old catalog URL pointed at the same object store through an HTTP gateway.
- Generated catalog metadata stored thumbnail references as `htree://nhash1.../filename-thumb.ext`.
- No hostnames, pubkeys, raw hashes, catalog identifiers, or exact remote names were retained.

Findings:
- Image resizing was not the main limiter. The local image tool usually spent tens of milliseconds per thumbnail, while storage reads/writes dominated wall time.
- Running thumbnail generation on all CPU cores made throughput worse once the backing store saturated. The useful concurrency limit was set by random read/write latency, not by CPU availability.
- Writing each thumbnail as a one-file hashtree directory doubled the tiny-object write path: one blob for the image and one blob for the directory node. For immutable image thumbnails, storing the thumbnail as the root file CID and keeping the filename in the URL path removed that extra directory write while preserving htree URLs and browser content-type hints.
- Direct local hashtree reads avoided external gateway/CDN fetches, but turned the repair into a cold random-read workload against the local object store.
- Disabling background access-time updates during the bulk pass avoided extra metadata write amplification. That setting is a bulk-maintenance throttle, not a normal serving preference.
- Disabling peer fallback reads during the bulk pass made misses and slow reads fail locally instead of creating extra network/storage work. That should be restored for normal daemon operation when the maintenance pass is done.
- The UI-side thumbnail check should remain metadata-only: prefer `*ThumbnailUrl` fields and never probe media availability just to decide whether a row/circle has a thumbnail.
- Temporary image files should be created per image and removed in `finally`; a stale-startup cleanup is useful, but a persistent thumbnail temp cache is not appropriate for tens of thousands of albums.

Interpretation:
- High disk utilization is partly expected for this workload: a large sparse content-addressed store plus copy-on-write/block-device layers turns a full catalog backfill into many cold random reads and small durable writes.
- Some of the pressure was avoidable and was fixed by reducing write amplification: batch uploads, batched owner-index writes, capped access updates, and raw-file thumbnail URLs.
- Remaining addressable work is mostly data-shape work, not more CPU parallelism: avoid rewriting song directories whose only change could live in a compact side index, improve locality of repair ordering where practical, and keep bulk repair concurrency below the point where storage queues grow.
- Optimistic Blossom upload admission is a separate serving/write-latency tradeoff. It can reduce client-visible latency for small uploads, but it should not be enabled or disabled as part of a bulk catalog repair without a separate burst-write test, because it changes when clients receive success relative to durable storage.

## 2026-06-08 - Git Pack Release Clone And Push

Question: after the pack-backed git remote fixes and release, what are clean
clone and small incremental push timings for a large public project repository?

Setup:
- Client used `git-remote-htree` 0.2.59.
- Clone used a brand-new helper data directory and a fresh destination, so no
  existing LMDB, helper cache, or local Git object cache was reused.
- Push measurement used a small documentation-only commit on the same repository.
- No pubkeys, private hostnames, exact repo names, raw hashes, or IPs were
  retained.

Fresh clone result:

| Metric | Result |
| --- | ---: |
| Total wall time | 11.08 s |
| Object-tree enumeration | 4.42 s |
| Pack download and write | 2.87 s |
| Installed pack count | 1 |
| Installed pack payload | 77.8 MiB |
| Git objects in installed pack | 20,478 |
| Loose current/checkpoint objects written | 687 |

Interpretation:
- Fresh clone used the pack checkpoint path: one ordinary Git pack carried the
  historical object set, with a small loose-object frontier for current refs and
  helper-visible metadata.
- The remaining clone time was split between tree enumeration/resolution and
  pack transfer/install; per-object loose download was no longer the dominant
  cost.

## 2026-06-08 - Deterministic Git Pack Checkpoint Chain

Question: after replacing overlapping full checkpoint packs with deterministic
checkpoint pack ranges, what are the publish and fresh-clone costs for the same
large public project repository?

Setup:
- Client used `git-remote-htree` 0.2.60 built from the release-prep checkout.
- The current branch was force-published once so the mutable root no longer
  inherited the previous single full-pack checkpoint.
- Clone measurements used brand-new helper data directories and fresh
  destinations, with no existing LMDB, helper cache, or local Git object cache.
- No pubkeys, private hostnames, exact repo names, raw hashes, temp paths, or
  IPs were retained.

Force publish result:

| Metric | Result |
| --- | ---: |
| Total wall time | 66.29 s |
| Reachable Git objects listed | 21,190 |
| Checkpoint files built | 10 |
| Checkpoint pack ranges | 5 |
| Checkpoint payload plus indexes | 87.45 MiB |
| Loose/current Git objects imported | 2,154 |
| New hashtree blobs uploaded | 128 |

Fresh clone results:

| Cache state | Total wall time | Object-tree enumeration | Pack install | Download and loose write | Installed packs | Packed Git objects | Loose frontier objects |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Cold just-published blobs | 47.75 s | 3.64 s | 12.29 s | 30.59 s | 5 | 20,599 | 713 |
| Warm repeat | 15.46 s | 2.28 s | 3.08 s | 5.50 s | 5 | 20,599 | 713 |
| Warm current root after two small delta pushes | 9.74 s | 2.37 s | 2.96 s | 3.43 s | 5 | 20,599 | 721 |

Incremental push result:

| Metric | Result |
| --- | ---: |
| Total wall time | 12.90 s |
| Git object delta listed | 4 |
| Local objects read for frontier/tree merge | 257 |
| Cached-root merge output | 66 object blobs, 21 files, 8 dirs, 27 reused |
| New hashtree blobs uploaded | 21 |

Interpretation:
- The new root used five deterministic checkpoint pack ranges instead of one
  full pack. A later publisher that reaches the same earlier checkpoint tips
  can reuse those earlier pack blobs instead of publishing the same history in a
  different full-pack blob.
- Fresh clone still installs ordinary Git packs and avoids loose historical
  object download, but it now pays per-pack overhead. Warm-cache clone remained
  close to the earlier single-pack result; the first clone after publishing was
  dominated by cold CDN/origin transfer for the newly uploaded pack blobs.
- The largest remaining clone bottleneck is the large checkpoint range in the
  current history shape, not many tiny loose-object downloads. More even
  object-level pack slicing could reduce that, but commit-boundary checkpoints
  are simpler and preserve deterministic reuse across publishers.
- Small pushes after the chained checkpoint root stayed on the cached-root delta
  path and did not rebuild or reupload the historical pack ranges.

## 2026-06-08 - v0.2.61 Clone Recovery Verification

Question: after adding daemon-root validation and relay retry, does a clean
fresh clone of the same large public project repository still use the current
checkpoint pack arrangement and avoid the decryption failure path seen with a
stale local root/key cache?

Setup:
- Client used source-built `git-remote-htree` 0.2.61.
- Clone measurements used brand-new helper data directories and fresh
  destinations.
- One run disabled local-daemon preference to isolate the relay/CDN path; one
  run used the default helper configuration.
- No pubkeys, private hostnames, exact repo names, raw hashes, temp paths, or
  IPs were retained.

Fresh clone verification:

| Mode | Total wall time | Object-tree enumeration | Pack install | Download and loose write | Installed packs | Loose frontier objects |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Relay/CDN path only | 12.88 s | 2.62 s | 3.83 s | 5.03 s | 5 | 726 |
| Default helper path | 10.36 s | 2.48 s | 3.12 s | 3.76 s | 5 | 726 |

Interpretation:
- Both fresh clones succeeded at the same current branch tip and installed the
  five deterministic checkpoint packs from the v0.2.60 pack-chain root.
- The local-daemon recovery fix is covered separately by a fake-daemon
  regression: stale daemon roots are discarded before caching, and ref loading
  retries through relays.
- A corrupted `.git/objects` subtree key now fails the object-tree load loudly
  instead of returning an empty object set to Git.

## 2026-06-08 - Multi-Server Clone Frontier Bottleneck

Question: after the stale-helper cleanup on a remote Linux host, why are clean
fresh clones still slower there than the best local repeat measurements, and
which loose-object concurrency default fits the current checkpoint-pack layout?

Setup:
- The host had stale `htree` and `git-remote-htree` binaries earlier in PATH;
  cleanup left the Cargo-installed 0.2.61 binaries as the canonical commands.
- Clone runs used fresh helper data directories and fresh destinations.
- Default runs used a local daemon read path plus remote read servers. One run
  disabled local-daemon preference to isolate the relay/CDN path.
- No pubkeys, private hostnames, exact repo names, raw hashes, temp paths, or
  IPs were retained.

Fresh clone results:

| Mode | Total wall time | Object-tree enumeration | Pack install | Download and loose write | Installed packs |
| --- | ---: | ---: | ---: | ---: | ---: |
| Default, first cold-ish remote-host run | 39.17 s | 3.64 s | not isolated | 30.25 s | 5 |
| Default, warmed repeat | 16.14 s | 4.46 s | 2.59 s | 8.36 s | 5 |
| Relay/CDN path only, warmed repeat | 20.27 s | 4.46 s | 2.61 s | 12.51 s | 5 |
| Source-built 0.2.62 local smoke | 105.86 s | 7.68 s | 53.48 s | 33.94 s | 5 |

Current loose frontier inspection:

| Object type | Loose count | Payload size |
| --- | ---: | ---: |
| commit | 67 | 14.2 KiB |
| tag | 2 | 0.2 KiB |
| tree | 403 | 184.3 KiB |
| blob | 281 | 12.4 MiB |
| total | 753 | 12.6 MiB |

Concurrency sweep on the default remote-host read path:

| Loose-object concurrency | Total wall time | Object-tree enumeration | Pack install | Download and loose write |
| ---: | ---: | ---: | ---: | ---: |
| 16 | 15.97 s | 2.67 s | 2.64 s | 9.89 s |
| 32 | 41.37 s | 2.53 s | 2.67 s | 32.61 s |
| 64 | 12.38 s | 2.64 s | 2.86 s | 6.15 s |
| 96 | 18.11 s | 5.36 s | 2.61 s | 9.44 s |
| 128 | 73.05 s | 32.66 s | 32.97 s | 6.67 s |

Interpretation:
- The installed root used the five deterministic checkpoint packs from the
  current pack-chain arrangement. The helper was not using a current-tip or
  tail pack.
- Remaining clone cost came from the loose current/checkpoint frontier plus
  per-pack installation, not from re-fetching all historical Git objects.
- A single direct Blossom server should stay conservative because origin
  throttling or lower server-side concurrency can make large bursts harmful.
- The faster default for this shape is to treat two or more read servers as a
  multi-server path even when the first read server is a loopback daemon. That
  keeps direct/single-server clones at 16 while allowing local-daemon plus
  CDN/origin fallback clones to use the 64-concurrent loose-object path.
- The source-built 0.2.62 smoke clone was kept as a correctness check because
  it succeeded at the same current root with five checkpoint packs and 753
  loose frontier objects. Its wall time was dominated by slow pack transfer and
  install variance, so it was not used to choose the concurrency default.

Follow-up:
- After a release tag push, a fresh remote-host clone of the mutable root no
  longer saw checkpoint pack metadata. It enumerated about 20.6k object-tree
  entries, prepared about 20.4k loose objects, spent 50.33 s enumerating and
  199.22 s downloading/writing loose objects, then failed because an expected
  Git object was still missing. The bad root shape came from treating a new tag
  ref with no previous tag tip as a full-repository push instead of using the
  existing remote branch tip as a delta base. The 0.2.63 fix makes new tag/ref
  pushes reuse an existing remote ref when it is an ancestor of the pushed ref,
  and rebuilds missing checkpoint roots from deterministic checkpoint
  boundaries without adding a current-tip tail pack.
- After repairing the mutable root with the 0.2.63 helper, a fresh remote-host
  clone again installed five checkpoint packs. The run enumerated in 2.94 s,
  installed packs in 2.76 s, spent 10.26 s in download/write, wrote 797 loose
  frontier objects, and completed in 16.83 s wall time.
- After installing the published 0.2.63 crates and restarting the remote-host
  daemon, another fresh-datadir clone succeeded with the same checkpoint-backed
  root shape: five packs installed in 2.70 s, 801 loose frontier objects were
  written, 20,599 objects were already in packs, enumeration took 2.78 s,
  download/write took 35.24 s, and wall time was 41.55 s. The slower wall time
  was again in the loose-object download/write phase, not in pack install.
- After installing the published 0.2.64 crates and restarting the same
  remote-host daemon, a fresh-datadir clone succeeded with line-oriented
  progress output: five packs installed in 3.26 s, 840 loose frontier objects
  were written, 20,599 objects were already in packs, enumeration took 35.31 s,
  download/write took 20.37 s, and wall time was 60.82 s. The captured helper
  log had 21 lines and zero carriage-return redraws. This run was useful for
  output verification; its slow path was object-tree enumeration plus loose
  frontier transfer, not pack installation.

### 2026-06-08: git-remote-htree 0.2.65 clone verification

Setup:
- Source-built 0.2.65 helper binaries were used before cargo publication.
- The public mutable root was republished after finding an unreadable root
  shape whose Git object directory was empty. The new push path verifies that
  an uploaded repo root is readable from a configured write server before
  announcing it.
- Each clone used a fresh helper data directory and an empty work directory.
- No pubkeys, private hostnames, exact repo names, raw hashes, temp paths, or
  IPs were retained.

Fresh clone results after the repaired root was published:

| Mode | Total wall time | Object-tree enumeration | Pack install | Loose download and write | Installed packs | Loose frontier |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Local workstation smoke | 24.97 s | 10.02 s | 3.81 s | 9.47 s | 5 | 871 |
| Remote Linux host, first run | 46.99 s | 5.27 s | 33.25 s | 7.48 s | 5 | 871 |
| Remote Linux host, warmed repeat | 41.62 s | 20.24 s | 10.88 s | 9.69 s | 5 | 871 |
| Remote Linux host, warmed repeat | 43.52 s | 5.25 s | 3.24 s | 34.19 s | 5 | 871 |

Interpretation:
- The repaired root uses the intended five deterministic checkpoint packs and
  no current-tip tail pack.
- Git reported 20,599 objects already covered by installed packs and 871 loose
  frontier objects written. The helper is no longer fetching or writing the
  historical objects that are inside installed packs.
- The metadata enumeration shortcut reduced normal object-tree enumeration to
  about 5.3 s in two remote-host runs, but relay/read-server variance can still
  make the root/object-tree read phase slower.
- Remaining remote-host clone variance came from either the largest checkpoint
  pack transfer or the 871 small loose-object reads. The slow path is network
  retrieval from the Blossom/CDN read path, not local Git object writing.

### 2026-06-08: git-remote-htree 0.2.66 pack progress smoke

Setup:
- Source-built 0.2.66 debug helper was placed first in `PATH`.
- Clone used a fresh home/data directory and an empty work directory.
- No pubkeys, private hostnames, exact repo names, raw hashes, temp paths, or
  IPs were retained.

Fresh clone result:

| Mode | Total wall time | Object-tree enumeration | Pack install | Loose download and write | Installed packs | Loose frontier |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Local workstation smoke | 78.11 s | 10.82 s | 24.7 s | 31.56 s | 5 | 875 |
| Remote Linux host smoke | 84.64 s | 20.62 s | 39.6 s | 15.19 s | 5 | 897 |

Interpretation:
- Captured non-terminal output reported checkpoint pack install as one
  aggregate `Loading git packs` progress stream: an initial line, two 10 s
  heartbeat lines while the large pack advanced, and one final done line.
- Remote-host non-terminal output had the same aggregate shape, with three
  10 s heartbeat lines while the large pack was stalled/slow and one final done
  line when it completed.
- The clone verified with `git fsck --connectivity-only`.
- This run was for output behavior, not tuning. The slow path again varied
  between metadata/root reads, the largest checkpoint pack transfer, and loose
  frontier retrieval.

### 2026-06-15: Blossom upload write-path tuning on a large live LMDB store

Setup:
- Benchmarks used a large production-like Blossom origin with multi-terabyte
  LMDB blob state. Identifying hostnames, exact repos, pubkeys, IPs, raw hashes,
  and temp paths are intentionally omitted.
- Payloads were deterministic 256 KiB blobs unless noted. Origin tests used a
  local tunnel to the daemon; public-edge tests used the deployed Worker path.
- The starting point was a live LMDB blob database where direct inline LMDB
  batch writes were pathologically slow: 32 x 256 KiB took 109.7 s with default
  LMDB flags, or 66.9 s with `MDB_NORDAHEAD`.

Changes tested:
- Keep LMDB for metadata but spill blobs >=64 KiB to content-addressed external
  files, with old inline LMDB blobs still readable.
- Add an optional hot LMDB tier for new writes, with reads falling back to the
  legacy giant LMDB. The main upload store must open through the same `LocalStore`
  path as other LMDB stores; otherwise only side stores use the hot tier.
- Remove read-before-write checks on cached blob uploads and rely on LMDB
  `NO_OVERWRITE` for primary-tier dedupe.
- Raise the live blob-write limiter from 2 to 8 after hot-tier deployment.
- Stream Worker `/upload/check` and `/upload/batch` passthrough bodies instead
  of buffering the entire request in the Worker before forwarding.

Results:

| Path | Shape | Throughput | Latency notes |
| --- | --- | ---: | --- |
| Inline legacy LMDB | direct batch, 32 x 256 KiB | 0.07 MiB/s | 109.7 s wall |
| Legacy LMDB + external spill, sync off | direct batch, 128 x 256 KiB | 4.45 MiB/s | 7.19 s wall |
| Hot LMDB + external spill, sync off | direct batch, 128 x 256 KiB | 5.19 MiB/s | 6.17 s wall |
| Origin before main store used hot tier | `/upload/batch`, c1, 128 x 256 KiB | 2.33 MiB/s | p95 2.36 s |
| Origin after main store used hot tier | `/upload/batch`, c1, 128 x 256 KiB | 4.18 MiB/s | p95 0.57 s |
| Origin hot tier, write limit 8 | raw PUT, c8, 32 x 256 KiB | 2.79 MiB/s | p95 0.84 s |
| Origin hot tier, write limit 16 | raw PUT, c16, 32 x 256 KiB | 3.53 MiB/s | p95 1.49 s |
| Origin hot tier, write limit 8 | `/upload/batch`, c4, 128 x 256 KiB | 11.66 MiB/s | p95 0.89 s |
| Origin hot tier, sync on | `/upload/batch`, c4, 128 x 256 KiB | 3.41 MiB/s | p95 4.55 s |
| Public edge after streaming Worker | `/upload/batch`, c1, 32 x 256 KiB | 1.70 MiB/s | p95 1.66 s |
| Public edge after streaming Worker | `/upload/batch`, c4, 128 x 256 KiB | 2.44 MiB/s | p95 6.19 s |

Interpretation:
- LMDB itself was not the enemy; using one huge LMDB database as both metadata
  index and large-blob body store was the enemy. The write path became dominated
  by giant-database page/freelist behavior and per-object durability work.
- The hot-tier plus external-spill path makes the origin usable again and lets
  parallel batch uploads exceed 10 MiB/s at the origin.
- Raw single-object PUT is still fixed-cost-heavy for 256 KiB blobs. It needs
  server-side coalescing or a packed/segmented blob writer to reach modern
  small-object throughput without pushing tail latency up.
- Public-edge throughput remains much lower than origin-tunnel throughput. The
  Worker streaming passthrough removes one full-body buffer/copy, but the public
  edge-to-origin leg and JSON/base64 batch protocol remain limiting factors.
- `HTREE_LMDB_EXTERNAL_BLOB_SYNC=0` is a fast interim setting. Durable modern
  performance should use a pack/segment writer with one sync per group rather
  than syncing one external file and directory per blob.

### 2026-06-15: Packed external blob writes with durable sync

Setup:
- Same large production-like origin shape as the previous write-path tuning
  experiment, with identifying hostnames, exact repos, pubkeys, IPs, raw hashes,
  and temp paths omitted.
- The daemon used a hot LMDB tier for new blob metadata, legacy LMDB fallback for
  older blobs, external blob spill for blobs >=64 KiB, and a 64 MiB external pack
  target. External blob sync was enabled.
- Payloads were deterministic 256 KiB blobs. Batch writes used 16 blobs per
  `/upload/batch` request and four concurrent requests unless noted.

Results:

| Path | Shape | Throughput | Latency notes |
| --- | --- | ---: | --- |
| Origin, packed external blobs, sync on | `/upload/batch`, c4, 128 x 256 KiB | 11.58 MiB/s | p95 0.86 s |
| Public edge, packed external blobs, sync on | `/upload/batch`, c4, 128 x 256 KiB | 2.70 MiB/s | p95 5.72 s |
| Origin read of fresh packed blobs | GET, c32, 128 x 256 KiB | 98.46 MiB/s | raw blob reads |
| Public edge read of fresh packed blobs | GET, c32, 128 x 256 KiB | 65.04 MiB/s | raw blob reads |

Interpretation:
- Packed external blob writes remove the durability/performance tradeoff from
  the interim external-spill design: origin write throughput with sync enabled
  stayed essentially equal to the previous unsynced per-hash external-file mode
  and far above per-file sync mode.
- Each concurrent 16-blob write batch produced one 4 MiB pack file in this test,
  so the origin synced one pack file and parent directory per request instead of
  one file and directory per blob.
- Packed reads are not the bottleneck for fresh blobs. Origin and public-edge
  reads both stayed well above write throughput under 32-way fetch load.
- Remaining public-edge write bottlenecks are outside the local storage writer:
  Cloudflare/public edge-to-origin behavior plus JSON/base64 batch protocol
  overhead. The next protocol-level improvement is a binary batch upload format
  or git pack/tail-pack transport that avoids base64 JSON for many small blobs.

### 2026-06-15: Binary Blossom batch upload

Setup:
- Same large production-like origin shape as the packed external blob writer
  experiment. Identifying hostnames, exact repos, pubkeys, IPs, raw hashes, and
  temp paths are omitted.
- The daemon used hot LMDB metadata, legacy LMDB fallback, external blob spill
  for blobs >=64 KiB, 64 MiB external pack target, external blob sync enabled,
  and blob write queue limit 8.
- The new `/upload/batch-binary` extension uses one auth event per batch and a
  binary body with per-entry sha256, optional content type, and raw bytes. The
  existing JSON/base64 `/upload/batch` remains supported, and the client tries
  binary first with JSON fallback.
- Payloads were deterministic 256 KiB blobs. Unless noted, batches used 16 blobs
  per request.

Results:

| Path | Shape | Throughput | Latency notes |
| --- | --- | ---: | --- |
| Origin JSON batch | c4, 128 x 256 KiB | 25.42 MiB/s | p95 0.80 s |
| Origin binary batch | c4, 128 x 256 KiB | 29.48 MiB/s | p95 0.78 s |
| Origin binary batch | c8, 256 x 256 KiB | 32.57 MiB/s | p95 1.53 s |
| Public edge JSON batch | c4, 128 x 256 KiB | 3.67 MiB/s | p95 3.96 s |
| Public edge binary batch | c4, 128 x 256 KiB | 5.14 MiB/s | p95 2.66 s |
| Public edge binary batch | c12, 256 x 256 KiB | 7.02 MiB/s | p95 5.97 s |
| Origin binary batch, 64 blobs/request | c2, 256 x 256 KiB | 61.03 MiB/s | p95 0.56 s |
| Origin binary batch, 64 blobs/request | c4, 256 x 256 KiB | 67.70 MiB/s | p95 0.90 s |
| Origin binary batch, 128 blobs/request | c2, 256 x 256 KiB | 68.14 MiB/s | p95 0.85 s |
| Origin binary batch, 256 blobs/request | c1, 256 x 256 KiB | 65.35 MiB/s | p95 0.80 s |
| Public edge binary batch, 64 blobs/request | c2, 256 x 256 KiB | 4.20 MiB/s | p95 5.26 s |
| Public edge binary batch, 64 blobs/request | c4, 256 x 256 KiB | 5.69 MiB/s | p95 8.59 s |
| Public edge binary batch, 128 blobs/request | c2, 256 x 256 KiB | 4.16 MiB/s | p95 10.00 s |
| Public edge binary batch, 256 blobs/request | c1, 256 x 256 KiB | 3.07 MiB/s | p95 10.33 s |
| Public edge binary batch, 16 blobs/request, post git-target tuning | c8, 256 x 256 KiB | 6.19 MiB/s | p95 4.76 s |
| Public edge binary batch, 16 blobs/request, post git-target tuning | c10, 256 x 256 KiB | 6.65 MiB/s | p95 5.07 s |
| Public edge binary batch, 16 blobs/request, post git-target tuning | c12, 256 x 256 KiB | 7.47 MiB/s | p95 6.10 s |
| Public edge binary batch, 16 blobs/request, post git-target tuning | c16, 256 x 256 KiB | 5.75 MiB/s | p95 9.20 s |
| Public edge read of fresh binary-batch blobs | GET, c32, 256 x 256 KiB | 26.05 MiB/s | p95 0.32 s |
| Origin read of fresh binary-batch blobs | GET, c32, 256 x 256 KiB | 2160 MiB/s | cache-hot local read |

Interpretation:
- Removing JSON/base64 from the public write path improved c4 public-edge
  throughput by about 40% and cut p95 latency by about one third on the same
  payload shape.
- Higher public-edge concurrency helped until roughly c12 with 16-blob batches.
  Larger 32-blob batches were worse and mostly inflated tail latency.
- Origin-only uploads benefit strongly from fat binary batches, reaching roughly
  65-68 MiB/s with 64-256 blobs/request. The public edge path penalizes the same
  larger request bodies, so `git-remote-htree` defaults to a 4 MiB batch target
  and exposes `HTREE_GIT_BATCH_UPLOAD_TARGET_BYTES` for origin/local tuning.
- Short post-change public probes confirmed 16-blob batches still peak near c12
  under the current baseline traffic. Raising client concurrency to c16 reduced
  throughput and nearly doubled p95 latency.
- Temporarily raising the live blob write queue limit from 8 to 12 improved
  origin c16 throughput but reduced public-edge c12 throughput and worsened
  latency. The live queue limit was restored to 8.
- Origin storage and local reads are no longer the limiting path for this blob
  size. Remaining public write work is mostly edge/origin transport overhead and
  request scheduling; the next likely step is git pack/tail-pack upload or a
  long-lived upload stream that avoids per-batch request overhead.

### 2026-06-15: Duplicate LMDB replay writes and public upload body path

Setup:
- Same production-like origin shape as the binary batch upload experiment.
  Identifying hostnames, exact repos, pubkeys, IPs, raw hashes, and temp paths
  are omitted.
- The daemon used hot LMDB metadata, external blob spill for blobs >=64 KiB,
  64 MiB external pack target, external blob sync enabled, and a blob write
  queue limit of 4.
- The test focused on replay/retry behavior: uploading the same packed binary
  batch again after all hashes were already present.
- Public-edge object storage was intentionally not used for the hot path. The
  target architecture remains local origin storage/local fileserver capacity for
  cost and operational control.

Results:

| Path | Shape | Throughput | Latency notes |
| --- | --- | ---: | --- |
| Origin binary batch, first write | c1, 96 x 256 KiB | 14.04 MiB/s | about 1.65 s request latency |
| Origin binary batch, duplicate replay | c1, 96 x 256 KiB | 189.59 MiB/s | about 70 ms request latency |
| Origin binary batch, first write | c1, 256 x 256 KiB | 36.50 MiB/s | about 1.60 s request latency |
| Origin binary batch, first write | c4, 4 x 256 x 256 KiB | 67.70 MiB/s | p50 about 3.53 s |
| Public edge binary batch, first write | c1, 96 x 256 KiB | 5.06 MiB/s | about 4.60 s request latency |
| Public edge binary batch, duplicate replay | c1, 96 x 256 KiB | 4.65 MiB/s | about 5.02 s request latency |
| Public edge binary batch, first write | c4, 4 x 96 x 256 KiB | 4.84 MiB/s | p50 about 19.6 s |
| Public tunnel, no Worker upload handler | c1, 96 x 256 KiB | 6.22 MiB/s | about 3.72 s request latency |
| Public tunnel, no Worker upload handler | c4, 4 x 96 x 256 KiB | 8.02 MiB/s | p50 about 11.31 s |
| Origin read of fresh blobs | GET, c64, 256 x 256 KiB | 1507.63 MiB/s | cache-hot local read |
| Public edge read of fresh blobs | GET, c16, 96 x 256 KiB | 8.39 MiB/s | benchmark client path |
| Public CDN-style redirected read | GET, 1 x 4 MiB | 13-18 MB/s | direct curl sample |

Interpretation:
- The LMDB writer had been preparing external pack files before it knew whether
  hashes already existed. Duplicate/retry batches could therefore create orphan
  pack files and perform avoidable filesystem sync work even when LMDB skipped
  every insert. The fix preflights sorted candidate hashes, filters existing
  hashes before pack preparation, and deduplicates repeated hashes inside the
  same batch.
- The duplicate replay path now returns quickly and does not create additional
  external pack files. This confirms LMDB itself was not the source of the
  duplicate-write slowness; the storage layer was feeding it unnecessary file
  work.
- A write queue limit of 4 is reasonable for the current synchronous local
  durability policy: four concurrent 64 MiB origin batches reached about
  68 MiB/s, while duplicate checks are effectively metadata-bound.
- Public writes remained around 5 MiB/s even when the origin queue was idle.
  That points to request body transfer before the origin writer rather than
  LMDB or local disk throughput.
- Sending the same upload benchmark through an existing public tunnel to the
  local origin, bypassing the Worker upload handler, improved c4 throughput to
  about 8 MiB/s. That is better but still far below origin capacity, so the
  tunnel request-body path is also a limiter for large writes.
- The next performance step should be a direct local-origin upload transport
  for large write bodies, such as a restricted HTTPS upload hostname/proxy or
  equivalent local fileserver path. A cloud object-store admission/cache layer is
  not part of the desired architecture unless explicitly approved.
- A DNS-only direct HTTPS prototype was prepared with a narrow reverse proxy,
  but public ACME validation timed out before reaching the origin, consistent
  with upstream/router inbound filtering. The temporary DNS/proxy exposure was
  removed after the test. A usable direct path needs explicit public TCP ingress
  to the local fileserver or a better non-Worker upload transport.

### 2026-06-16: Streaming add batches leaf chunk writes

Setup:
- Follow-up from a large local import where direct writes into a multi-terabyte
  LMDB blob database stalled while faulting cold database pages.
- The existing LMDB layer already had `put_many` batch writes, hot-tier support,
  external blob spill, and packed external blobs, but `HashTree::put_stream`
  admitted leaf chunks one at a time through `Store::put`.

Change:
- `HashTree::put_stream_with_progress` now batches prepared leaf chunks through
  `Store::put_many`, flushing at 64 MiB or 128 chunks. The cap keeps memory
  bounded while letting LMDB use one batched transaction/existence preflight for
  streamed file chunks.

Evidence:
- `cargo test -p hashtree-core put_stream` passed, including the existing
  `prop_put_stream_matches_put` property and a new test proving streamed leaf
  chunks use bounded `put_many` batches.
- `cargo test -p hashtree-cli --lib storage::tests::lmdb_hot_blob_legacy_guard_scopes_tiered_store`
  passed.
- `cargo test -p hashtree-cli --lib storage::tests::hashtree_store_uses_scoped_lmdb_hot_blob_dir`
  passed.
- `cargo test -p hashtree-lmdb external_blob_pack_batches_large_values` passed.

Interpretation:
- This is the near-term fix for huge `htree add` imports: do not ask a giant cold
  LMDB store to perform one transaction per stream chunk.
- The larger architecture remains hot-to-cold tiering: new writes should land in
  a small hot LMDB/external-pack store, while old multi-terabyte state is used as
  a read fallback. Direct live imports into the legacy giant store are still the
  wrong operational shape for HDD-backed state.

### 2026-06-15: Public upload redirect and write-concurrency check

Setup:
- Same production-like local origin as above, with hot LMDB metadata, external
  packed blobs, external blob sync enabled, and live blob write queue limit 4.
- The public Worker was briefly configured to return a `307` for large
  `/upload/batch-binary` bodies to a public tunnel hostname that reaches the
  same local-origin fileserver. No R2/S3/object-store hot path was used.
- Payloads were deterministic 256 KiB blobs. Batch size is blob count per
  request; throughput is end-to-end from a public client through the public
  hostname.

Results:

| Path | Shape | Throughput | Latency notes |
| --- | --- | ---: | --- |
| Public Worker, large redirect enabled | c4, 96 blobs/request | 0 MiB/s | failed: cross-host 307 dropped `Authorization` in reqwest, origin returned 401 |
| Public Worker, redirect disabled | c4, 96 blobs/request | 7.31 MiB/s | p50 12.01 s |
| Public tunnel, no Worker body handler | c4, 96 blobs/request | 8.69 MiB/s | p50 10.79 s |
| Public Worker, 16 blobs/request | c4 | 7.32-8.35 MiB/s | p95 about 2.5-2.8 s in clean sequential runs |
| Public Worker, 16 blobs/request | c8 | 7.74 MiB/s | p95 5.48 s |
| Public Worker, 16 blobs/request | c12 | 8.00-8.50 MiB/s | p95 about 7.1-7.5 s |
| Public Worker, 32 blobs/request | c4 | 8.38 MiB/s | p95 4.98 s |
| Public Worker, 32 blobs/request | c8 | 8.09 MiB/s | p95 8.44 s |
| Public Worker, 32 blobs/request | c12 | 7.68 MiB/s | p95 12.42 s |
| Public Worker, 64 blobs/request | c4 | 6.93 MiB/s | p95 9.38 s |
| Public Worker, 64 blobs/request | c8 | 7.36 MiB/s | p95 12.95 s |
| Public Worker, 64 blobs/request | c12 | 8.19 MiB/s | p95 11.62 s |
| Public tunnel, 16 blobs/request | c4 | 7.45 MiB/s | p95 2.47 s |
| Public tunnel, 16 blobs/request | c12 | 8.89 MiB/s | p95 6.21 s |

Interpretation:
- Cross-origin upload redirects are not a safe generic accelerator for
  authenticated Blossom writes. Common HTTP clients, including reqwest, strip
  `Authorization` when following a redirect to another host. The Worker redirect
  option was changed to require an explicit enable flag and the live deployment
  was redeployed with no redirect binding; large unauthenticated test bodies now
  return the normal validation error instead of redirecting.
- The Rust Blossom client now follows upload body redirects manually only for
  same-origin targets, same-host HTTP-to-HTTPS upgrades, and loopback test
  redirects; it refuses cross-host HTTPS redirects instead of carrying upload
  authorization to a different hostname.
- Four server-side blob write permits remain reasonable. Origin-local c4 writes
  already reached about 68 MiB/s in the previous test, while public c4-c12 writes
  all cluster around 8 MiB/s. Raising public client concurrency mostly increases
  tail latency once the tunnel is saturated.
- The current git-upload default of roughly 4 MiB batch bodies is still a good
  conservative public default. Today's matrix found 8 MiB bodies can tie or
  slightly beat 4 MiB in one run, but the gain is small and less stable than the
  transport ceiling. Use `HTREE_GIT_BATCH_UPLOAD_TARGET_BYTES` for controlled
  origin/local experiments rather than raising the public default blindly.
- LMDB and the local fileserver are no longer the slow path for these writes.
  The remaining gap to modern throughput is public ingress: Worker request-body
  proxying plus Cloudflare Tunnel transport. A truly large improvement needs a
  better direct local-fileserver upload transport, long-lived upload stream, or
  git pack/tail-pack upload path that reduces per-request edge overhead without
  moving hot storage into a cloud object store.

Follow-up:
- `upload.iris.to` was moved off the Worker custom-domain path and added directly
  to the existing Cloudflare Tunnel ingress alongside the CDN/read hostname.
  `https://upload.iris.to/api/status` returned 200 and the Worker custom-domain
  list for that hostname was empty after the change.
- A direct proxied AAAA origin prototype was prepared with a local Caddy reverse
  proxy on HTTP/HTTPS and firewall rules limited to Cloudflare source ranges.
  Public requests moved from stale tunnel 404s to Cloudflare 522/timeouts, and a
  destination-filtered packet capture saw no inbound origin packets for the
  direct hostname. This confirms public TCP ingress to the local fileserver is
  still blocked before the origin, so direct DNS cannot be the fix until the
  upstream/router path is opened.
- Workerless tunnel upload throughput was correct but not good enough: c4,
  192 x 256 KiB binary batches with 16 blobs/request reached 3.63-3.67 MiB/s
  through `upload.iris.to`; the same shape through the CDN/read tunnel hostname
  reached 4.66 MiB/s. c12 reached 3.80 MiB/s. Adding one extra tunnel connector
  moved c12 only to 4.08 MiB/s, and five connector processes with c24 reached
  only 4.36 MiB/s with much worse tail latency. Larger 24 MiB binary batch
  requests did not fix it either, reaching 3.92 MiB/s at c4. A same-host
  loopback origin check with 64-blob binary batches reached 15.12 MiB/s during
  the same pass, so public ingress remained the tighter cap even under current
  origin load.
- A temporary HTTP/2 tunnel connector was tested against the same Workerless
  ingress. It reached 4.07 MiB/s at c4 and 3.85 MiB/s at c12 for
  192 x 256 KiB binary batches, then the normal QUIC connector was restored.
  HTTP/2 was not a meaningful improvement.
- Direct public ingress was also checked via local router port-mapping paths.
  UPnP TCP mappings for the HTTP/HTTPS origin path were rejected by the router
  with policy errors, and the simple IPv6 UPnP path exposed no pinhole-capable
  device. Direct `upload.iris.to`/`cdn.iris.to` therefore still needs a router,
  ISP, or admin-side ingress change; it is not something the hashtree daemon can
  fix from inside the tunnel.
- A bounded live probe after these changes wrote 64 x 256 KiB fresh blobs through
  `upload.iris.to` as binary batches at 3.43 MiB/s. Reads of those same fresh
  blobs were 3.26 MiB/s on first fill and 25.30 MiB/s warm through
  `upload.iris.to`; through `cdn.iris.to`, the first fill was 4.46 MiB/s and the
  warm repeat was 24.33 MiB/s. The server stayed healthy afterward with idle
  blob queues and no recent 5xx burst.
- The first htree push after the tail-pack change uploaded only 57 hashtree blobs
  but still paid for failed attempts to use `wss://upload.iris.to/nostr`, which
  now returns 404 on the Workerless origin path. The stale relay was removed from
  default generated configs and from the local operator config; existing
  blocklists still protect users who already have it configured.
- A follow-up push confirmed that the stale upload relay 404 was gone, then
  showed `wss://relay.damus.io` as the remaining failed relay. Defaults were
  adjusted toward the relays that succeeded in the push by removing Damus and
  adding `nos.lol`; the local operator config was updated the same way. A second
  smoke then showed `relay.primal.net` failing while `temp.iris.to` and
  `nos.lol` succeeded, so Primal was removed from the shared default and local
  operator config as well.
- Conclusion: dropping the Worker is architecturally cleaner and fixes the 404
  routing failure, but it is not the modern-throughput fix. The hard bottleneck
  remains Cloudflare Tunnel/request-body transport under shared read/write load.
  The next meaningful architecture change is real public TCP ingress to the
  local fileserver, or a protocol change that avoids repeated public tunnel body
  uploads, such as a long-lived upload stream or git pack/tail-pack admission.

### 2026-06-15: Underfull first-publish Git pack checkpoint

Question: can `git-remote-htree` make medium first publishes cheaper without
lowering the normal deterministic checkpoint interval for large repos?

Setup:
- Local measurements compared reachable Git object counts, current-tree object
  counts, raw loose Git-object payload size, working-tree payload size, and a
  deterministic `git pack-objects` pack+idx payload for three repository shapes.
- No exact repo names, pubkeys, hashes, private paths, or host details are
  retained here.

Results:

| Shape | Reachable Git objects | Current-tree objects | Loose Git payload | Working-tree payload | Pack+idx payload |
| --- | ---: | ---: | ---: | ---: | ---: |
| Medium source repo | 455 | 409 | 2.1 MiB | 2.0 MiB | 0.54 MiB |
| Small history-heavy worker repo | 225 | 17 | 3.2 MiB | 0.26 MiB | 0.11 MiB |
| Large project repo | 21,695 | 1,638 | 281.4 MiB | 15.5 MiB | 78.1 MiB |

Change:
- Added a separate underfull first-publish checkpoint threshold. No-delta
  first publishes with at least 256 reachable Git objects now build one
  current-tip pack even when below the normal 4096-object deterministic
  checkpoint interval.
- The main interval stays 4096, so large repos do not suddenly produce many
  more checkpoint ranges. Delta and rebuild paths still avoid current-tip tail
  packs.
- Operators can disable or tune the behavior with
  `HTREE_GIT_PACK_CHECKPOINT_UNDERFULL_MIN_OBJECTS`.

Verification:
- `cargo fmt --manifest-path rust/Cargo.toml -p git-remote-htree -- --check`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree git_pack_checkpoint -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree underfull -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree -- --nocapture`
  was stopped after the P2P integration test hung in its final pull; before the
  stop, the full lib suite passed 168 tests, the basic and diff-push
  integrations passed, and the missing-old-chunks integration showed a pack
  backed clone shape with one loose object plus one small pack.
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree --test p2p_git -- --nocapture`
  passed in isolation afterward, so the earlier hang appears order/flakiness
  related rather than a deterministic regression from the underfull pack change.

Interpretation:
- For medium first-publish repos, a single pack can replace hundreds of loose
  Git-object uploads and is substantially smaller than the corresponding loose
  Git payload. Working-tree files still need to be present in the hashtree root,
  but the `.git/objects` side no longer has to be hundreds of small blobs.
- This does not solve the raw public ingress ceiling by itself. It reduces
  request/object churn for git pushes while the remaining large write throughput
  work stays focused on public ingress, long-lived upload streams, or direct
  local-fileserver transport.

### 2026-06-16: Underfull delta Git tail-pack checkpoints

Question: can `git-remote-htree` make medium delta pushes cheaper, not only
medium first publishes?

Change:
- Delta pushes that already have a pack-backed remote base now build a
  current-tip tail pack when the pushed delta has at least the underfull
  threshold of Git objects. The normal deterministic checkpoint interval stays
  unchanged, and missing-base-checkpoint rebuilds still avoid adding a
  current-tip tail pack.
- Inherited base pack coverage is merged with the new tail pack coverage before
  building the hashtree root. Import selection still brings in current tree
  objects needed to materialize the browsable working tree, but it avoids
  re-importing unchanged base blobs as loose Git objects.

Shape measurements:

| Shape | Delta Git objects | Loose Git payload | Pack+idx payload | Current working-tree payload |
| --- | ---: | ---: | ---: | ---: |
| Repeated medium text edits | 900 | 278.6 KiB | 102.3 KiB | 13.1 KiB |
| Tiny-object edge case | 900 | 69.1 KiB | 89.5 KiB | 4 B |

Interpretation:
- For source-like repeated text deltas, the tail pack cuts the `.git/objects`
  upload payload substantially and replaces hundreds of loose Git object entries
  with pack+idx files. This should reduce public tunnel pain for medium git
  pushes even while raw `upload.iris.to` body throughput remains capped.
- For extremely tiny object deltas, pack index overhead can exceed loose bytes,
  but the pack still collapses object fanout. Keep the underfull threshold
  tunable with `HTREE_GIT_PACK_CHECKPOINT_UNDERFULL_MIN_OBJECTS`.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree underfull_delta -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree tail_pack -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree helper::tests -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree git::storage::tests -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree --lib`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-blossom`

### 2026-06-16: Byte-gated underfull Git pack checkpoints

Question: can the underfull pack optimization avoid adding complexity or bytes
for tiny-object pushes while still packing source-like histories that actually
benefit?

Change:
- Underfull first-publish and pure delta tail-pack plans now require byte
  savings before their generated pack+idx is installed into the push tree. The
  helper compares generated pack bytes with the raw Git object content bytes the
  pack would cover, and falls back to the ordinary loose-object path if the pack
  would be larger.
- The normal large-repo deterministic checkpoint interval is unchanged, and the
  existing object-count threshold remains tunable with
  `HTREE_GIT_PACK_CHECKPOINT_UNDERFULL_MIN_OBJECTS`.
- This is only a `git-remote-htree` push-shape optimization. It does not add any
  bucket, R2, S3, or external object-store admission path.

Verification:
- `cargo fmt --manifest-path rust/Cargo.toml -p git-remote-htree -- --check`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree underfull_initial -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree git_pack_checkpoint -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree tail_pack -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p git-remote-htree -- --nocapture`
- `cargo install --path rust/crates/git-remote-htree --force`

### 2026-06-16: CDN extensionless hash redirect

Question: can the public read hostname avoid Cloudflare's extensionless-cache
miss path without broad Cache Rules or Worker logic?

Change:
- Added a narrow server redirect for GET `/<sha256>` on the CDN hostname to
  `/<sha256>.bin`. The upload hostname keeps normal Blossom-compatible
  extensionless lookup behavior.
- Added `docs/PERFORMANCE.md` to capture the current public-edge model:
  extensionful CDN blob reads, local-origin writes, no R2/S3 hot-path admission,
  no bulk-upload Worker handler, and the direct-ingress options for dynamic
  home IPs or public reverse proxy fallback.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli serve_content_or_blob_ -- --nocapture`
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli --lib`
- A clean Linux release build using a fresh hashtree checkout and matching FIPS
  source completed successfully, then the live origin daemon was restarted with
  the new binary.
- Live public probes after restart showed the CDN hostname returning `308` from
  an extensionless 64-hex blob path to the `.bin` path with immutable cache
  headers, while the upload hostname kept extensionless Blossom lookup semantics
  and returned a normal not-found response for a missing blob.

Interpretation:
- This improves extensionless CDN read behavior without reintroducing Worker or
  object-store logic. It does not fix the public write ceiling; write throughput
  still needs direct 443 ingress to the local reverse proxy, or a public reverse
  proxy connected to the local origin over an authenticated private link when
  direct home NAT forwarding is unavailable.

### 2026-06-16: Tiered LMDB hot quota excludes legacy cold data

Question: can an Osiris hot LMDB tier sit in front of a much larger legacy LMDB
without startup or write-time quota pressure evicting the cold database?

Change:
- `HashtreeStore` now opens its raw blob backend with an explicit LMDB map size
  but without adapter-level `max_bytes` eviction. The tree/Blossom retention
  layer remains responsible for quota policy.
- Tiered LMDB exposes writable-tier stats/list/delete helpers. Hot-cache quota
  checks and local-only eviction use the writable primary tier only; full reads
  still fall through to the legacy tier, and explicit full deletes still delete
  from both tiers.
- Size-only retention/indexing paths now use blob metadata instead of reading
  whole blob bodies.

Verification:
- `cargo fmt --check`
- `cargo test -p hashtree-cli --lib storage::tests::tiered_lmdb_legacy_bytes_do_not_drive_hot_quota`
- `cargo test -p hashtree-cli --lib storage::tests::lmdb_hot_blob_legacy_guard_scopes_tiered_store`
- `cargo test -p hashtree-cli --lib storage::tests::hashtree_store_uses_scoped_lmdb_hot_blob_dir`
- `cargo test -p hashtree-cli --lib`

Interpretation:
- A hot tier can now be capped for writes without treating an old, larger LMDB
  as disposable cache. This is a prerequisite for routing public uploads to an
  Osiris hot origin while keeping Vader's large store as cold/read-through data
  and replication target.

### 2026-06-16: LMDB insert-if-absent batch reports

Question: can duplicate-heavy Blossom/git upload retries avoid read-before-write
checks, duplicate metadata writes, and quota eviction based on candidate bytes?

Change:
- Added an exact `PutManyReport` for local batch writes. The old `put_many`
  count API remains as a wrapper.
- LMDB single and batch writes now let `MDB_NOOVERWRITE` decide insert
  membership. Duplicate puts return false and do not touch access metadata,
  eviction order, stats, or external blob files.
- Batch writes deduplicate only repeated hashes inside the request, then use one
  LMDB write transaction and report only newly inserted hashes/bytes.
- External packed blobs are written only after LMDB accepts the hash. The batch
  first reserves accepted hashes in the transaction, writes pack files for those
  accepted entries, then replaces reservations with final pack markers before
  commit.
- Cached and durable Blossom batch quota now runs after the raw write and uses
  exact inserted bytes. Map-full cleanup remains a separate physical
  map-pressure retry path.
- Durable owned uploads roll back newly inserted bodies if post-insert quota
  enforcement rejects the write.

Local storage benchmark setup:
- Release `storage_write_bench` example on a local temp LMDB store.
- External blobs enabled for values >=64 KiB, 64 MiB pack target, sync enabled.
- Candidate throughput counts all candidate bytes, so duplicate replay has a
  visible throughput even when `inserted=0`.

Results:

| Shape | Batch | Payload | Inserted | Wall | Candidate throughput |
| --- | ---: | ---: | ---: | ---: | ---: |
| Unique | 1 | 256 x 256 KiB | 256 | 4142 ms | 15.45 MiB/s |
| Duplicate replay | 1 | 256 x 256 KiB | 0 | 374 ms | 171.11 MiB/s |
| Unique | 16 | 256 x 256 KiB | 256 | 622 ms | 102.77 MiB/s |
| Duplicate replay | 16 | 256 x 256 KiB | 0 | 366 ms | 174.62 MiB/s |
| Unique | 256 | 256 x 256 KiB | 256 | 424 ms | 150.66 MiB/s |
| Duplicate replay | 256 | 256 x 256 KiB | 0 | 371 ms | 172.17 MiB/s |
| Unique | 4096 | 4096 x 64 KiB | 4096 | 1645 ms | 155.60 MiB/s |
| Duplicate replay | 4096 | 4096 x 64 KiB | 0 | 1475 ms | 173.53 MiB/s |
| 90/10 replay/new | 256 | 256 x 256 KiB | 26 | 384 ms | 166.59 MiB/s |
| Unique, 128 MiB logical max | 256 | 256 x 256 KiB | 256 | 414 ms | 154.56 MiB/s |
| Duplicate, 128 MiB logical max | 256 | 256 x 256 KiB | 0 | 365 ms | 175.28 MiB/s |

Verification:
- `cargo fmt --check`
- `cargo test -p hashtree-lmdb --lib`
- `cargo test -p hashtree-cli --lib server::blossom::tests::owned_blossom_uploads_are_rejected_when_storage_limit_is_full`
- `cargo test -p hashtree-cli --lib`
- `cargo build -p hashtree-cli --release --example storage_write_bench`

Interpretation:
- Duplicate-heavy local writes are now metadata-bound and avoid external pack
  creation, duplicate access-time writes, and candidate-byte eviction.
- Batch sizes >=16 are the useful local write shape for 256 KiB blobs. Batch 1
  is still much slower because request/batch overhead dominates; 256 and 4096
  are similar on this local store.
- Logical quota checks no longer punish duplicate-heavy batches. The measured
  128 MiB logical-max case stayed in the same performance band as the unbounded
  case.
- This fixes the local LMDB/write-path waste. If public `upload.iris.to`
  remains far below these local numbers, the remaining bottleneck is ingress
  transport/body forwarding or remote deployment shape, not LMDB insert
  membership.

### 2026-06-16: Hot origin with workerless replica tunnel

Question: after the LMDB insert-if-absent fix, does the public write path still
stall because of local storage, public ingress, or hot-origin-to-replica
transport?

Setup:
- Public `upload.iris.to` and `cdn.iris.to` point at a hot local origin with
  bounded write-behind replication to a much larger replica origin.
- The replica queue is capped at 512 MiB and upload concurrency is 4.
- The first private route to the replica was tested with the same signed
  binary-batch client used for public upload tests. A workerless tunnel hostname
  to the same replica was then tested as a fallback route.

Results:

| Path | Shape | Result |
| --- | --- | ---: |
| Hot origin container loopback | 128 x 256 KiB, batch 32, c4 | 142.80 MiB/s |
| Private hot-origin-to-replica route | 8 x 256 KiB, batch 8, c1 | did not finish in 30s; earlier 32 x 8 run was 0.06-0.08 MiB/s |
| Workerless replica tunnel | 128 x 256 KiB, batch 32, c4 | 8.83 MiB/s |
| Workerless replica tunnel | 128 x 256 KiB, batch 32, c8 | 4.72 MiB/s |
| Workerless replica tunnel | 128 x 256 KiB, batch 32, c12 | 4.46 MiB/s |
| Public `upload.iris.to` from a Linux host | 128 x 256 KiB, batch 32, c4 | 8.46 MiB/s |
| Public `upload.iris.to` reads | 128 x 256 KiB, c16 | 39.67 MiB/s |
| Public `cdn.iris.to` reads | 128 x 256 KiB, c16 | 35.05 MiB/s |

Operational changes:
- The replica target was switched from the unusably slow private route to the
  existing workerless tunnel hostname. Under the measured public c4 write load,
  the bounded replica queue drained back to near-empty instead of saturating.
- Successful write-behind replication logs were moved from info to debug; warn
  logs still cover queue-full, retry, and failure cases.

Interpretation:
- The hot origin's local LMDB/write path is not the public bottleneck.
- The broken private route was the cause of replica queue saturation; the
  workerless tunnel route is fast enough to keep pace with the measured public
  write path at c4.
- Public writes are still transport-bound at about 8-9 MiB/s for this shape.
  Reaching modern bulk-write throughput still needs a better ingress path or a
  protocol shape that sends fewer/larger long-lived bodies; it does not need
  R2/S3/object-store admission.

### 2026-06-16: Workerless hot-origin public ingress timing

Question: after moving public writes to the workerless hot origin, is the
remaining upload ceiling Cloudflare, nginx, hashtree/LMDB, or batch sizing?

Setup:
- Public `upload.iris.to` and `cdn.iris.to` resolved through Cloudflare and
  negotiated HTTP/2 from the client. The public reverse proxy also listens with
  HTTP/2 and proxies upload traffic to the hot local origin with request
  buffering disabled.
- The hot origin status stayed healthy during the test: replica queue remained
  bounded and recent 5xx stayed at zero.
- The reverse proxy access log was extended to include request length, request
  time, upstream response time, upstream status, and cache status. No private IPs
  or raw blob hashes are retained here.

Results:

| Path / shape | Result |
| --- | ---: |
| Public writes, release client, 128 x 256 KiB, batch 16, c8 | 8.02 MiB/s |
| Public writes, release client, 64 x 256 KiB, batch 16, c1/c2/c4/c8/c16 | 5.37 / 7.37 / 7.56 / 7.05 / 8.09 MiB/s |
| Public reads through CDN, release client, 128 x 256 KiB, c8 | 40.10 MiB/s |
| Public writes from a second client host, 64 x 256 KiB, batch 16, c8 | 7.29 MiB/s |
| Normal Cloudflare path, one 4 MiB signed binary batch via curl | 4.08 MB/s upload, 1.03 s total |
| Direct origin resolve, same host/SNI, one 4 MiB signed binary batch via curl | 4.59 MB/s upload, 0.91 s total |
| Public single stream, 4 / 8 / 16 MiB binary batch bodies | 4.51 / 5.62 / 6.79 MiB/s |
| Public single stream, 32 MiB binary batch body | failed with edge 520 before origin log entry |
| Public writes, 64 MiB total, 16 MiB batches, c4 | 9.37 MiB/s |

Reverse-proxy timing sample:
- 4 MiB public binary batch requests reached nginx with `req_time` and
  `upstream_time` both around 0.5-1.1 s. With request buffering disabled, this
  means nginx is streaming the body to the origin as Cloudflare/client delivers
  it; the timing does not indicate a separate slow LMDB commit.
- 8 MiB and 16 MiB single-stream requests reached origin and completed, while
  the 32 MiB request did not appear in the origin proxy access log.

Interpretation:
- Release-mode reads are fine for current purposes; earlier lower read numbers
  were client/build-profile artifacts.
- The write ceiling is still public body ingress. It reproduced from two client
  hosts, did not stress nginx or htree CPU, and did not grow the replica queue.
- Bypassing the Cloudflare proxy for one direct-origin request improved a 4 MiB
  upload only marginally, so simply turning off the proxy is unlikely to produce
  a step-function throughput win by itself.
- Larger binary batches reduce request count and improve single-stream
  throughput up to about 16 MiB, but 32 MiB is unsafe through the current public
  path. Keep the conservative 4 MiB git batch default and use
  `HTREE_GIT_BATCH_UPLOAD_TARGET_BYTES` for controlled origin/local experiments.
- Further large improvements need a better bulk-write ingress path or fewer
  bytes/objects per git push, not R2/S3/bucket admission and not a larger local
  LMDB write queue.

### 2026-06-16: Upload-host immutable read cache check

Question: should `upload.iris.to` get another origin-side cache layer for hash
GETs, or is that extra complexity with the current Cloudflare/local-origin path?

Setup:
- A previously uploaded 256 KiB benchmark blob was requested through both the
  upload and CDN public hostnames using extensionful `/<sha256>.bin` URLs.
- The public path was not changed during this check.

Results:

| Path / request | Observed headers |
| --- | --- |
| `upload.iris.to/<sha256>.bin`, repeated HEAD | `cache-control: public, max-age=31536000, immutable`; `cf-cache-status: HIT`; content length 256 KiB |
| `cdn.iris.to/<sha256>.bin`, first HEAD | immutable cache-control; origin cache header reported `MISS`; Cloudflare reported `MISS` |
| `cdn.iris.to/<sha256>.bin`, repeated HEAD | immutable cache-control; Cloudflare reported `HIT` |
| `upload.iris.to` read benchmark, 64 x 256 KiB, c16 | 31.82 MiB/s first pass, 54.57 MiB/s repeated pass |

Interpretation:
- Do not add a second upload-host cache layer without a colder-edge benchmark
  showing it is needed. Extensionful immutable blob URLs are already eligible
  for Cloudflare edge caching on the upload hostname.
- This keeps the public design simpler: writes stream through the local origin,
  reads use immutable content-addressed cache behavior, and no R2/S3/bucket hot
  path is involved.

### 2026-06-16: Upload nginx HTTP/2 body buffer tuning

Question: is the current public write ceiling caused by nginx/hashtree storage,
or by slow request-body ingress from Cloudflare/client into the upload origin?

Setup:
- Live public upload origin was still the non-S3 hot-origin image.
- The disruptive storage healthcheck timer on the large replica remained
  masked/inactive.
- The upload reverse proxy already had raised worker connection and file
  descriptor limits. This pass changed only origin TLS/body handling:
  `ssl_protocols TLSv1.2 TLSv1.3`, `http2_body_preread_size 1m`, and
  `client_body_buffer_size 1m` on the upload server.
- A temporary per-vhost attempt to disable HTTP/2 for `upload.iris.to` validated
  but did not change logged request protocol on the shared 443 listener, so it
  was reverted.

Results:

| Shape | Before body-buffer tuning | After body-buffer tuning |
| --- | ---: | ---: |
| Public writes, 64 x 256 KiB, batch 16, c8 | 3.96 MiB/s | 3.37 MiB/s |
| Public writes, 128 x 256 KiB, batch 16, c12 | 2.94 MiB/s | 4.02 MiB/s |
| Public CDN reads, same 64 x 256 KiB set, c16 | 29.13 MiB/s first pass, 34.77 MiB/s repeat | unchanged path |
| Public upload-host reads, same 64 x 256 KiB set, c16 | 28.81 MiB/s first pass, 38.13 MiB/s repeat | unchanged path |

Reverse-proxy timing:
- Before body-buffer tuning, the c12 run logged 4 MiB `POST /upload/batch-binary`
  requests with request/upstream times around 6.9-8.6 s.
- After body-buffer tuning, the same request shape logged about 4.3-5.7 s.
- Hashtree CPU stayed low and no slow Blossom batch, queue-full, or storage
  warning stream appeared during the probes.

Interpretation:
- The body-buffer/TLS modernization is worth keeping, but it is not a complete
  fix. The unstable c8/c12 numbers and matching request/upstream times still
  point to public request-body ingress before LMDB.
- Do not chase larger LMDB write queues for this symptom. The next step-function
  write improvement is topology/protocol: a better public ingress route, a true
  hot edge that accepts locally before private replication, or fewer bytes/bodies
  per git push.

### 2026-06-16: Client upload HTTP/1.1 transport sweep

Question: does the client-to-Cloudflare upload protocol choice explain part of
the public write variance?

Setup:
- `hashtree-blossom` now has separate upload transport selection. Upload bodies
  default to HTTP/1.1-only, while reads keep the ordinary HTTP client. The
  `upload_queue_bench` example can opt back into HTTP/2 negotiation with
  `--upload-http2-auto`.
- Benchmarks used public `upload.iris.to`, binary batches, 128 x 256 KiB,
  16 blobs/request, concurrency 12.

Results:

| Client upload transport | Result |
| --- | ---: |
| HTTP/2-auto, first comparison | 4.39 MiB/s |
| HTTP/1.1-only, first comparison | 7.99 MiB/s |
| HTTP/1.1-only, after default change | 8.03 MiB/s |
| HTTP/2-auto opt-out, after default change | 8.44 MiB/s |
| Alternating sweep HTTP/2-auto | 8.04 MiB/s, 8.49 MiB/s |
| Alternating sweep HTTP/1.1-only | 8.30 MiB/s, 8.73 MiB/s |

Interpretation:
- The first comparison showed a large HTTP/1.1 win, but the alternating sweep
  showed both transports can reach about 8 MiB/s once the edge path is behaving.
  HTTP/1.1-only was still slightly ahead in that sweep and is safer for upload
  bodies because it avoids bad HTTP/2-auto samples without affecting read
  negotiation.
- This is a client/git-push improvement, not a server storage fix. LMDB and
  hashtree CPU stayed below the public write ceiling during the probes.

### 2026-06-16: LMDB duplicate-write hot path audit

Question: is LMDB being used incorrectly on duplicate-heavy Blossom writes, or
is the remaining public upload ceiling above the local storage layer?

Setup:
- Current LMDB write paths use one write transaction per batch and
  `PutFlags::NO_OVERWRITE` as the exact insert-if-absent primitive. Duplicate
  single puts return `false`; duplicate batch items are skipped by LMDB and do
  not write blob metadata, eviction-order records, or logical byte counters.
- Batch writes return `PutManyReport { total, inserted, inserted_bytes,
  inserted_hashes }`; quota enforcement uses exact inserted bytes after the
  write attempt, not the total candidate byte count.
- Local release-mode storage bench used cached-batch writes with 256 KiB blobs
  and a 1 GiB logical max. A separate public sample used
  `upload_queue_bench` against the upload hostname with binary batches, 64 x
  256 KiB, 16 blobs/request, concurrency 12, HTTP/1.1 upload bodies.

Verification:
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-lmdb --lib -- --nocapture`
  passed: duplicate single put is a no-op, batch reports only new hashes/bytes,
  and duplicate-heavy batches do not evict by candidate bytes.
- `cargo test --manifest-path rust/Cargo.toml -p hashtree-cli duplicate -- --nocapture`
  passed: Blossom duplicate writes do not refresh blob access time, cached
  duplicate-heavy quota uses actual inserted bytes, and duplicate uploads do
  not trigger write-behind replication.

Results:

| Shape | Batch size | Inserted | Candidate throughput | Inserted throughput |
| --- | ---: | ---: | ---: | ---: |
| Unique local cached batch, 256 x 256 KiB | 1 | 256 | 30.95 MiB/s | 30.95 MiB/s |
| 100% duplicate replay, same candidates | 1 | 0 | 176.13 MiB/s | 0.00 MiB/s |
| Unique local cached batch, 256 x 256 KiB | 16 | 256 | 115.47 MiB/s | 115.47 MiB/s |
| 100% duplicate replay, same candidates | 16 | 0 | 176.47 MiB/s | 0.00 MiB/s |
| Unique local cached batch, 256 x 256 KiB | 256 | 256 | 153.30 MiB/s | 153.30 MiB/s |
| 100% duplicate replay, same candidates | 256 | 0 | 176.01 MiB/s | 0.00 MiB/s |
| 90/10 duplicate replay, 256 x 256 KiB | 256 | 26 | 172.90 MiB/s | 17.56 MiB/s |
| Public upload sample, binary batch, c12 | 16 | 64 | 7.12 MiB/s | 7.12 MiB/s |

Interpretation:
- LMDB duplicate handling is not the reason public writes are in the
  single-digit MiB/s range. Local duplicate replay processes candidate bytes at
  about 176 MiB/s, and unique local batches are tens to hundreds of MiB/s
  depending on batch size.
- The public upload sample remains an order of magnitude below the local
  duplicate-aware write path. Continue prioritizing public ingress/topology,
  request-body transport, and git object fanout before adding probabilistic
  duplicate filters or cloud object-store admission layers.

### 2026-06-16: Public ingress, direct-origin, and concurrency follow-up

Question: after the hot origin is local and LMDB is fast, is the remaining
single-digit public upload rate caused by Cloudflare proxying, direct-origin
network path, nginx/host socket limits, or too much client concurrency?

Setup:
- Public upload hostname routed to the hot origin's local htree daemon. A
  same-host benchmark to that daemon removes Cloudflare and public network
  ingress from the path.
- The hot-origin filesystem had become dangerously full because of Docker build
  cache and old build contexts. Disposable build/cache artifacts were removed,
  restoring about 54 GiB free space without deleting active htree blob data.
- `upload_queue_bench` gained a diagnostic `--resolve host=ip:port` option and
  `--danger-accept-invalid-certs` flag so the same Blossom upload code can test
  a direct origin path without changing DNS. The danger flag is for measurement
  only; direct deployment needs a publicly trusted certificate.
- The hot origin's TCP receive/send buffer caps were raised from tiny defaults
  to 32 MiB and persisted with sysctl.

Results:

| Shape | Result |
| --- | ---: |
| Hot origin to local htree, binary batch c12, 128 x 256 KiB | 100.96 MiB/s |
| Public hostname, same shape after disk cleanup | 6.81 MiB/s |
| Direct-origin override, same TLS name, invalid-cert diagnostic | 9.31 MiB/s |
| Public hostname immediate A/B after direct-origin run | 8.94 MiB/s |
| Second client public hostname, same shape | 9.32 MiB/s |
| Second client raw 32 MiB unauthenticated PUT through public hostname | 7.8 MB/s upload, 401 after body |
| Second client raw 32 MiB unauthenticated PUT direct-origin diagnostic | 6.8 MB/s upload, 401 after body |
| Public hostname after 32 MiB socket buffer cap | 9.45 MiB/s |
| Public hostname, later small c4/c8 recheck, 64 x 256 KiB | 7.00-7.41 MiB/s |
| Upload-host origin HTTP/1.1-only A/B, 128 x 256 KiB c4 | 9.69 MiB/s |
| Restored origin HTTP/2 A/B, same 128 x 256 KiB c4 | 9.63 MiB/s |
| Raw SSH stream from same client to the public hot host | 8.86 MiB/s |

Public batch/concurrency sweep after the above:

| Blob batch size | Concurrency | Result |
| ---: | ---: | ---: |
| 16 | 4 | 9.67 MiB/s |
| 16 | 8 | 8.68 MiB/s |
| 16 | 12 | 5.44 MiB/s |
| 16 | 16 | 8.54 MiB/s |
| 32 | 4 | 5.44 MiB/s |
| 32 | 8 | 5.49 MiB/s |
| 32 | 12 | 5.03 MiB/s |
| 32 | 16 | 5.66 MiB/s |
| 64 | 4 | 8.09 MiB/s |
| 64 | 8 | 8.98 MiB/s |
| 64 | 12 | 9.40 MiB/s |
| 64 | 16 | 8.95 MiB/s |

Interpretation:
- The hot-origin daemon and local store are healthy; the same request shape is
  about 10x faster without public ingress.
- Direct-origin was not a step-function win in this test and also needs a real
  public certificate before it could be used by normal clients. Do not flip DNS
  to direct-only expecting it to solve write throughput by itself.
- Disabling Cloudflare-to-origin HTTP/2 for the upload host did not materially
  change the larger c4 sample, so the origin HTTP version is not the current
  step-function bottleneck.
- A raw SSH stream to the same hot host landed in the same throughput band as
  Blossom uploads, which points at the client-to-hot-host network path rather
  than LMDB, request parsing, or nginx buffering.
- The public path currently behaves like a request-body ingress ceiling around
  8-10 MiB/s from multiple clients. Socket buffer modernization is worth keeping
  but did not move this particular sample.
- Keep public git-push defaults conservative: the default Blossom
  `upload_concurrency` is 4, while private/local origins can explicitly raise it
  after measuring their path.

### 2026-06-16: Large local add against live LMDB store

Question: why can `htree add` of a large local map file be far slower than raw
disk reads, even when no Blossom upload is involved?

Setup:
- Large local plaintext file, about 80 GiB.
- Live LMDB-backed blob store with about 3.6 TiB stored objects and external
  local blob packs enabled for large blobs.
- The source file read alone reached about 230-250 MB/s.
- The host was under heavy memory and I/O pressure during tests; swap was full,
  so these are stressed-system measurements rather than hardware ceilings.

Results:

| Shape | Result |
| --- | ---: |
| Local add without service LMDB env | Stalled at 0 B for 90s; stack in LMDB mmap `filemap_fault` |
| Service LMDB env, default 2 MiB content chunks, external pack fsync enabled | 704 MiB in 90s, about 7.8 MiB/s |
| Same env, default content chunks, external pack fsync disabled | 1.3 GiB in 90s, about 15 MiB/s after writeback pressure |
| Patched plain `--local`, default content chunks, no manual env | 1.2 GiB in 90s, about 14.4 MiB/s |
| Bulk local profile plus link-aware indexing, default 2 MiB content chunks, 4 GiB prefix sample with final sync | 4 GiB in 44.75s, about 91 MiB/s |
| Same env, explicit 256 MiB content chunks, external pack fsync disabled | 6.5 GiB in 90s, about 74 MiB/s sustained |

Interpretation:
- LMDB itself was not the only issue. Missing `HTREE_LMDB_NO_READ_AHEAD=1` on
  an ad-hoc local command caused a cold huge-mmap stall before file bytes were
  processed.
- With the service LMDB env, the bottleneck moved to local external pack writes.
  Per-pack `fsync` caused XFS log waits and single-digit MiB/s throughput.
- Disabling per-pack fsync is appropriate only for trusted local bulk ingest
  where the source file still exists and the import can be rerun after a crash.
  Public/server upload paths should keep stronger durability by default.
- Default 2 MiB content chunks were not intrinsically the problem. The slow path
  was paying LMDB sync/writeback overhead, not using local external pack defaults
  for ad-hoc local adds, and then walking the just-built tree with avoidable
  storage lookups during indexing.
- Larger content chunks still reduce per-pack metadata and sync overhead, but
  they intentionally change CIDs for large files and should remain explicit.
- `htree add --local` should use this fast local-ingest profile by default:
  local-only, LMDB no-readahead, local external blob packs, relaxed per-pack
  fsync, relaxed LMDB commit sync with one explicit final store sync, and larger
  stream store batches. It should not silently change the content chunk size,
  because that would change CIDs relative to non-local adds.

### 2026-06-16: Current hot-origin redeploy and status counters

Question: after adding upload-replication counters and git upload traversal
optimizations, can the public hot origin be updated without regressing write
throughput, and do the new counters prove write-behind behavior?

Setup:
- Built and installed current master on the deep replica's systemd daemon.
- Built a no-S3 hot-origin container from current master using the reproducible
  `rust/Dockerfile.hot-origin` context shape: `hashtree-rust/`,
  `cashu-service/`, and `fips/`.
- The hot-origin image needed `pkg-config`, `libdbus-1-dev`, and `libclang-dev`
  in the builder stage for FIPS dependencies, plus runtime `libdbus-1-3`.
- The container was recreated with the existing local data volume, config
  volume, proxy network, and upload replication settings.

Results:

| Shape | Result |
| --- | ---: |
| Deep replica daemon after restart | active, new upload-replication counters visible |
| Hot origin container-local binary batch, 128 x 256 KiB, c4, batch 16 | 152.44 MiB/s |
| Hot origin replication counters after local sample | 4 accepted batches, 64 uploaded blobs, 16 MiB replicated, 0 failures |
| Public upload hostname from this client, 64 x 256 KiB, c4, batch 16 | 8.78 MiB/s |
| Hot origin queue state after public sample | blob writes idle, replication queue empty, 0 failed/fallback/skipped jobs |

Interpretation:
- The current hot-origin local write path remains fast after the redeploy; the
  new status counters show accepted/coalesced replication work draining cleanly.
- Public writes are still in the same single-digit MiB/s band from this client,
  so the current bottleneck is still public ingress/client-to-hot-host network
  rather than LMDB, queue limits, or local write-behind.
- Recent live traffic after the redeploy had no 5xx, no htree WARN/ERROR logs,
  and negligible htree CPU. The visible request mix was normal upload/hash/Nostr
  traffic, not an obvious sustained DDoS.
- Keep the hot path local-fileserver based. This deploy did not add R2/S3,
  buckets, or an external object-store admission layer.

### 2026-06-17: Home downlink is not the upload ceiling

Question: if the home connection reports a fast downlink, why can public
Blossom uploads from the home client still be slow?

Setup:
- Same home client and public upload hostname as the previous public write
  checks.
- Fresh-seed benchmark data to avoid duplicate replay.
- Same 32 MiB planned binary batch shape for public and hot-origin-local
  checks: 128 x 256 KiB blobs, batch 16, concurrency 4.

Results:

| Shape | Result |
| --- | ---: |
| Home client network quality uplink | 69.762 Mbps |
| Home client network quality downlink | 771.461 Mbps |
| Home client to public upload hostname, fresh binary batch | 1.36 MiB/s |
| Hot-origin local binary batch, same fresh workload | 107.86 MiB/s |
| Hot-origin write-behind state after local sample | 0 queued, 0 in flight, 0 failed |

Interpretation:
- The reported fast home downlink does not bound upload throughput from the
  home client. This sample's measured uplink was about 70 Mbps, or roughly
  8.7 MB/s before protocol overhead.
- The fresh public upload was worse than raw uplink capacity in this run, which
  points at public edge/origin/request-body behavior or transient congestion on
  top of ISP uplink asymmetry.
- The same fresh workload reached more than 100 MiB/s locally on the hot
  origin, and replication drained without backlog. That keeps LMDB, local
  storage, and the hashtree write queue out of the primary-cause box for this
  public-client sample.

### 2026-06-17: PMTiles range-read CPU under live local load

Question: while a local map-index job is reading a large PMTiles object through
`htree` over localhost, is the active Vader bottleneck ZFS/storage, or repeated
range-serving CPU work?

Setup:
- Live deep store on an XFS filesystem backed by a ZFS zvol.
- Active local map-index job reading the PMTiles object through
  `http://127.0.0.1:8080/.../tiles.pmtiles` with high tile concurrency.
- Live daemon was still on the pre-cache binary.
- Patched side daemon from commit `f71ae2be` was started temporarily on
  `127.0.0.1:18080` against the same data directory, then stopped after the
  benchmark.
- Benchmark: 240 fixed random 16 KiB HTTP range reads against the same PMTiles
  path, first against live `8080`, then against patched side `18080`.

Results:

| Shape | Result |
| --- | ---: |
| Live pre-cache daemon, same host load | avg 62.210 ms, p50 62.266 ms, p95 100.037 ms, wall 14.931 s |
| Patched side daemon, same host load | avg 3.802 ms, p50 1.927 ms, p95 3.860 ms, wall 0.913 s |
| Patched live daemon after controlled restart | avg 4.855 ms, p50 3.439 ms, p95 13.359 ms, wall 1.166 s |
| Full threaded `hashtree-cli --lib` test suite | 293 passed, 0 failed, 7 ignored |

Interpretation:
- The active PMTiles/indexer load was mostly `htree` CPU, not ZFS write
  throughput. Device telemetry during the run showed the hashtree zvol doing
  light reads, while the daemon spent several cores on localhost HTTP range
  serving.
- The old range path rebuilt immutable file chunk metadata and started chunk
  iteration from the beginning for every request. On a large 2 MiB-chunked
  object, random tiny ranges can repeatedly scan/copy tens of thousands of
  chunk entries.
- Commit `f71ae2be` adds a bounded in-process immutable file metadata cache and
  starts uniform-chunk range iteration at the overlapping chunk. This removes
  the observed repeated CPU work without changing on-disk format, CIDs, LMDB,
  or the storage topology.
- The patched binary was installed on Vader and activated with a controlled
  `hashtree.service` restart while the map-index job kept running. The live
  daemon came back healthy, served 0 recent 5xx responses, and reproduced the
  range-latency improvement on the real `8080` path.
- Do not migrate away from the ZFS zvol for this specific bottleneck. ZFS may
  still matter for separate bulk-write or durability policy decisions, but this
  live load is better addressed in hashtree's range-serving path.

### 2026-06-17: v0.2.71 public upload/read smoke after route hotfix

Question: after restoring allowed encrypted `npub` release serving and
redeploying the public origin, are public Blossom writes and reads healthy?

Setup:
- Public upload hostname through the normal Cloudflare-facing path.
- Fresh deterministic 32 MiB workload: 128 x 256 KiB blobs.
- Upload shape: binary batch, batch size 16, concurrency 4, HTTP/1 upload
  transport.
- Read shape: same hashes, individual reads, concurrency 8.

Results:

| Shape | Result |
| --- | ---: |
| Fresh public binary-batch upload | 9.97 MiB/s, 128/128 succeeded |
| Public reads of the same blobs | 22.72 MiB/s, 128/128 succeeded |
| Hot-origin upload replication queue after sample | 0 queued, 0 in flight, 0 failed |

Interpretation:
- The public path is no longer in the pathological sub-MiB/s state seen during
  the earlier incident, and the origin accepted the batch workload without
  queue buildup.
- This sample is still far below origin-local write throughput observed in the
  same runbook period, so it should not be treated as storage-engine capacity.
  It is a public ingress/path measurement.
### 2026-07-20: Nostr crawl checkpoint I/O amplification on Vader

Question: why did a four-author content checkpoint stop advancing for hours
after the index reached roughly 2.55 million retained events?

Setup:
- Content and compact-profile crawlers used separate LMDB directories on the
  same mirrored HDD array, each in an independently capped systemd cgroup.
- The content checkpoint started from durable author 264 with 2,549,398
  selected events and applied at most 65,536 retained events per author.
- Process wait state, cgroup pressure, `/proc` I/O counters, block-device
  telemetry, and checkpoint phase timings were sampled without opening either
  active LMDB.

Results:

| Observation | Result |
| --- | ---: |
| HDD array utilization during the content index phase | 100% |
| Array workload | about 380 random 4 KiB reads/s, about 1.5 MiB/s |
| Aggregate full I/O pressure during the sample | about 56% |
| Content cgroup full I/O pressure during the sample | about 49% |
| Content process state | blocked in `folio_wait_bit_common`, reads advancing |
| Relay fetch time in completed large checkpoints | about 68 seconds |
| Index-build time in the author-200 to author-264 checkpoint | about 3,211 seconds |

Interpretation:
- The crawler was active rather than deadlocked. The bottleneck was random
  content-addressed B-tree reads on the HDD array, not relay throughput, CPU,
  LMDB commit size, or a shared memory ceiling.
- An 8,192-event index commit repeated the collection's by-id lookup: Nostr
  event preparation proved new ordinary event IDs absent, then
  `CollectionWriter::put_batch` walked the same by-id paths again. The crawl
  now passes those proven-absent IDs into the collection update.
- The crawler's configurable default is now a still-bounded 32,768-event
  index commit with 256-way B-tree nodes. This reduces repeated walks and
  gradually rewrites touched paths into shallower trees without loading the
  complete event catalog into memory.

### 2026-07-22: Disk-backed bulk Nostr projection

Question: can a multi-million-event archive rebuild avoid rewriting and
garbage-collecting nine immutable B-trees after every small durable batch?

Setup:
- The original production projector checkpointed every 1,024 retained events,
  incrementally updating all nine query indexes with B-tree order 64 and one
  update worker.
- The replacement stores event blobs normally but resolves replaceable winners
  and all ordered index entries in a durable LMDB spool. It constructs each
  final immutable B-tree once after the staged prefix has been consumed.
- Version 0.2.134 stores arbitrary-length logical index keys as 400-byte trie
  edges, avoiding LMDB's physical key-size ceiling while retaining exact
  bytewise output order.

Results:

| Observation | Incremental projector | Bulk-v2 projector |
| --- | ---: | ---: |
| Typical 1,024-event checkpoint wall time | 30–50 s | 0.4–0.6 s initially |
| Selected-event rate in representative live intervals | about 27 events/s | about 1,800 events/s |
| Reads before the incremental run stopped | about 2.53 TB | 13.35 GB in an early roughly 190-checkpoint sample |
| Writes before the incremental run stopped | about 2.35 TB | 53.63 GB in the same early sample |
| Durable logical spool size in that bulk sample | not applicable | about 1.2 GB |
| Long tag and parameterized-replaceable regression coverage | failed above LMDB key limit | passed at 1,024 bytes |

Interpretation:
- Repeated immutable-tree traversal, copy-on-write construction, and
  superseded-generation garbage collection caused the pathological reads and
  most of the wall time. The bulk path removes that work from each checkpoint;
  ordinary checkpoints now append to the spool and the final trees are streamed
  once in sorted order.
- The early bulk write count is still substantially above logical spool growth
  because the crash-resumable path commits and synchronizes every 1,024 events.
  Larger durable batches may reduce that residual amplification, but must be
  measured separately rather than inferred from the initial recovery run.
- The bulk run must retain strict single-writer ownership of its target and a
  frozen or independently verified staged prefix. Final-tree construction and
  publication remain closed-store validation steps.

### 2026-07-26: Pack-aware PoolStore drain commit batching

Question: how much of PoolStore evacuation time comes from committing one
external file and several LMDB transactions per blob rather than from reading,
hashing, and verifying the payloads?

Setup:
- Two fresh real PoolStore members were created on local storage.
- The source held 256 unique 32 KiB blobs (8 MiB total).
- The target used synchronized external blobs with a 64 MiB pack target.
- The serial production relocation path and the bounded packed production path
  each read, hash-verified, copied, target-verified, and deleted the same data.
- The packed path used one 256-item batch. This was a single local debug-build
  comparison, not a production-array benchmark.

Results:

| Path | Wall time |
| --- | ---: |
| Serial loose-file relocation | 8.190 s |
| Bounded packed relocation | 0.659 s |
| Observed ratio | 12.4x |

Interpretation:
- Payload I/O and verification were held constant. The measured improvement
  came from replacing per-blob external-file synchronization and catalog/member
  transactions with one durable pack write and bounded batch transactions.
- Production performance will depend on blob-size distribution and storage
  latency. The local ratio should not be treated as a promised server speedup;
  the structural bound is up to 256 blobs and at most 64 MiB per batch.

### 2026-09-05: Core tests without DOM emulation

Question: does the core TypeScript suite need its per-worker DOM environment?

The active core tests use native Node APIs and do not use DOM APIs. The excluded
direct-navigation script runs a separate Playwright browser. Compared the same
20 files and 324 tests with Vitest 4.1.10 on Node 25.5.0, running happy-dom, Node,
Node, then happy-dom to include a reverse-order comparison. The loader test file
was excluded from this timing comparison because its regressions were being
updated concurrently.

| Environment | First run | Second run | Aggregate worker environment setup |
| --- | ---: | ---: | ---: |
| happy-dom | 3.75 s | 2.54 s | 6.68 s / 4.99 s |
| Node | 2.23 s | 2.04 s | 1 ms / 2 ms |

All four runs passed all 324 tests. Removing the environment override uses
Vitest's default Node environment and permits removing happy-dom and three
otherwise unused transitive packages. These local runs show the eliminated setup
cost; the small sample and concurrent work do not establish a stable CI speedup.


### 2026-09-05: Core property tests without per-case Tokio runtimes

Question: can the in-memory tree and diff properties run faster without reducing
randomized coverage?

Setup:
- Run the seven properties in `formal_diff_props` and `formal_tree_props`, plus
  the existing wrong-key regression, using 256 cases per property and RNG seed
  `20260905`. Run each test binary serially (`--test-threads=1`).
- Compare three warm debug-binary runs before and after replacing per-case Tokio
  worker-pool startup with the existing `futures::executor::block_on`. These
  production paths use `MemoryStore` and an in-memory cursor; they require no
  Tokio timers, tasks, or I/O driver.
- The after build also consolidates collected and streamed diffs and removes
  repeated hash lookups and store reference-count updates. Compilation is
  excluded; this is a local test-suite measurement, not a production benchmark.

| Combined suites | Run 1 | Run 2 | Run 3 | Median |
| --- | ---: | ---: | ---: | ---: |
| Before | 7.140 s | 6.003 s | 6.257 s | 6.257 s |
| After | 5.340 s | 5.213 s | 6.012 s | 5.340 s |

The median fell about 15%, with all generated cases and assertions retained.
Individual runs varied with local load; the durable improvement is eliminating
1,792 unnecessary runtime creations and their worker threads per suite run.


### 2026-09-09: Mesh reader recovery and bounded multihop probes

Question: can Hashtree reads survive independent reader cancellation, exhausted
hop budgets, corrupt providers, cycles and provider departure using the current
production routing path?

The retired `hashtree-sim` mesh simulator is no longer production-relevant.
These probes compose the real `BlobRouter`, `MeshForwardingRoute`, `MemoryStore`
and blob wire codec. Only the carrier is simulated. Counts exclude FIPS/TCP and
underlay overhead; these are deterministic routing checks, not WAN throughput
or latency benchmarks.

Cancellation regression: one stalled mesh request owned the shared read while
16 independent readers joined it. Cancelling the owner formerly failed all 16
readers. After the fix one remaining reader takes over; the rest share it.
The replacement uses the original request's HTL and each reader's own deadline.
An additional regression verifies that an owner's earlier deadline can expire
without shortening a later reader's search. Ordinary results and failures stay
shared; no detached work is created.

| Cancellation result | Before | After |
| --- | ---: | ---: |
| Remaining readers receiving verified 4 KiB payload | 0 / 16 | 16 / 16 |
| Carrier attempts, including the cancelled first attempt | 1 | 2 |

Directed-chain probes verify exact HTL decrement and stop one hop short without
contacting the provider. All five sufficient-budget reads succeeded; all five
insufficient-budget reads returned explicit no-result without reaching it.
Every intermediate cache contained the verified payload. With the original
provider removed, an HTL-0 local read still succeeded without carrier traffic.

| Mesh hops | Cold blob-wire bytes | Ordinary warm-read bytes | Cached HTL-0 read bytes |
| --- | ---: | ---: | ---: |
| 1 | 4,139 | 4,139 | 0 |
| 2 | 8,278 | 8,278 | 0 |
| 4 | 16,556 | 16,556 | 0 |
| 8 | 33,112 | 33,112 | 0 |
| 10 | 41,390 | 41,390 | 0 |

A cyclic diamond tried a corrupt first provider, suppressed the cycle, and
retrieved correct content in four carrier attempts and 12,460 blob-wire bytes.
After removal of the original provider and requester cache, a surviving
intermediate cache served the next read in one hop and 4,139 bytes. Corrupted
content never entered the requester or intermediate cache.

The warm-read measurement exposes an unresolved optimization: the generic
router writes its cache but reads it only through the application's ordinary
routes. After a mesh success, adaptive ranking can put that route above the
previously missing local route, so a warm read repeats all network hops. The
router deliberately treats routes and stores as opaque, including cache stores
that may themselves have remote policies; a follow-up should make applications'
local-read preference explicit rather than infer locality from a route name.

Real carrier validation at the baseline revision also passed:
- 22 `hashtree-fips-transport` tests, including concurrent >1 MiB transfer,
  corrupt-content rejection, provider hedging, provider death/replacement and
  stream cancellation.
- The CLI daemon's real three-node FIPS test observes HTL 2→1→0 and rejects
  exhausted forwarding. Its local test execution took 0.23 s, excluding build;
  this is test duration, not a measured WAN latency.

The updated routing suite has 20 passing tests. The shared-forward state keeps
its existing 1,024-entry bound and cycle suppression. The fix also merges the
tracked and untracked forwarding execution branches to keep one dispatch path.


### 2026-09-09: Prefer the daemon store without blocking mesh hedges

Follow-up to the preceding topology measurement: the CLI now supplies its
existing configured-store preference on both incoming FIPS blob requests and
in-process FIPS reads. This is an application policy using the router's existing
preferred-route argument; the generic router's opaque route/cache policy and
public resolver type remain unchanged. The browser worker already uses the
same approach to prefer IndexedDB.

A real three-node FIPS regression retrieves a 4 KiB blob through an intermediate
daemon, then repeats the read. Before the change, the intermediate fetched the
blob upstream again despite caching it. The updated regression confirms one
upstream retrieval across both reads, while HTL 2→1→0 and exhaustion behavior
remain intact.

| Warm-read measurement | Before | After |
| --- | ---: | ---: |
| Mesh links traversed by repeat read | 2 | 1 |
| Blob-wire bytes per repeat read | 8,278 | 4,139 |
| Repeat traffic on intermediate-to-source link | 4,139 | 0 |

Blob-wire bytes are derived from observed request counts and the shared codec's
36-byte request, 7-byte reply header and 4,096-byte payload. FIPS/TCP/underlay
framing and retransmissions are excluded. The caller in this test is deliberately
a raw peer route, so it still contacts the intermediate on every read.

The configured store may synchronously read disk or fall back to S3. Running
that preferred read inline would prevent Tokio from polling hedge/deadline
timers. The private CLI adapter therefore reuses the server's existing bounded
blocking-read queue. A single-thread runtime regression blocks the store while
a healthy alternate route wins after the configured 5 ms hedge; completion
must stay below the generous 500 ms test bound. A second test confirms an
incoming 20 ms deadline remains an error, not a false miss.

A started blocking read can outlive the caller, just as existing server reads
can. The reused queue keeps its permits until the closure ends, so cancellation
cannot admit unlimited replacement disk/S3 work. Existing queue-admission and
error/no-result integration tests also pass. No public API or second queue was
added, and no generic cache-first behavior was introduced.

Validation: 23 CLI tests matching `fips` and all four storage admission/queue
tests passed; the production CLI library passes strict Clippy with dependency
linting excluded. Strict all-target lint remains blocked by unrelated existing
vendored-Heed, storage-test and pool-migration warnings; those were left outside
this routing change.

### Browser cache byte accounting (2026-11-16)

A fresh Chrome profile stored 256 blocks of 128 KiB each. The worker checked
a 64 MiB limit and then a 24 MiB limit. Counting the payload bytes returned by
IndexedDB during each check gave 32 MiB and 72 MiB with Dexie 0.1.13. The same
checks with the transactional counter returned zero payload bytes. Both runs
kept 256 blocks at the first limit and 192 at the second. This measures quota
accounting and eviction on a disposable block cache. Walking protected event
indexes and admitting new relay events have separate costs.

### Browser relay event indexing (2026-11-16)

In a fresh Chrome database, the worker ingested the same 200 signed video
events through the relay runtime, then queried all 200 in timestamp order.
The portable index took 5,398 ms and issued 6,468 file-block writes. The
opt-in IndexedDB event cache took 373 ms and issued zero file-block writes.
Neither timing includes fixture signing or the final query. With an existing
portable index, a separate run read back 80 old events together with 120
new arrivals without rewriting the old index.

### Native Hashtree peer-credit retrieval (2026-11-19)

The Rust release build bought 48 distinct 256-KiB blobs at two sats each
through an authenticated in-process carrier, using real Schnorr signatures,
two SQLite journals and the production buyer and seller. It then read all
48 blobs from its local memory store. We compared a prefix frame that
allocated and copied the seller’s payload and shifted it on the buyer with
the shipped suffix frame, which the buyer removes by truncating its buffer.
The same Linux VM ran five release tests per format with SQLite on its
disk, pinned to one CPU. The table reports medians.

| Across 48 purchases and 48 repeated reads | Prefix | Suffix |
| --- | ---: | ---: |
| First purchase of each blob | 3.52 s | 3.15 s |
| Repeated reads | 6.04 ms | 6.78 ms |
| User CPU in the whole test | 0.19 s | 0.16 s |
| System CPU in the whole test | 0.08 s | 0.05 s |
| Peak process RSS | 36,628 KiB | 36,628 KiB |
| Application bytes exchanged | 12,744,594 | 12,744,594 |
| Bytes occupied by journal files, including WAL, before close | 8,703,488 | 8,703,488 |
| File-system output (512-byte blocks) | 30,064 | 30,064 |

Every run produced all 48 hash-verified blobs and settled 96 sats of
issuer-local credit in 144 exchanges. Repeated reads sent zero frames.
The cold median fell 10.5%. Cold runs varied from 3.03–3.87 seconds with
the prefix and 2.94–4.43 seconds with the suffix. Cache code was identical
in both versions. A separate tmpfs run took 110 ms versus 103 ms for the
same 48 purchases. Durable journal operations dominate cold retrieval
on disk. The journal footprint reflects rewriting and retaining account
snapshots as their history grows. The timings exclude network transit and wallet payouts. Use the separate
`private_credit_over_fips_nets_against_vpn_service` integration test for a
real encrypted local UDP transfer and reciprocal settlement against VPN
service. To repeat the shipped benchmark, run the following command in
the Rust workspace with `TMPDIR` on the storage being measured.

```sh
cargo test --locked --release -p hashtree-paid-retrieval --features peer-credit --lib credit::tests::measure_purchases_and_cache_hits -- --nocapture --test-threads=1
```


A separate two-process smoke check ran the native Hashtree CLI and FIPS
relay over authenticated loopback UDP. A seeded, mutually signed Hashtree
blob claim had six sats outstanding. The normal Hashtree CLI showed the
five-sat invoice, one-sat conversion fee and six-sat total before its
confirmation prompt. It verified the receiver-side fixture invoice and the
FIPS journal recorded one sat in converter earnings, one msat in network
costs and zero remaining obligations. This run took 1.52 seconds including
connection, confirmation and an automatic retry of the existing payment.
The CLI used 0.055 seconds of user CPU, 0.023 seconds of system CPU and
76.8 MiB peak RSS. No real Lightning funds were used.


The automatic withdrawal check used the native Hashtree daemon startup path,
the FIPS relay executable and a wallet RPC fixture. With a five-sat payment
and a one-sat fee ceiling the daemon requested one labeled invoice. The
first attempt left the payment pending. The following 60-second tick
reconciled it, the receiving wallet showed paid, and the relay accounted
for its fee. End-to-end time was 60.28 seconds.


A separate three-process regtest used Hashtree as creditor, FIPS as the
credit-conversion hop and nostr-vpn as the Lightning payer. Two Core
Lightning wallets independently recorded the 5,000-msat payment. All three
credit journals closed against the same preimage. The two starting bilateral
service balances were fixture-seeded. Hashtree spent six credit sats, FIPS
earned one forwarding sat and the direct Lightning channel required zero
network fees. The scripted quote and confirmation through verification took
0.714 seconds on that run.

### 2026-11-26: Stop TCP blob work when its caller cancels

The caller cancellation test previously replaced the peer fetch with a promise,
so it checked the returned error without observing the real TCP connection. A
real endpoint fixture showed that cancellation sent no TCP abort and left the
handshake polling until its deadline. The corrected path propagates the caller
signal through connection setup, reads, writes and polling waits. It aborts the
affected connection exactly once and performs no further polls after cancellation;
an independent request remains active. A delayed connection is also aborted before
it sends a blob request. The 72 transport tests, including Rust interoperability,
pass with the unchanged timeout and retry policy.
