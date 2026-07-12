# @hashtree/worker

Modular browser worker for hashtree blob caching and Blossom connectivity.

Runs hashtree storage operations in a Web Worker to keep the main thread free. Handles IndexedDB caching, Blossom server uploads/downloads, connectivity probing, and P2P data exchange.

## Install

```bash
npm install @hashtree/worker
```

## Usage

```typescript
import { HashtreeWorkerClient } from '@hashtree/worker';
import HashtreeWorker from './workers/hashtree.worker.ts?worker';

const client = new HashtreeWorkerClient(HashtreeWorker, {
  blossomServers: [{ url: 'https://upload.example', read: true, write: true }],
});
await client.init();

// Store and retrieve blobs
const { hashHex } = await client.putBlob(data);
const { data: blob } = await client.getBlob(hashHex);
```

## Plain Worker + FIPS WebRTC

Browser mesh traffic runs through FIPS. FIPS owns Nostr discovery, signaling,
and WebRTC links; the worker only exchanges Hashtree request/response frames
through the provider interface:

```typescript
import { HashtreeWorkerClient } from '@hashtree/worker';
import { DexieStore } from '@hashtree/dexie';
import { createBrowserHashtreeFipsProvider } from '@hashtree/fips-transport/browser';

const client = new HashtreeWorkerClient(HashtreeWorker, {
  storeName: 'demo-worker',
});
const provider = await createBrowserHashtreeFipsProvider({
  deviceSecretKey,
  relays: ['wss://relay.damus.io'],
  localStore: new DexieStore('demo-fips-provider'),
});
client.setP2PProvider(provider);
await client.init();

// Before discarding the worker client:
await provider.stop();
await client.close();
```

There is no direct Hashtree WebRTC signaling or data-channel stack in this
package. `@fips/transport-webrtc` is the browser WebRTC underlay.

## Relay Worker Client

If you are using `@hashtree/worker/relay-entry?worker` and need relay-backed tree-root
metadata or subscription calls, use the dedicated relay wrapper:

```typescript
import { RelayWorkerClient } from '@hashtree/worker/relay-client';
import HashtreeWorker from '@hashtree/worker/relay-entry?worker';

const client = new RelayWorkerClient(HashtreeWorker, {
  storeName: 'demo-sites-worker',
  relays: ['wss://relay.damus.io'],
  blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
  pubkey: '336f319763657d6b0e65a5b5876719e8c8dcdcf9396852be71ee26b73368b29b',
});

// Created with createBrowserHashtreeFipsProvider(...) as above.
client.setP2PProvider(fipsProvider);
await client.init();
const root = await client.getTreeRootInfo('npub1example', 'sites/example');
const stop = client.onTreeRootUpdate((update) => {
  console.log(update.treeName, update.visibility);
});
```

## Browser Runtime Defaults

When the app runs inside Iris or another shell that injects `window.__HTREE_SERVER_URL__`
(or the launch URL carries `htree_server`), the main app-facing API should be
`createHtreeRuntime(...)`:

```typescript
import {
  HashtreeWorkerClient,
  createHtreeRuntime,
} from '@hashtree/worker';
import HashtreeWorker from './workers/hashtree.worker.ts?worker';

const DEFAULT_RELAYS = [
  'wss://relay.damus.io',
  'wss://relay.primal.net',
];

const DEFAULT_BLOSSOM_SERVERS = [
  { url: 'https://upload.example', read: false, write: true },
  { url: 'https://cdn.example', read: true, write: false },
];

const runtime = createHtreeRuntime({
  appId: 'my-app',
  relays: DEFAULT_RELAYS,
  blossomServers: DEFAULT_BLOSSOM_SERVERS,
});

const workerClient = new HashtreeWorkerClient(HashtreeWorker, {
  ...runtime.getWorkerConfig({
    storeName: 'my-app-worker',
  }),
});

const mediaUrl = runtime.urls.media('htree://nhash1example/video.mp4', {
  clientScoped: true,
  mimeType: 'video/mp4',
});

await runtime.media.ensureReady({
  registerMediaPort: (port) => workerClient.registerMediaPort(port),
});
```

## Embedding In A Custom Worker

If you already have your own worker and do not want `@hashtree/worker` to own the whole
worker entrypoint, attach the protocol handler yourself:

```typescript
import { attachHashtreeWorker } from '@hashtree/worker/worker';

attachHashtreeWorker(self);

self.addEventListener('message', (event) => {
  if (event.data?.type === 'my-custom-message') {
    // Your own worker logic.
  }
});
```

If you want stricter isolation, attach it to a dedicated `MessagePort` instead of `self`.
That lets a larger worker multiplex hashtree traffic without sharing one global message
channel.

Behavior:

- In plain web mode, `runtime.endpoints` keeps your configured public relays and Blossom servers.
- In native child runtimes, `runtime.endpoints` and `runtime.getWorkerConfig()` switch transport defaults to the local daemon endpoints.
- `runtime.urls.media(...)` handles `/htree/...` URL generation plus the per-client `htree_c` and optional `htree_t` query params.
- `runtime.media.ensureReady(...)` handles the common page-side service-worker/media-port handshake.

## Transport Notes

- If you expect media or files to keep working through the worker, daemon, or FIPS peers, app-facing URLs should stay in `htree://...` or `/htree/...` form. Raw `https://` Blossom URLs bypass the runtime routing and will fail when a client intentionally has no direct Blossom access.
- Mesh reads should be treated as liveness-based, not fixed-timeout-based. Slow peers are normal on cold paths; callers should hedge to more peers instead of converting a slow in-flight read into a synthetic not-found.
- After bytes or fragments start flowing, prefer idle/progress-based expiry over total wall-clock deadlines. That keeps large media transfers alive without giving malicious peers an unlimited pin.
- For realistic verification, test cold direct navigation with requester-side Blossom disabled and assert that artwork/audio loads from `/htree/...` without any fallback requests to default Blossom servers.

## Service Worker Client Keys

If your service worker intercepts `/htree/...` media requests and forwards them to a worker over `MessagePort`, use a stable per-tab client key:

- `createHtreeRuntime(...)` generates it once per tab/webview.
- `runtime.urls.media(..., { clientScoped: true })` appends it as the `htree_c` query param.
- `runtime.media.ensureReady(...)` sends the same key in `REGISTER_WORKER_PORT` and `PING_WORKER_PORT`.

That lets the service worker map fetches back to the correct worker port when multiple tabs or isolated child webviews are active at once, without falling back to a single global port.

## Exports

- `@hashtree/worker` — `createHtreeRuntime`, `resolveRuntimeEndpoints`, and `HashtreeWorkerClient`
- `@hashtree/worker/relay-client` — `RelayWorkerClient` plus relay-backed tree-root metadata types
- `@hashtree/worker/worker` — `attachHashtreeWorker(...)` for embedding the worker protocol into a custom worker
- `@hashtree/worker/entry` — Worker entry point
- `@hashtree/worker/protocol` — Shared message types between main thread and worker

## License

MIT
