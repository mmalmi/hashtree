# @hashtree/fips-transport

Hashtree blob exchange over FIPS endpoint bytes.

This package keeps FIPS below Hashtree: FIPS discovers peers, signals transports,
and moves opaque bytes between node identities. Hashtree still owns hash
verification, content routing, HTL, peer choice, hedging, and cache writes.

Public Hashtree FIPS swarms use a Hashtree-specific FIPS discovery app scope:

```text
hashtree-v1
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

console.log(DEFAULT_FIPS_DISCOVERY_APP); // hashtree-v1
const hash = await sha256(new TextEncoder().encode('hello'));
const data = await store.get(hash);
```

If a peer does not have a blob, it does not need to send a miss. Silence is
treated as unknown/no response. The transport sends one request to each selected
peer for a read and lets the caller's existing source selection decide whether
to ask other peers or fall back to Blossom/local sources.

The Hashtree discovery scope is an application participation advert, not the
same thing as a generic FIPS reachability advert. A native daemon may advertise
its own `fips-overlay-v1` identity while a separate endpoint identity behind it
advertises `hashtree-v1`.
