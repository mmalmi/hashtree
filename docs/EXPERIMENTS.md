# Experiments

This file records performance and behavior experiments without identifying data. Do not store pubkeys, secrets, IP addresses, private hostnames, exact private repo names, or raw content hashes here unless explicitly requested.

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
