import 'fake-indexeddb/auto';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { once } from 'node:events';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { runInNewContext } from 'node:vm';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { WebSocket, WebSocketServer } from 'ws';
import { getPublicKey } from 'nostr-tools/pure';
import { expect, it } from 'vitest';

it('starts real graph reconciliation from a default IIFE consumer worker', async () => {
  // Reuse the Vite version already locked by our test runner; no added build dependency.
  const require = createRequire(import.meta.url);
  const testRunner = createRequire(require.resolve('vitest/package.json'));
  const { build } = await import(pathToFileURL(testRunner.resolve('vite')).href);
  const fixture = await mkdtemp(path.join(tmpdir(), 'hashtree-worker-iife-'));
  const relay = new WebSocketServer({ port: 0 });
  await once(relay, 'listening');
  const address = relay.address();
  if (!address || typeof address === 'string') throw new Error('Local relay missing');
  const owner = getPublicKey(new Uint8Array(32).fill(42));
  let scope: any;
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    const entry = fileURLToPath(new URL('../src/relay-entry.ts', import.meta.url));
    await writeFile(path.join(fixture, 'main.js'), `import Worker from ${JSON.stringify(`${entry}?worker`)}; new Worker();`);
    const result = await build({ configFile: false, root: fixture, publicDir: false, logLevel: 'silent',
      worker: { format: 'iife' },
      build: { write: false, minify: false, rollupOptions: { input: path.join(fixture, 'main.js') } },
    });
    const outputs = (Array.isArray(result) ? result : [result]).flatMap(result => result.output);
    const asset = outputs.find(output => output.type === 'asset' && String(output.source).includes('NEG-OPEN'));
    expect(asset).toBeDefined();
    const negotiation = new Promise<Record<string, unknown>>((resolve, reject) => {
      timeout = setTimeout(() => reject(new Error('Bundled worker never opened graph reconciliation')), 5000);
      relay.on('connection', socket => socket.on('message', bytes => {
        const message = JSON.parse(bytes.toString());
        if (message[0] === 'NEG-OPEN') {
          clearTimeout(timeout); resolve(message[2]);
          socket.send(JSON.stringify(['NEG-ERR', message[1], 'unsupported']));
        }
        if (message[0] === 'REQ') socket.send(JSON.stringify(['EOSE', message[1]]));
      }));
    });
    void negotiation.catch(() => {});
    scope = { indexedDB, IDBKeyRange, WebSocket, crypto, URL, URLSearchParams,
      TextEncoder, TextDecoder, Uint8Array, ArrayBuffer, DataView, Error, RangeError, TypeError, DOMException, AbortController, AbortSignal,
      Blob, Request, Response, performance, setTimeout, clearTimeout, setInterval, clearInterval,
      queueMicrotask, structuredClone, atob, btoa, console, Event, CustomEvent, EventTarget,
      fetch: async () => new Response('', { status: 503 }),
      navigator: { onLine: true, userAgent: 'synthetic-worker' }, postMessage() {},
    };
    scope.self = scope;
    runInNewContext(String(asset.source), scope, { timeout: 2000 });
    await scope.onmessage({ data: { id: 'iife-init', type: 'init', config: {
      storeName: `iife-graph-${crypto.randomUUID()}`, pubkey: owner,
      relays: [`ws://127.0.0.1:${address.port}`], graphReconciliation: true, blossomServers: [],
    } } });
    expect(await negotiation).toEqual({ kinds: [3, 10000], authors: [owner] });
  } finally {
    clearTimeout(timeout);
    await scope?.onmessage?.({ data: { id: 'iife-close', type: 'close' } });
    for (const socket of relay.clients) socket.terminate();
    await new Promise<void>(resolve => relay.close(() => resolve()));
    await rm(fixture, { recursive: true, force: true });
  }
}, 30000);
