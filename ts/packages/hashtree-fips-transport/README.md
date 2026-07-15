# @hashtree/fips-transport

Hashtree blob exchange over reliable TCP/FIPS streams.

This package keeps FIPS below Hashtree: FIPS discovers peers, signals transports,
and moves authenticated datagrams between node identities. TCP/FIPS owns
ordered byte delivery, flow control, and segment retransmission. Hashtree still
owns hash verification, peer choice, one whole-session retry, and cache writes.
The byte framing is documented in
[`docs/tcp-fips-blob-v1.md`](../../../docs/tcp-fips-blob-v1.md).

Browser providers join the shared FIPS discovery fabric by default:

```text
fips-overlay-v1
```

The adapter exposes one transport: `TcpBlobTransport`. It uses `@fips/tcp`
service 39018 and verifies each blob hash before returning or caching data:

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

The discovery scope is configurable for isolated deployments, but applications
should normally stay on `fips-overlay-v1` so they share the generic FIPS transit
fabric instead of creating a parallel Hashtree-only WebRTC island.
