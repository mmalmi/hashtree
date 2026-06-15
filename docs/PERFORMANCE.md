# Hashtree public edge performance

This note summarizes the current operational model for public hashtree and
Blossom traffic. Detailed measurements belong in `docs/EXPERIMENTS.md`.

## Current model

- Public reads should use the CDN/read hostname and extensionful content-addressed
  blob URLs, such as `/<sha256>.bin`, whenever possible.
- Public writes should go to the upload hostname backed by the local hashtree
  origin. The preferred hot path is local fileserver and local storage capacity,
  not an R2/S3 object-store admission layer.
- Cloudflare Worker upload handlers should not be reintroduced for bulk Blossom
  writes unless a future benchmark shows a clear win. The measured Worker and
  Workerless tunnel paths were both far below local-origin write capacity.

## Known bottleneck

Public bulk upload throughput is currently limited before LMDB and the local
blob writer. Local-origin writes have reached tens of MiB/s in experiments,
while public request-body uploads through Cloudflare Tunnel have clustered in
the low single-digit MiB/s range under realistic load.

Raising the local blob write queue is therefore not the main fix once the origin
queue is idle. Higher client concurrency mostly increases tail latency when the
public ingress path is saturated.

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

## Write path fixes

Useful write-side improvements reduce bytes, object fanout, or repeated request
setup before traffic reaches the public ingress ceiling:

- Use packed Git checkpoints and tail packs for medium git pushes instead of
  hundreds of loose Git objects.
- Keep batch bodies large enough to amortize per-request overhead, but do not
  expect larger batches to beat a saturated tunnel by themselves.
- Prefer long-lived upload streams or pack/tail-pack admission for future
  protocol work.
- For a large step-function improvement, provide direct public TCP ingress to the
  local fileserver through a Cloudflare-proxied origin path restricted to
  Cloudflare source IPs, or an equivalent direct ingress design.

Cloudflare Tunnel remains useful for reachability and simple operations, but it
should be treated as the fallback public ingress for bulk writes rather than the
target architecture for modern upload throughput.

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

## Evidence

See `docs/EXPERIMENTS.md` for measured Worker, Workerless tunnel, HTTP/2 tunnel,
direct-ingress, write-queue, read-cache, and git pack/tail-pack experiments.
