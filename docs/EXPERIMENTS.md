# Experiments

This file records performance and behavior experiments without identifying data. Do not store pubkeys, secrets, IP addresses, private hostnames, exact private repo names, or raw content hashes here unless explicitly requested.

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
