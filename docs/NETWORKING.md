# Hashtree Networking and P2P Protocol

Hashtree resolves mutable Nostr roots, discovers authenticated FIPS peers, and
exchanges content-addressed blobs.

## Mutable roots

Mutable tree and repository roots are Nostr replaceable events:

- kind `30064` (`30078` is accepted for compatibility)
- `d` tag: tree or repository name
- `l` tag: `hashtree` (legacy unlabeled events may be accepted)
- `hash` tag: current root hash

Clients accept valid signed events matching the requested author and `d`, then
select the newest `created_at`; the lowest event ID wins a tie. Subscriptions
should remain open: a quiet relay interval is not proof that a root does not
exist.

## Peer discovery

Peer identity is a Nostr public key (`npub`).
[FIPS](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/fips)
discovery finds a route to another `npub`. For a direct peer, a contact endpoint
can come from a static peer entry (`npub`, UDP/TCP, and `host:port`) or an
endpoint advertisement over Nostr or the local network. FIPS authenticates the
connection against the `npub`; the endpoint is only a connection hint. Hashtree
labels its Nostr and LAN discovery advertisements with:

```text
fips-overlay-v1
```

Eligible blob providers are exact: explicitly configured `npub`s or
authenticated same-host FIPS instances advertising `hashtree.blob/1` on service
`39018`. A generic connected FIPS peer is not eligible.

## Blob protocol v1

To read a tree, the client fetches its root blob, verifies it, decrypts it when
needed, and decodes it locally. It then follows child hashes for the required
nodes and chunks. Each lookup fetches one immutable blob by SHA-256, never by
filename or path:

1. The client submits the hash to `BlobRouter`.
2. The router selects and searches its configured sources, such as local
   storage, a P2P provider group, and Blossom/HTTP stores. It may hedge several
   routes.
3. The P2P route selects and hedges a bounded set of eligible providers. Each
   peer attempt opens an authenticated
   [fips-tcp](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/fips-tcp)
   session to service `39018`.
4. The attempt sends the blob hash and an HTL forwarding budget.
5. The provider checks its configured blob routes and replies with the raw
   stored bytes or `NoResult`.
6. The router accepts the first reply satisfying
   `SHA256(bytes) == requested hash`, may cache it, and cancels slower attempts.
7. Each peer session closes after its reply or error. Any retry is client policy
   and remains bounded by the request deadline.

Bad framing, a wrong hash, timeout, or reset is an error, not a missing blob.
`fips-tcp` provides ordered delivery, flow control, and retransmission.

### Wire format

Each session carries one request and one reply.

Request, 36 bytes:

| Offset | Size | Value |
| ---: | ---: | --- |
| 0 | 1 | magic `H` (`0x48`) |
| 1 | 1 | version `1` |
| 2 | 1 | method `GET` (`1`) |
| 3 | 1 | HTL, `0..10` |
| 4 | 32 | blob SHA-256 |

Reply header, 7 bytes:

| Offset | Size | Value |
| ---: | ---: | --- |
| 0 | 1 | magic `H` |
| 1 | 1 | version `1` |
| 2 | 1 | status: `0` no result, `1` data |
| 3 | 4 | unsigned big-endian payload length |

`NoResult` has length zero. `Data` is followed by exactly `length` raw blob bytes.
The maximum payload is 16 MiB.

Reference request for HTL `0` and hash bytes `00..1f`:

```text
48010100000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
```

Reference data reply header for three bytes:

```text
48010100000003
```

## Routing

HTL is `0..10`; clients normally start at `10`.

- Direct same-host requests use HTL `0`.
- At HTL `0`, the receiver may serve locally but must not forward.
- Each Hashtree forwarding hop decrements HTL by one.
- FIPS transport hops do not change HTL.

| Outcome | Meaning |
| --- | --- |
| Data | Verified blob returned |
| No result | This route completed without the blob |
| Timeout/error | Result remains unknown |

`NoResult` from one route does not stop or negatively cache the lookup. The
lookup reports missing only when all routes complete with `NoResult`. If no
route returns data and any route times out or fails, the lookup returns an error.

## Serving rules

- Public endpoints return stored blob bytes only (public data or ciphertext).
- Never transmit decryption keys.
- Never assemble or expose plaintext trees without an explicit allowlist.
- Authentication identifies a peer; authorization still applies separately.
- Bound payload size, concurrent requests, and per-peer work.

## Implementations

- [Wire codec](../rust/crates/hashtree-core/src/blob_route.rs)
- [FIPS transport](../rust/crates/hashtree-fips-transport/src/tcp_blob.rs)
- [Native mesh router](../rust/crates/hashtree-network/src/blob_router.rs)
- [TypeScript FIPS transport](../ts/packages/hashtree-fips-transport/src/tcpBlobTransport.ts)
- [TypeScript mesh router](../ts/packages/hashtree-mesh/src/blobRouter.ts)
