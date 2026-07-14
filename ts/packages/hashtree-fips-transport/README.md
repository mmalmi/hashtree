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

The adapter surface is intentionally tiny:

```ts
export interface FipsEndpoint {
  send(peerId: string, data: Uint8Array): Promise<void>;
  onMessage(handler: (message: { peerId: string; data: Uint8Array }) => void | Promise<void>): () => void;
  listPeerIds?(): readonly string[] | Promise<readonly string[]>;
  localPeerId?(): string;
}
```

The low-level endpoint-data bridge remains available for record-oriented
embeddings. Managed browser and worker providers use `@fips/tcp` service 39018.

Use it as a local-first store wrapper:

```ts
import { MemoryStore, sha256 } from '@hashtree/core';
import {
  FipsTransportStore,
  createFipsNodeEndpoint,
  DEFAULT_FIPS_DISCOVERY_APP,
} from '@hashtree/fips-transport';

const endpoint = createFipsNodeEndpoint(fipsNode);

const localStore = new MemoryStore();
const store = new FipsTransportStore({
  endpoint,
  localStore,
  peers: () => endpoint.listPeerIds?.() ?? [],
});

console.log(DEFAULT_FIPS_DISCOVERY_APP); // fips-overlay-v1
const hash = await sha256(new TextEncoder().encode('hello'));
const data = await store.get(hash);
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
