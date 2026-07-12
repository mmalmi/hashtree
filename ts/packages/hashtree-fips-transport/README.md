# @hashtree/fips-transport

Hashtree blob exchange over FIPS endpoint bytes.

This package keeps FIPS below Hashtree: FIPS discovers peers, signals transports,
and moves opaque bytes between node identities. Hashtree still owns hash
verification, content routing, HTL, peer choice, hedging, and cache writes.

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

For a real `@fips/core` node, use the built-in endpoint-data bridge. It maps
Hashtree blob frames onto FIPS app-owned endpoint bytes:

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

If a peer does not have a blob, it does not need to send a miss. Silence is
treated as unknown/no response. The transport sends one request to each selected
peer for a read and lets the caller's existing source selection decide whether
to ask other peers or fall back to Blossom/local sources.

`HashtreeWorkerClient` can use a managed browser FIPS node directly. FIPS owns
Nostr peer discovery and WebRTC signaling; this package only carries Hashtree
blob frames over authenticated FIPS endpoint data:

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
fabric instead of creating a parallel Hashtree-only WebRTC island. Hashtree's
request/response codec remains application-specific and ignores unrelated FIPS
endpoint payloads.
