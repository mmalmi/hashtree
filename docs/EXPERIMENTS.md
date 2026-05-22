# Experiments

This file records performance and behavior experiments without identifying data. Do not store pubkeys, secrets, IP addresses, private hostnames, exact private repo names, or raw content hashes here unless explicitly requested.

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
