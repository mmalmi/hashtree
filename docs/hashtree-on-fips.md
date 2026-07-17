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

The client sends a fixed 36-byte `BlobRequest { hash, htl }`. The provider
returns a seven-byte header for `BlobReply::Data(bytes)` or
`BlobReply::NoResult`; a Data header is followed by the blob. NoResult says only
that this route produced no data. It is neither proof of global absence nor a
cacheable negative result. Implementations reject blobs above 16 MiB and verify
the requested hash before returning or caching them. The exact wire format and
shared vectors are in
[`tcp-fips-blob-v1.md`](tcp-fips-blob-v1.md).

Direct same-host requests use HTL `0`. The existing HTL resolver is adapted as
one composite `BlobRoute`. Only forwarding from one Hashtree mesh peer to
another consumes one HTL; terminal adapters and FIPS routing hops do not. The
Hashtree forwarding wrapper coalesces equal in-flight requests and rejects a
lower-HTL re-entry for the same hash as a route-local miss, so a cycle does not
repeat provider work until exhaustion. This state is bounded and ephemeral;
HTL still bounds correctness when the tracking table is full.

## Same-host And Standalone Routes

Trusted same-user processes on one host normally open the application's shared
LMDB `PoolStore` directly. The pool is the one explicit write destination and
exclusively selects among its opaque storage members. The read-only `BlobRouter`
treats that complete pool as one route; it does not choose pool members or own
writes, deletes, pins, quotas, or garbage collection.
Pool placement and its bounded automatic temperature balancer are documented
in [`pool-store.md`](pool-store.md).

Applications that need the ordinary `Store` shape use
`hashtree_network::RoutedStore`: `get` and `has` use the hash-verifying router,
while every mutation, pin, capacity setting, and GC action delegates only to the
application's explicit primary store. It is not a write router.

When a process or host boundary is required, `FipsBlobRoute` owns the
authenticated in-memory capability roster and exclusively selects among its
`hashtree.blob/1` providers. The outer `BlobRouter` sees that provider set as
one composite route, passes a deadline and bounded attempt budget, accepts only
hash-valid data, and remains free to continue to another route.

The composite deliberately preserves the FIPS-era provider policy: capability
providers are ranked by FIPS discovery, explicit application peers are
deduplicated and interleaved, the bounded attempt budget truncates that union,
and the selected providers race with first valid data winning. FIPS owns
reachability and replacement; the outer router learns only the composite route's
outcomes. There is no second outer route or second selection owner for any peer.

The provider uses fips-tcp's capability-aware listener bind, so the capability
appears only after the FSP port is owned and disappears with the listener.
Client-only endpoints reject every inbound blob session. FIPS's fixed loopback
UDP rendezvous (`127.0.0.1:21211` by default) and ordinary Noise IK establish
the links; this adapter adds no filesystem registry, bootstrap protocol,
shared-egress role, write router, or fallback blob framing.

TypeScript follows the same boundary. `StoreBlobRoute` adapts a local `Store`,
while `@hashtree/mesh`'s read-only `BlobRouter` orders opaque route identities
and centrally verifies their data. The browser worker composes `idb`, `p2p`,
and `blossom` routes; IndexedDB remains its explicit write destination. The P2P
bridge and Blossom store each own their internal provider/server set, so the
outer router does not infer peers, duplicate provider selection, or route
writes. Inside the P2P composite, a nested `BlobRouter` creates stable routes
only for exact identities advertised by the configured provider. An empty
provider roster performs no P2P fetch.

## Result Semantics

Transport uncertainty must not become false absence:

- An explicit `BlobReply::NoResult` means only that one provider or route
  produced no data. It is never cached and does not cancel other routes.
- No provider, NoResult, timeout, reset, provider death, and malformed or
  corrupt replies remain local to the FIPS composite route. They do not suppress
  a shared-store, terminal, remote, or HTL-resolver route in the active search.
- A transport failure remains a failure for that route; it is not rewritten as
  NoResult or global absence. If the standalone route also fails, its error is
  returned.
- A hash mismatch is an error and poisoned bytes are never cached.

These rules let callers continue to another source without recording a slow,
broken, or empty peer as proof that content does not exist.

## Paid Retrieval Status

The retired DataQuote/DataPayment/DataChunk framing was not a released daemon
read path: repository history has no production caller of `get_with_quote`; its
only caller was the simulator. Hashtree therefore exposes Cashu wallet/config
helpers but, as of the 2026-07-17 native cleanup, configures no paid blob route.
The obsolete quote/chunk policy fields are accepted as unknown legacy TOML and
ignored.

A future paid provider belongs behind one opaque `BlobRoute` and must own its
quote, token transfer, replay protection, and recovery protocol. It must still
return the unchanged `BlobReply`, leaving central size/hash verification to
`BlobRouter`; it must not restore the deleted blob framing or peer selector.

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
