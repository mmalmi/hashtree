# Hashtree on FIPS

Hashtree uses FIPS for authenticated identity, discovery, routing, and
node-to-node datagrams. Blob exchange runs as a reliable TCP/FIPS service above
those datagrams, so Hashtree does not copy discovery, retransmission, or
underlay-specific logic.

## Scope

Public browser providers join the shared FIPS discovery fabric:

```text
fips-overlay-v1
```

Private deployments can select another discovery app. Browser and native
providers should normally share the generic FIPS fabric instead of creating a
parallel Hashtree-only WebRTC island.

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

## Result Semantics

Transport uncertainty must not become false absence:

- `null` means every attempted provider explicitly reported missing.
- No providers is an availability error.
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

The package gate builds the core and transport packages, checks shared Rust/TS
wire vectors and strict absence semantics, runs a real two-node FIPS worker
fetch, lints the package, and regenerates the tracked distribution. Additional
cross-runtime and browser/native scenarios belong in the full-stack integration
lab.
