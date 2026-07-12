# Hashtree On FIPS

Hashtree uses FIPS as the peerfinding, signaling, and node-to-node byte
transport layer. FIPS stays lower than Hashtree: it discovers same-app endpoint
identities, negotiates UDP or WebRTC links, and carries opaque endpoint bytes.
Hashtree keeps the content protocol.

## Scope

Public Hashtree providers join the shared FIPS Nostr discovery fabric:

```text
fips-overlay-v1
```

Private deployments can select another discovery app, but browser and native
Hashtree providers should normally share `fips-overlay-v1` rather than create a
parallel Hashtree-only WebRTC island. The Hashtree request/response codec ignores
unrelated FIPS endpoint payloads.

## Wire Shape

FIPS endpoint bytes carry the existing `@hashtree/mesh` frames:

```text
0x00 msgpack({ h, htl? })       DataRequest
0x01 msgpack({ h, d })          DataResponse
```

There is no Hashtree service-port registry in FIPS. The local adapter boundary is:

```ts
send(peerId, bytes)
onMessage(({ peerId, data }) => ...)
```

The responder verifies it has the requested hash locally, sends a response only
when it does, and stays silent on unknown hashes. The requester verifies the
response hash before resolving or caching it.

## Loss And Silence

Do not tunnel TCP over FIPS for Hashtree blobs. UDP/WebRTC loss is acceptable at
this layer because each frame is independently hash-addressed and Hashtree's
source selection can ask more peers or fall back to other stores.

Silence means unknown/no response, not absence. A timeout should not be recorded
as a content miss, and it should not trigger endless retries to the same peer.
Hedging to other peers is a Hashtree scheduling concern.

## FIPS WebRTC Underlay

FIPS supplies WebRTC as an underlay transport beside UDP/TCP/Tor. Hashtree does
not publish its own WebRTC discovery or signaling events. FIPS WebRTC uses:

- advert kind `37195`
- advert identifier `fips-overlay-v1`
- discovery `d` and `protocol` tags `fips-overlay-v1`
- signal kind `21059`
- NIP-59/NIP-44 encrypted WebRTC offer, answer, candidate, and reject messages
- unordered, unreliable data channels for FIPS packets

A native node with both UDP and WebRTC enabled can bridge browser-only FIPS
WebRTC peers and ordinary native FIPS UDP peers. If the Hashtree node is a
separate endpoint identity behind that daemon, the daemon should route FIPS
endpoint bytes to it without Hashtree learning about the underlay.

## Interop Tests

The useful acceptance matrix is:

- TS Hashtree over TS FIPS WebRTC fetches a blob by hash.
- TS Hashtree over Rust FIPS UDP fetches a blob by hash.
- Browser TS WebRTC peer exchanges FIPS packets with native Rust WebRTC peer.
- Native Rust node with UDP and WebRTC forwards Hashtree bytes between browser
  WebRTC and native UDP peers.
- Silent peer returns no miss; requester times out as unknown and can ask another
  source.
- Poisoned response is ignored because the returned bytes do not hash to the
  requested hash.
