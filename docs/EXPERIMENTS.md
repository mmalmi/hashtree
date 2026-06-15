# Experiments

This file records performance and behavior experiments without identifying data. Do not store pubkeys, secrets, IP addresses, private hostnames, exact private repo names, or raw content hashes here unless explicitly requested.

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
