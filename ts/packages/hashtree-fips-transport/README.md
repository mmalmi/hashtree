# @hashtree/fips-transport

Hashtree blob exchange over reliable TCP/FIPS streams.

[Getting started](https://github.com/mmalmi/hashtree/blob/master/ts/GETTING_STARTED.md) · [API reference](https://github.com/mmalmi/hashtree/blob/master/ts/API.md)

## Install

Install the FIPS peers from their immutable release archives alongside this
package. These two peers are not yet available in the npm registry:

```bash
npm install https://github.com/mmalmi/hashtree/releases/download/hashtree-ts-runtime-v0.5.17/hashtree-fips-transport-0.4.20.tgz \
  https://github.com/mmalmi/fips-ts/releases/download/runtime-v0.0.48/fips-core-0.0.48.tgz \
  https://github.com/mmalmi/fips-ts/releases/download/runtime-v0.0.48/fips-transport-webrtc-0.0.51.tgz
```

With npm 12, add `--allow-remote=all` to this command and subsequent `npm install`
or `npm ci` commands, because the FIPS packages use release URL dependencies.

## Usage

This package keeps FIPS below Hashtree: FIPS discovers peers, signals transports,
and moves authenticated datagrams between node identities. TCP/FIPS owns
ordered byte delivery, flow control, and segment retransmission. Hashtree still
owns hash verification, peer choice, one whole-session retry, and cache writes.
The byte framing is documented in the
[networking protocol](https://github.com/mmalmi/hashtree/blob/master/docs/NETWORKING.md#blob-protocol-v1).

Browser providers join the shared FIPS discovery fabric by default:

```text
fips-overlay-v1
```

The adapter exposes one transport: `TcpBlobTransport`. It uses `@fips/tcp`
service 39018 and verifies each blob hash before returning or caching data:

The following integration snippet assumes `fipsNode` is an already-running
`FipsDatagramEndpoint` and `peerId` is the remote FIPS identity. For browser apps,
the managed provider below creates the node and discovery connection for you.

```ts
import { MemoryStore, sha256 } from '@hashtree/core';
import {
  TcpBlobTransport,
  DEFAULT_FIPS_DISCOVERY_APP,
} from '@hashtree/fips-transport';

const localStore = new MemoryStore();
const transport = new TcpBlobTransport({
  endpoint: fipsNode,
  localStore,
});

console.log(DEFAULT_FIPS_DISCOVERY_APP); // fips-overlay-v1
const hash = await sha256(new TextEncoder().encode('hello'));
const data = await transport.get(hash, [peerId]);
await transport.close();
```

`HashtreeWorkerClient` can use a managed browser FIPS node directly. FIPS owns
Nostr peer discovery and WebRTC signaling; this package only carries Hashtree
blob streams over authenticated FIPS service datagrams:

```ts
import { createBrowserHashtreeFipsProvider } from '@hashtree/fips-transport/browser';

const provider = await createBrowserHashtreeFipsProvider({
  deviceSecretKey,
  relays,
  localStore,
});

workerClient.setP2PProvider(provider);

// Shut down the provider before discarding the worker client.
await provider.stop();
```

Here `deviceSecretKey` is a persistent 32-byte device secret (bytes or hex),
`relays` is your relay URL list, `localStore` is the block cache, and `workerClient`
is an initialized [Hashtree worker client](https://github.com/mmalmi/hashtree/blob/master/ts/packages/hashtree-worker/README.md).
Keep the device secret private. Provider creation establishes discovery, not a
guarantee that a peer with a particular blob is online; handle misses, failures,
and reconnection in the app. Serving peers receive raw stored blobs; do not
serve decrypted files through the blob interface.

The discovery scope is configurable for isolated deployments, but applications
should normally stay on `fips-overlay-v1` so they share the generic FIPS transit
fabric rather than creating an application-specific discovery fabric.

Explicit worker `providerRoutes` retain their forwarding policy when the same
peer passes automatic service discovery. Discovered-only peers and explicitly
local-only routes stay local-only, as do requests with an explicit HTL of zero.

## Controlled inbound serving

`serveBlob(hash, peerId, signal, htl)` optionally supplies an incoming response
from an application-authorized source. It runs after peer admission, receives
the authenticated peer identity and the existing request's cancellation signal,
and must honor HTL 0 as local-only. Returned bytes are hash-verified. Return null
only for a confirmed miss or denied read; throw for incomplete or failed reads.
The hook does not replace the local store used by outgoing requests.

`getUploadLimitBytesPerSecond` optionally supplies a dynamic global response
limit. Null or zero means unlimited. Header and body writes share a one-second
bucket; partial writes refund unused capacity. Responses larger than the bucket
progress in smaller chunks within the original request deadline. Closing the
transport aborts pending serving callbacks and response writers.
