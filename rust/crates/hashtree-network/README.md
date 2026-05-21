# hashtree-network

Shared mesh networking primitives for hashtree: routing, signaling, negotiated
peer links, routed store logic, and simulation support.

The core is transport-generic:

- `SignalingTransport` abstracts discovery and negotiation message delivery
- `PeerLink` / `PeerLinkFactory` abstract the direct byte path between peers
- `MeshRouter` coordinates peer discovery plus negotiated link setup
- `MeshStoreCore` runs routed retrieval over any local `Store`

Production transport crates compose this core with their own peerfinding and
byte links. The same router/store core powers simulation and can support FIPS,
local mocks, nearby transports, or any other `PeerLinkFactory`.

## Architecture

1. Discovery and negotiation flow through a `SignalingTransport`
2. Direct peer links are created by a `PeerLinkFactory`
3. `MeshStoreCore` routes hash-addressed requests over the connected peers

## Usage

Simulation builds on this crate directly. Native production networking should
compose the shared core from a transport crate such as `hashtree-fips-transport`.
