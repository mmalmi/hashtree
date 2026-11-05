import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { chromium, expect } from '@playwright/test';
import { startLocalFipsWebSocketSeed } from './support/localFipsWebSocketSeed.mjs';

// Bundle through the package's existing locked Vitest/Vite compiler dependency.
const requireVitest = createRequire(import.meta.resolve('vitest/node'));
const requireVite = createRequire(requireVitest.resolve('vite'));
const { build } = requireVite('esbuild');
const bundle = await build({
  stdin: { contents: `
    import { createBrowserHashtreeFipsProvider, createBrowserHashtreeNostrProvider } from './src/browserProvider.ts';
    import { MemoryStore, sha256, toHex } from '@hashtree/core';
    import { identityFromSecretKey } from '@fips/core';
    window.fixture = { createBrowserHashtreeFipsProvider, createBrowserHashtreeNostrProvider,
      MemoryStore, sha256, toHex, identityFromSecretKey };
  `, resolveDir: fileURLToPath(new URL('..', import.meta.url)), sourcefile: 'browser-fixture.ts' },
  bundle: true, platform: 'browser', format: 'esm', write: false,
});
const server = createServer((request, response) => {
  response.setHeader('content-type', request.url === '/bundle.js' ? 'text/javascript' : 'text/html');
  response.end(request.url === '/bundle.js' ? bundle.outputFiles[0].contents : '<script type="module" src="/bundle.js"></script>');
});
server.listen(0, '127.0.0.1');
await once(server, 'listening');
const origin = `http://127.0.0.1:${server.address().port}`;
const socketAttempts = [], requests = [], failures = [];
let browser, context, seed, page;
const deadline = setTimeout(() => { void context?.close(); }, 60000);
try {
  browser = await chromium.launch({ headless: true });
  context = await browser.newContext();
  await context.route('**/*', async route => {
    const url = route.request().url();
    if (new URL(url).origin === origin) return route.continue();
    requests.push(new URL(url).origin); await route.abort();
  });
  await context.routeWebSocket('**', route => {
    socketAttempts.push(route.url());
    if (seed && route.url() === seed.url) route.connectToServer();
    else route.close({ code: 1000, reason: 'Unconfigured socket' });
  });
  page = await context.newPage();
  page.on('pageerror', error => failures.push(error.message));
  await page.addInitScript(() => {
    window.rtcConfigs = [];
    const Original = window.RTCPeerConnection;
    window.RTCPeerConnection = class extends Original {
      constructor(config, ...rest) { super(config, ...rest); window.rtcConfigs.push(config); }
    };
  });
  await page.goto(origin);
  await page.waitForFunction(() => window.fixture);
  const invalid = await page.evaluate(async () => {
    const f = window.fixture, before = await indexedDB.databases();
    let rejected = 0;
    for (const config of [
      { relays: [], websocketSeedUrls: ['https://invalid.example'] },
      { relays: [], websocketSeedUrls: ['  '] },
      { relays: ['not a url'], websocketSeedUrls: [] },
      { relays: ['https://relay.example'], websocketSeedUrls: [] },
    ]) {
      try {
        await f.createBrowserHashtreeNostrProvider({
          identity: await f.identityFromSecretKey(new Uint8Array(32).fill(30)), localStore: new f.MemoryStore(),
          ...config, stunServers: [],
        });
      } catch { rejected++; }
    }
    return { rejected, databasesUnchanged: JSON.stringify(before) === JSON.stringify(await indexedDB.databases()) };
  });
  assert.deepEqual(invalid, { rejected: 4, databasesUnchanged: true });
  // Both real transports/node and durable public-announcement routing initialize
  // without public carriers; an empty list never inherits default bootstrap.
  const empty = await page.evaluate(async () => {
    const f = window.fixture;
    const identity = await f.identityFromSecretKey(new Uint8Array(32).fill(31));
    const options = { identity, relays: [], websocketSeedUrls: [], stunServers: [], localStore: new f.MemoryStore() };
    const p = await f.createBrowserHashtreeNostrProvider(options);
    const result = { websocketAbsent: p.webSocketTransport === undefined,
      webrtcPresent: p.webRtcTransport.type === 'webrtc', sourcePresent: !!p.nostrSource };
    await p.stop();
    const reopened = await f.createBrowserHashtreeNostrProvider(options);
    await reopened.stop();
    return result;
  });
  assert.deepEqual(empty, { websocketAbsent: true, webrtcPresent: true, sourcePresent: true });
  assert.deepEqual(socketAttempts, []); assert.deepEqual(requests, []);
  // A deliberately chosen local seed supplies initial FIPS adjacency. Signed
  // announcements use the supplied discovery bus; actual WebRTC and blob data
  // still pass through the unmodified production transports and FIPS node.
  seed = await startLocalFipsWebSocketSeed();
  await page.evaluate(async seedUrl => {
    const f = window.fixture, events = new Map(), readers = new Set();
    const discoveryProvider = {
      publish: async event => { events.set(event.id, event); for (const cb of readers) cb(event, 'fixture-discovery'); },
      subscribe: async (_filter, cb) => { readers.add(cb); for (const event of events.values()) cb(event, 'fixture-discovery'); return () => readers.delete(cb); },
    };
    window.providers = [];
    window.peerLinks = [];
    for (const byte of [32, 33]) {
      const localStore = new f.MemoryStore();
      if (byte === 32) window.sourceStore = localStore;
      const p = await f.createBrowserHashtreeFipsProvider({
        identity: await f.identityFromSecretKey(new Uint8Array(32).fill(byte)), localStore,
        relays: [], discoveryProvider, websocketSeedUrls: [seedUrl], stunServers: [],
        iceGatherTimeoutMs: 3000, connectTimeoutMs: 15000,
      });
      p.node.on('peer', event => window.peerLinks.push(event));
      window.providers.push(p);
    }
    window.discoveryReaders = () => readers.size;
  }, seed.url);
  await expect.poll(() => page.evaluate(() => window.peerLinks.some(event => event.remoteAddr?.transport === 'webrtc'
    && event.state === 'connected')), { timeout: 25000 }).toBe(true);
  const delivery = await page.evaluate(async () => {
    const f = window.fixture, [source, reader] = window.providers;
    const bytes = new TextEncoder().encode('chosen discovery and local bootstrap carry real peer bytes');
    const hash = await f.sha256(bytes);
    // Store access belongs to the fixture, through the same Store passed at construction.
    await window.sourceStore.put(hash, bytes);
    const fetched = await reader.fetch(f.toHex(hash), source.localPeerId);
    const rtcConfigs = window.rtcConfigs;
    await Promise.all(window.providers.map(provider => provider.stop()));
    return { matched: fetched?.length === bytes.length && fetched.every((byte, i) => byte === bytes[i]),
      rtcConfigs, discoveryReadersAfterStop: window.discoveryReaders() };
  });
  assert.equal(delivery.matched, true);
  assert.equal(delivery.discoveryReadersAfterStop, 0);
  assert.ok(delivery.rtcConfigs.length > 0);
  assert.ok(delivery.rtcConfigs.every(config => !config.iceServers?.length));
  assert.ok(socketAttempts.length >= 2 && socketAttempts.every(url => url === seed.url));
  assert.deepEqual(requests, []); assert.deepEqual(failures, []);
  console.log(JSON.stringify({ passed: true, emptyNetworkNoEgress: true, emptyNetworkReopen: true,
    explicitDiscovery: true, realWebRtc: true, realBlobDelivery: true, onlyOwnedSeedSockets: socketAttempts.length }));
} catch (error) {
  console.error(JSON.stringify({ socketAttempts, requests, failures,
    state: await page?.evaluate(() => ({ rtcConfigs: window.rtcConfigs,
      links: window.peerLinks?.map(event => ({ transport: event.remoteAddr?.transport, state: event.state })) })).catch(() => undefined) }));
  throw error;
} finally {
  clearTimeout(deadline);
  await context?.close(); await browser?.close(); await seed?.close();
  server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
}
