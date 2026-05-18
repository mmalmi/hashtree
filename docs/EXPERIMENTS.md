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
