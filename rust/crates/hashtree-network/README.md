# hashtree-network

`hashtree-network` provides the generic, read-only `BlobRouter` used by
Hashtree applications. Routes are opaque: each route owns any peer selection,
transport, terminal-store, or remote-provider policy behind it.

The router honors explicit route preferences, keeps bounded decaying runtime
outcomes, cools down transport failures, explores recovered routes, and
centrally verifies every successful payload before return or cache write.
Writes remain explicit through the application's configured `Store`.

The retired WebRTC/raw-frame mesh, duplicate blob request/response codec, and
private pubsub carrier are intentionally not part of this crate. FIPS owns
transport addressing and `nostr-pubsub` owns Nostr event distribution.
