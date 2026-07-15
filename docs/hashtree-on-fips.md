# Hashtree on FIPS

The Rust `hashtree-fips-transport` crate and TypeScript
`@hashtree/fips-transport` package use FIPS for authenticated identity,
discovery, routing, and node-to-node datagrams. Blob exchange runs as a reliable
TCP/FIPS service above those datagrams, so the adapters do not copy discovery,
retransmission, or underlay-specific logic.

## Scope

Public browser providers join the shared FIPS discovery fabric:

```text
fips-overlay-v1
```

Private deployments can select another discovery app. Browser providers should
normally share the generic FIPS fabric instead of creating a parallel
Hashtree-only WebRTC island.

## Blob Service

`TcpBlobTransport` uses FIPS service port `39018`. TCP/FIPS owns ordered byte
delivery, flow control, and segment retransmission. Hashtree owns provider
choice, one bounded whole-session retry, hash verification, and cache writes.

The client sends a fixed 35-byte GET request containing a 32-byte SHA-256 hash.
The provider returns a seven-byte header that explicitly says found or missing;
a found header is followed by the blob. Implementations reject blobs above 16
MiB and verify the requested hash before returning or caching them. The exact
wire format and shared vectors are in
[`tcp-fips-blob-v1.md`](tcp-fips-blob-v1.md).

## Optional Same-host Store

Rust `SameHostBlobStore` wraps an ordinary local `Store`. Local writes,
deletes, pins, limits, and statistics never depend on another process. On a
local miss it reads FIPS's authenticated in-memory capability roster, races at
most four ranked `hashtree.blob/1` providers, accepts only a hash-valid result,
and caches it while preserving existing pins.

The provider uses fips-tcp's capability-aware listener bind, so the capability
appears only after the FSP port is owned and disappears with the listener.
Client-only stores reject every inbound blob session. FIPS's fixed loopback UDP
rendezvous (`127.0.0.1:21211` by default) and ordinary Noise IK establish the
links; this adapter adds no filesystem registry, bootstrap protocol,
shared-egress role, or fallback blob framing.

## Result Semantics

Transport uncertainty must not become false absence:

- For an explicit transport query, `null` means every attempted provider
  explicitly reported missing; no providers is an availability error.
- For the optional same-host Store wrapper, no advertised local provider is an
  ordinary standalone miss. Once a provider attempt begins, timeout, reset, or
  a mixed miss/failure result remains an error.
- Timeouts, resets, malformed responses, and mixed miss/failure results remain
  availability errors after the bounded retry.
- A hash mismatch is an error and poisoned bytes are never cached.

These rules let callers continue to another source without recording a slow or
broken peer as proof that content does not exist.

## Browser And Worker Providers

`createBrowserHashtreeFipsProvider(...)` starts a FIPS node with the WebRTC
adapter and exposes the `HashtreeWorkerClient` P2P-provider surface. The worker
provider tracks authenticated FIPS peers and delegates blob reads to
`TcpBlobTransport`; it does not maintain another discovery mesh.

FIPS WebRTC remains only an underlay beside UDP, native TCP, Tor, or future link
adapters. TCP/FIPS reliability is end-to-end between FIPS identities and does
not require the underlay itself to provide ordered delivery.

## Verification

The TypeScript package gate builds the core and transport packages, checks TCP
blob vectors and strict absence semantics, runs a real two-node FIPS worker
fetch, lints the package, and verifies the tracked distribution. The Rust gate
adds real FIPS endpoint streams, bounded concurrency, client-only serving,
same-host discovery, withdrawal, multi-provider failure semantics, and cache
repair. A bidirectional Rust/TypeScript process gate exchanges small and
multi-segment hits plus explicit misses over real FIPS UDP and TCP/FIPS. Broader
product scenarios belong in the full-stack integration lab.
