# Hashtree On FIPS

Hashtree uses FIPS as the peerfinding, signaling, and node-to-node byte
transport layer. FIPS stays lower than Hashtree: it discovers same-app endpoint
identities, negotiates UDP or WebRTC links, and carries opaque endpoint bytes.
Hashtree keeps the content protocol.

## Scope

Public Hashtree swarms use a FIPS Nostr discovery app:

```text
hashtree-v1
```

Private swarms can narrow this with app-specific suffixes, for example
`hashtree-v1:team-alpha`. Do not add another `fips-overlay-v1:` prefix to the
app scope.

This is separate from a generic FIPS reachability advert. A host can publish:

- `fips-overlay-v1` for the daemon or router identity that is reachable over
  UDP/WebRTC/Tor/etc.
- `hashtree-v1` for the Hashtree endpoint identity that wants or serves
  Hashtree blobs.

Those identities can be the same key for a small single-process node, or
different keys when a host daemon routes for an app endpoint on the same machine.
Hashtree should discover the Hashtree endpoint identity; FIPS can then route to
it through whatever adjacent transport peers and local gateway paths it knows.

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

## Native WebRTC Path

Native FIPS should grow WebRTC as another FIPS transport beside UDP/TCP/Tor.
The WebRTC transport should use the same FIPS Nostr discovery/signaling format
as `fips-ts`:

- advert kind `37195`
- advert identifier `fips-overlay-v1`
- discovery `d` and `protocol` tags `hashtree-v1`
- signal kind `21059`
- NIP-59/NIP-44 encrypted WebRTC offer, answer, candidate, and reject messages
- unordered, unreliable data channels for FIPS packets

A native node with both UDP and WebRTC enabled can then bridge browser-only FIPS
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
