# Hashtree public edge performance

This note summarizes the current operational model for public hashtree and
Blossom traffic. Detailed measurements belong in `docs/EXPERIMENTS.md`.

## Current model

- Public reads should use the CDN/read hostname and extensionful content-addressed
  blob URLs, such as `/<sha256>.bin`, whenever possible.
- Public writes should go to the upload hostname backed by the local hashtree
  origin. The preferred hot path is local fileserver and local storage capacity,
  not an R2/S3 object-store admission layer.
- The current public upload/read hostnames are intended to be plain
  Cloudflare-proxied DNS to a normal reverse proxy, then to the hashtree daemon.
  Do not assume the active write path is a Cloudflare Worker or cloudflared
  Tunnel without rechecking current deployment.
- Cloudflare Worker upload handlers should not be reintroduced for bulk Blossom
  writes unless a future benchmark shows a clear win. The measured Worker and
  Workerless tunnel paths were both far below local-origin write capacity.
- The preferred public edge shape is plain Cloudflare proxy/cache in front of a
  normal reverse proxy. Put host-specific cache, body-size, and routing policy on
  that reverse proxy where it can be observed and changed without Worker
  redeploys.
- S3/R2 support, if kept at all, should be explicit migration/archive tooling,
  not a hidden public read/write hot path. Removing that optional code is a
  simplification task, not a performance prerequisite for the current local
  origin design.

## Known bottleneck

Public bulk upload throughput is currently limited before LMDB and the local
blob writer. Origin-local release-mode writes have reached roughly 100 MiB/s
for public-shaped binary batches, while public request-body uploads from an
outside client have ranged from roughly 3-10 MiB/s depending on edge/client load.
A same-host probe through the public hostname reached about 24 MiB/s, so the
current gap is split between the Cloudflare/public-origin path and the outside
client's path to that edge. In the current deployment this is not explained by
Worker logic or an active cloudflared tunnel; the remaining limit is the
client-to-public-origin network path plus Cloudflare proxy/TLS/body handling.

Raising the local blob write queue is therefore not the main fix once the origin
queue is idle. Higher client concurrency mostly increases tail latency when the
public ingress path is saturated. The default Blossom upload concurrency is kept
conservative (`4`) for public pushes; private or same-LAN origins can raise
`upload_concurrency` in config after measuring their own path.

Request timing on the reverse proxy should include `$request_time`,
`$request_length`, and `$upstream_response_time` while debugging public upload
ceilings. With streaming proxying enabled, request time and upstream time will
track together because the origin receives the client body as it arrives; compare
those timings with origin-local benchmarks before blaming LMDB.

The public reverse proxy should also have enough file-descriptor and worker
connection headroom for websocket traffic plus upload/CDN bursts. A container
default such as 1024 open files or 1024 worker connections is unnecessarily low
for this role; tune nginx worker limits and keep access-log timing enabled so
connection pressure is visible before it becomes an upload/read symptom.

For Cloudflare-to-origin HTTP/2 uploads, keep request-body buffers large enough
to avoid tiny default preread behavior: a tested nginx baseline uses
`http2_body_preread_size 1m` and `client_body_buffer_size 1m` on the upload
server, plus modern origin TLS (`TLSv1.2 TLSv1.3`). This improved one saturated
c12 binary-batch probe, but did not remove the public ingress ceiling.

Client upload transports should prefer HTTP/1.1 for request bodies unless a
specific target proves HTTP/2 is better. Public `upload.iris.to` benchmarks found
HTTP/1.1-only uploads matched or slightly beat HTTP/2-auto in the stable part of
the sweep, and avoided at least one bad HTTP/2-auto sample. Reads still use the
normal HTTP client and may negotiate HTTP/2.

For diagnostics, compare three paths before changing architecture: public
hostname, direct-origin override with the same TLS name, and origin-local htree.
If public and direct-origin uploads are both slow while origin-local htree is
fast, DNS-only upload routing will not solve the problem by itself; the remaining
limit is the client-to-origin network path and request-body ingress.

LMDB duplicate handling is already exact enough for the hot Blossom write path:
single puts and batch puts rely on LMDB `NO_OVERWRITE`, duplicate writes do not
refresh blob access metadata, and quota uses actual inserted bytes. A local
release-mode cached-batch replay processed duplicate-heavy candidates at roughly
170 MiB/s, far above the public upload ceiling. Do not add Bloom filters,
probabilistic admission checks, or R2/S3 bucket layers for this symptom without
new evidence that local storage has become the bottleneck.

## Read path

Cloudflare's default cache behavior is extension-based, not MIME-type based.
For content-addressed blobs, prefer URLs with a cacheable extension:

- Good: `https://cdn.iris.to/<sha256>.bin`
- Avoid for hot reads: `https://cdn.iris.to/<sha256>`

The server still returns immutable cache headers for blob responses, but an
extensionless hash URL can miss Cloudflare's default cache eligibility unless a
Cache Rule explicitly covers it. Cache Rules are suitable for read hostnames when
their match condition is limited to immutable content-addressed blob paths and
does not include mutable tree, API, or status routes.

The upload hostname can also serve immutable hash GETs when clients reuse a
Blossom upload URL as a blob URL. Before adding origin-side cache complexity,
check the live headers: `cache-control: public, max-age=31536000, immutable` and
`cf-cache-status: HIT` are enough evidence that Cloudflare is already caching
that object at the edge.

Cold read-through from a deep upstream should use hashtree's batch blob read
extension when both sides support it. The hot origin asks for many missing
content hashes with one `POST /blob/batch`, verifies every returned blob, caches
the accepted blobs locally, and falls back to ordinary single-hash GETs for
unsupported or missing blobs. This is a transport optimization over the same
local content-addressed stores; it is not an R2/S3/bucket admission layer.

## Write path fixes

Useful write-side improvements reduce bytes, object fanout, or repeated request
setup before traffic reaches the public ingress ceiling:

- Use packed Git checkpoints and tail packs for medium git pushes instead of
  hundreds of loose Git objects; underfull packs are byte-gated so tiny-object
  edge cases do not install a pack+idx that is larger than the zlib-compressed
  loose-object bytes it replaces.
- Use compact `x-batch` authorization for multi-blob binary batches. Signing
  one digest for the ordered blob-hash list keeps Authorization headers small
  while the origin still verifies every uploaded blob hash from the body.
- Keep hot-origin write-behind replication bounded and coalesced. The public
  hot store should durably accept and serve blobs first, then merge adjacent
  replica jobs into larger batch-binary uploads so the deep store link is not
  forced to process one replica request per accepted client request.
- Keep batch bodies large enough to amortize per-request overhead, but do not
  expect larger batches to beat saturated public ingress by themselves. Current
  public measurements make 4-16 MiB binary batch bodies the safe operating
  range; 32 MiB bodies have produced edge 520 failures before reaching origin
  even after compact batch authorization removed per-blob auth-header growth.
- A generic framed upload stream was tested and did not materially outperform
  binary batch; do not re-add that endpoint just to reduce request count.
  Prefer pack/tail-pack admission or other git-aware byte/fanout reductions
  where a benchmark shows a material win.
- For a large step-function improvement, provide direct public TCP ingress to the
  local fileserver through a Cloudflare-proxied origin path restricted to
  Cloudflare source IPs, or an equivalent direct ingress design.

Cloudflare Tunnel remains useful for reachability and simple operations, but it
should be treated as the fallback public ingress for bulk writes rather than the
target architecture for modern upload throughput.

## Datacenter proxy and hot origin

A public datacenter reverse proxy can remove Cloudflare Worker and Tunnel from
the public request-body path while still keeping deep storage behind a private
link. This is simpler and easier to operate, but it is not enough by itself when
the private proxy-to-storage link is slower than the storage engine.

For modern public writes, the datacenter host should become a real hot
hashtree/Blossom origin with local storage, bounded disk use, and background
replication to the deep storage host. A pure forwarding proxy still sends every
upload byte across the private link before the client gets success, so its
throughput is capped by that link.

The hot-origin design must preserve read-after-write semantics for public
clients: `/upload/check`, `/upload/batch*`, and CDN/read paths need to agree
about newly accepted blobs before publishing mutable roots that reference them.
Background replication is acceptable only after the public hot store has durably
accepted the blob and can serve it.

## Direct ingress options

When the local origin sits behind a home NAT with no static IP, choose the
simplest public path that keeps the origin private:

- If the router has a real public WAN IP, use dynamic DNS plus a normal 443
  port-forward to a local reverse proxy. Keep the Cloudflare DNS record proxied,
  and firewall the reverse proxy so only Cloudflare source IP ranges can connect.
- If the router is behind CGNAT or inbound forwarding is not possible, put a
  reverse proxy on a public host and carry the private hop to the local origin
  over WireGuard or an equivalent authenticated tunnel. The public host should
  terminate Cloudflare-facing HTTPS and proxy to the local hashtree origin over
  the private link.
- Do not publish a separate `upload-direct` hostname for normal clients. Keep
  `upload.iris.to` as the public write endpoint and change only its underlying
  origin path.

In both designs, expose the reverse proxy rather than the raw hashtree daemon,
keep SSH private, and avoid public high-port URLs. Public `https://upload.iris.to`
should remain ordinary HTTPS on port 443.

## Origin TLS

A static home IP is not required for the origin certificate. When the public DNS
record is proxied through Cloudflare, browsers see Cloudflare's edge certificate;
the origin certificate only needs to be trusted by Cloudflare for the hostname.

Preferred origin TLS options:

- Use a Cloudflare Origin CA certificate on the reverse proxy and keep the
  Cloudflare SSL/TLS mode at Full (strict). This is ideal when the origin only
  accepts Cloudflare source IPs.
- Alternatively, let the reverse proxy obtain a normal public ACME certificate
  after the direct 80/443 origin path is reachable. DNS-01 ACME also works with
  dynamic home IPs if the DNS provider token can edit the required TXT records.

Do not expose the raw hashtree daemon to the Internet for certificate issuance.
TLS should terminate at the reverse proxy, which then proxies to the daemon over
localhost or a private link.

## Evidence

See `docs/EXPERIMENTS.md` for measured Worker, Workerless tunnel, HTTP/2 tunnel,
direct-ingress, write-queue, read-cache, and git pack/tail-pack experiments.
