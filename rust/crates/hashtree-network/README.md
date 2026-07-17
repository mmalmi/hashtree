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
behavior. Terminal and carrier routes do not use it.

The removed DataQuote/DataChunk protocol was never invoked by a production
Hashtree read path; its only paid-retrieval caller was the simulator. Cashu
wallet commands remain, but this crate deliberately configures no paid
`BlobRoute`. A future paid provider must own negotiation and replay protection
inside one opaque route without changing the published blob wire.

The retired WebRTC/raw-frame mesh, duplicate blob request/response codec, and
private pubsub carrier are intentionally not part of this crate. FIPS owns
transport addressing and `nostr-pubsub` owns Nostr event distribution.
