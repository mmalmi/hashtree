# hashtree-network

`hashtree-network` provides the generic, read-only `BlobRouter` used by
Hashtree applications. Routes are opaque: each route owns any peer selection,
transport, terminal-store, or remote-provider policy behind it.

The router honors explicit route preferences, keeps bounded decaying runtime
outcomes, cools down transport failures, explores recovered routes, and
centrally verifies every successful payload before return or cache write.
Writes remain explicit through the application's configured `Store`.
`RoutedStore` exposes that split through the normal `Store` API: reads call the
router, while writes, deletes, pins, limits, and garbage collection delegate
only to the explicitly supplied primary store.

`MeshForwardingRoute` is the opt-in boundary for one Hashtree peer-forwarding
decision. It consumes exactly one HTL, coalesces equal in-flight work, and
suppresses lower-HTL cycle re-entry without changing `BlobRequest` or transport
behavior. If the owner is cancelled or exhausts an earlier deadline, remaining
readers can take over the shared attempt within their own deadlines. Terminal
and carrier routes do not use it.

The removed DataQuote/DataChunk protocol was never invoked by a production
Hashtree read path; its only paid-retrieval caller was the simulator. Cashu
wallet commands remain, but this crate deliberately configures no paid
`BlobRoute`. A future paid provider must own negotiation and replay protection
inside one opaque route without changing the published blob wire.

FIPS owns transport addressing and `nostr-pubsub` owns Nostr event
distribution.

Deterministic topology probes run the production router, forwarding adapter,
stores and blob codec over simulated carriers:

```sh
cargo test -p hashtree-network --test mesh_topology -- --nocapture
cargo test -p hashtree-network --test mesh_recovery -- --nocapture
```

The probes cover 1–10 hops, hop exhaustion, cycles, corruption, provider churn,
verified caches and coalesced-reader cancellation. Byte counts include the blob
wire only; real FIPS carrier coverage lives in `hashtree-fips-transport` and the
CLI daemon's `daemon_mesh_forwarding_observes_two_one_zero_and_exhaustion` test.
