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
