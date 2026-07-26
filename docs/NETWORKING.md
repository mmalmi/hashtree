# Hashtree Networking and P2P Protocol

Hashtree resolves mutable Nostr roots, discovers authenticated FIPS peers, and
exchanges content-addressed blobs.

## Mutable roots

Repository roots are Nostr replaceable events:

- kind `30064` (`30078` is accepted for compatibility)
- `d` tag: repository name
- `l` tag: `hashtree`
- `hash` tag: current root hash

Clients select the newest valid signed event for the requested author and
repository. Subscriptions should remain open: a quiet relay interval is not proof
that a root does not exist.

## Peer discovery

Peer identity is a Nostr public key (`npub`).
[FIPS](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/fips)
discovery is how a node learns a transport endpoint for another `npub`. The
endpoint comes from a static peer entry (`npub`, UDP/TCP, and `host:port`) or an
endpoint advertisement over Nostr or the local network. FIPS authenticates the
connection against the `npub`; the endpoint is only a connection hint. Public
Hashtree nodes share this discovery scope:

```text
fips-overlay-v1
```

A blob provider is either explicitly configured or advertises the exact service:

```text
hashtree.blob/1
```

The default FSP port is `39018`. A connected FIPS peer is not automatically a
blob provider.

## Blob protocol v1

To read a tree, the client fetches its root blob, reads the child hashes, and
repeats for the required nodes and chunks. Each lookup fetches one immutable
blob by SHA-256, never by filename or path:

1. The client selects an explicitly configured provider or one advertising
   `hashtree.blob/1`.
2. It opens an authenticated
   [fips-tcp](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/fips-tcp)
   session to service `39018`.
3. It sends the blob hash and an HTL forwarding budget.
4. The provider checks its configured blob routes and replies with the raw
   stored bytes or `NoResult`.
5. The client verifies `SHA256(bytes) == requested hash` before returning or
   caching the blob.
6. The session closes. A reset may retry the whole session once.

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

Reference request:

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

`BlobRouter` may query local storage, peer groups, and HTTP stores concurrently.
The first hash-valid response wins. Writes go only to the configured primary
store.

| Outcome | Meaning |
| --- | --- |
| Data | Verified blob returned |
| No result | This route completed without the blob |
| Timeout/error | Result remains unknown |

A route-local miss must not become a global negative cache entry. Slow or failed
peers are not evidence that the blob is absent elsewhere.

## Serving rules

- Public endpoints serve raw blob or ciphertext bytes only.
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
