# @hashtree/nostr

WebRTC P2P storage and Nostr ref resolver for hashtree.

## Install

```bash
npm install @hashtree/nostr
```

## WebRTC Store

P2P data fetching via WebRTC with Nostr signaling:

```typescript
import { WebRTCStore } from '@hashtree/nostr';

const store = new WebRTCStore({
  signer,    // NIP-07 compatible
  pubkey,
  encrypt,   // NIP-44
  decrypt,
  localStore,
  relays: ['wss://relay.example.com'],
});

await store.start();
const data = await store.get(hash);
```

## Nostr Ref Resolver

Resolve `npub/treename` to merkle root hashes:

```typescript
import { createNostrRefResolver } from '@hashtree/nostr';

const resolver = createNostrRefResolver({
  subscribe: (filters, onEvent) => { /* NDK subscribe */ },
  publish: (event) => { /* NDK publish */ },
});

const root = await resolver.resolve('npub1.../myfiles');
```

## License

MIT
