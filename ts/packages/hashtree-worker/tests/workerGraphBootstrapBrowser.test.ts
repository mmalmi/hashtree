import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer, type Server } from 'node:http';
import { once } from 'node:events';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { chromium, type Browser, type Page } from '@playwright/test';
import { WebSocketServer } from 'ws';
import { SocialGraph } from 'nostr-social-graph';
import { finalizeEvent, getPublicKey } from 'nostr-tools/pure';
import { afterAll, beforeAll, expect, it } from 'vitest';
import { DEFAULT_BOOTSTRAP_PUBKEY } from '../src/relay/utils/constants.js';

const key = new Uint8Array(32).fill(42), owner = getPublicKey(key);
const friend = getPublicKey(new Uint8Array(32).fill(43));
const muted = getPublicKey(new Uint8Array(32).fill(44));
const follow = (created_at: number) => finalizeEvent({ kind: 3, created_at, content: '', tags: [['p', friend]] }, key);
const mute = (created_at: number) => finalizeEvent({ kind: 10000, created_at, content: '', tags: [['p', muted]] }, key);
let browser: Browser, server: Server, origin: string, fixture: string;

beforeAll(async () => {
  const require = createRequire(import.meta.url);
  const runner = createRequire(require.resolve('vitest/package.json'));
  const { build } = await import(pathToFileURL(runner.resolve('vite')).href);
  fixture = await mkdtemp(path.join(tmpdir(), 'worker-bootstrap-browser-'));
  const entry = fileURLToPath(new URL('../src/relay-entry.ts', import.meta.url));
  await writeFile(path.join(fixture, 'main.js'), `import Worker from ${JSON.stringify(`${entry}?worker`)}; new Worker();`);
  const result = await build({ configFile: false, root: fixture, publicDir: false, logLevel: 'silent',
    worker: { format: 'iife' }, build: { write: false, minify: false, rollupOptions: { input: path.join(fixture, 'main.js') } } });
  const outputs = (Array.isArray(result) ? result : [result]).flatMap(result => result.output);
  const asset = outputs.find(output => output.type === 'asset' && String(output.source).includes('NEG-OPEN'));
  if (!asset || asset.type !== 'asset') throw new Error('Production worker bundle missing');
  server = createServer((request, response) => {
    response.setHeader('Content-Type', request.url === '/worker.js' ? 'application/javascript' : 'text/html');
    response.end(request.url === '/worker.js' ? asset.source : '<!doctype html><title>Worker policy fixture</title>');
  }).listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('Fixture server missing');
  origin = `http://127.0.0.1:${address.port}`;
  browser = await chromium.launch({ headless: true });
}, 30000);

afterAll(async () => {
  await browser?.close();
  if (server) await new Promise<void>(resolve => server.close(() => resolve()));
  if (fixture) await rm(fixture, { recursive: true, force: true });
});

async function request(page: Page, payload: Record<string, unknown>) {
  return page.evaluate(payload => new Promise<any>((resolve, reject) => {
    const worker = (window as any).policyWorker as Worker, id = crypto.randomUUID();
    const timeout = setTimeout(() => reject(new Error(`No response to ${payload.type}`)), 5000);
    const listener = (event: MessageEvent) => {
      if (event.data.id !== id && !(payload.type === 'init' && event.data.type === 'ready')) return;
      clearTimeout(timeout); worker.removeEventListener('message', listener);
      if (event.data.type === 'error') reject(new Error(event.data.error)); else resolve(event.data);
    };
    worker.addEventListener('message', listener); worker.postMessage({ ...payload, id });
  }), payload);
}

async function retainedBytes(page: Page) {
  return page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = indexedDB.open('hashtree-social-graph');
      request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
    });
    try {
      return await new Promise<number[]>((resolve, reject) => {
        const request = db.transaction('socialGraph').objectStore('socialGraph').get('main');
        request.onsuccess = () => resolve([...request.result.data]); request.onerror = () => reject(request.error);
      });
    } finally { db.close(); }
  });
}
async function saved(page: Page) { return SocialGraph.fromBinary(owner, new Uint8Array(await retainedBytes(page))); }
async function retain(page: Page, bytes: Uint8Array) {
  await page.evaluate(async bytes => {
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = indexedDB.open('hashtree-social-graph', 10);
      request.onupgradeneeded = () => request.result.createObjectStore('socialGraph', { keyPath: 'id' });
      request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
    });
    try { await new Promise<void>((resolve, reject) => {
      const tx = db.transaction('socialGraph', 'readwrite');
      tx.objectStore('socialGraph').put({ id: 'main', data: new Uint8Array(bytes), updatedAt: Date.now() });
      tx.oncomplete = () => resolve(); tx.onerror = () => reject(tx.error);
    }); } finally { db.close(); }
  }, [...bytes]);
}

for (const mode of ['custom-default', 'explicit-bootstrap', 'retained-partial', 'switch-to-custom', 'other-tab', 'invalid-cache', 'root-switch', 'pending-root-switch', 'unchanged-root', 'pending-identity-switch'] as const) {
  it(`uses the actual browser worker with ${mode} policy`, async () => {
    const context = await browser.newContext({ serviceWorkers: 'block' });
    const page = await context.newPage(), errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    const relay = new WebSocketServer({ port: 0 }); await once(relay, 'listening');
    const address = relay.address(); if (!address || typeof address === 'string') throw new Error('Relay missing');
    const seedFollow = follow(100), seedMute = mute(90), localFollow = follow(300), localMute = mute(270);
    const seed = new SocialGraph(DEFAULT_BOOTSTRAP_PUBKEY);
    seed.addFollower(DEFAULT_BOOTSTRAP_PUBKEY, owner); seed.handleEvent([seedFollow, seedMute], true);
    const seedBytes = await seed.toBinary();
    let httpRequests = 0, abortedHttp = 0;
    let releaseHttp: (() => void) | undefined;
    const pendingHttp = new Promise<void>(resolve => { releaseHttp = resolve; });
    context.on('requestfailed', request => { if (request.url().startsWith('https://graph-api.iris.to/')) abortedHttp++; });
    await context.route('https://graph-api.iris.to/**', async route => {
      httpRequests++;
      if (mode === 'switch-to-custom' || mode === 'other-tab' || mode === 'pending-root-switch' || mode === 'pending-identity-switch' || mode === 'unchanged-root') await pendingHttp;
      await route.fulfill({ body: Buffer.from(seedBytes), contentType: 'application/octet-stream', headers: { 'Access-Control-Allow-Origin': '*', 'Access-Control-Expose-Headers': 'x-graph-root', 'x-graph-root': DEFAULT_BOOTSTRAP_PUBKEY } }).catch(() => {});
    });
    const handoff = mode === 'root-switch' || mode === 'pending-root-switch' || mode === 'pending-identity-switch';
    const negotiations: any[] = [], negotiationPaths: string[] = [];
    const active = new Map<any, Map<string, any[]>>(), negSockets = new Set<any>();
    relay.on('connection', (socket, connection) => socket.on('message', bytes => {
      const frame = JSON.parse(bytes.toString());
      if (frame[0] === 'NEG-OPEN') {
        negSockets.add(socket);
        negotiations.push(frame[2]); negotiationPaths.push(connection.url ?? '/'); socket.send(JSON.stringify(['NEG-ERR', frame[1], 'unsupported']));
      }
      if (frame[0] === 'CLOSE') { active.get(socket)?.delete(frame[1]); return; }
      if (frame[0] !== 'REQ') return;
      if (!active.has(socket)) active.set(socket, new Map());
      active.get(socket)!.set(frame[1], frame.slice(2));
      if (mode === 'custom-default' || handoff || (mode === 'switch-to-custom' && connection.url === '/custom')) for (const event of [localFollow, localMute]) {
        if (frame.slice(2).some((filter: any) => (!filter.kinds || filter.kinds.includes(event.kind)) &&
          (!filter.authors || filter.authors.includes(event.pubkey)) && (!filter.ids || filter.ids.includes(event.id)))) {
          socket.send(JSON.stringify(['EVENT', frame[1], event]));
        }
      }
      socket.send(JSON.stringify(['EOSE', frame[1]]));
    }));
    try {
      await page.goto(origin);
      if (mode === 'retained-partial') {
        const local = new SocialGraph(owner); local.handleEvent([localFollow, localMute], true); local.markListIncomplete(owner, 3);
        await retain(page, await local.toBinary());
      }
      if (mode === 'invalid-cache') await retain(page, new Uint8Array([0, 1, 2]));
      await page.evaluate(() => {
        const win = window as any; win.policyVersions = []; win.policyWorker = new Worker('/worker.js');
        win.policyWorker.addEventListener('message', (event: MessageEvent) => { if (event.data.type === 'socialGraphVersion') win.policyVersions.push(event.data.version); });
      });
      await request(page, { type: 'init', config: { pubkey: handoff ? muted : owner, storeName: `policy-${mode}`, relays: [`ws://127.0.0.1:${address.port}/initial`],
        ...(mode !== 'custom-default' && mode !== 'root-switch' ? { graphBootstrap: true } : {}), blossomServers: [] } });
      if (mode === 'switch-to-custom') {
        await expect.poll(() => httpRequests).toBe(1);
        // Omitting the update policy must revoke the earlier explicit permission.
        await request(page, { type: 'setRelays', relays: [`ws://127.0.0.1:${address.port}/custom`] });
        await expect.poll(() => abortedHttp).toBe(1);
        releaseHttp!();
      }
      if (mode === 'unchanged-root') {
        await expect.poll(() => httpRequests).toBe(1);
        await request(page, { type: 'setSocialGraphRoot', pubkey: owner });
        expect(abortedHttp).toBe(0); releaseHttp!();
      }
      if (handoff) {
        if (mode === 'root-switch') await expect.poll(() => negotiations.some(filter => filter.authors?.includes(muted))).toBe(true);
        else await expect.poll(() => httpRequests).toBe(1);
        await request(page, { type: mode === 'pending-identity-switch' ? 'setIdentity' : 'setSocialGraphRoot', pubkey: owner });
        if (mode !== 'root-switch') {
          await expect.poll(() => abortedHttp).toBe(1); releaseHttp!();
        }
        await expect.poll(() => negotiations.some(filter => filter.authors?.includes(owner)), { timeout: 5000 }).toBe(true);
      }
      if (mode === 'other-tab' || mode === 'invalid-cache') {
        let bytes: Uint8Array;
        if (mode === 'other-tab') {
          await expect.poll(() => httpRequests).toBe(1);
          const otherTab = await context.newPage(); await otherTab.goto(origin);
          const local = new SocialGraph(owner); local.handleEvent([localFollow, localMute], true); local.markListIncomplete(owner, 3);
          bytes = await local.toBinary(); await retain(otherTab, bytes); await otherTab.close();
          releaseHttp!();
          await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: owner })).pubkeys).toEqual([friend]);
        } else bytes = new Uint8Array([0, 1, 2]);
        // Exercise the real five-second persistence debounce, then a later update
        // and close flush: a conflict/invalid row must block every later save.
        await request(page, { type: 'handleSocialGraphEvents', events: [follow(400), mute(390)] });
        await page.waitForTimeout(5500);
        expect(await retainedBytes(page)).toEqual([...bytes]);
        await request(page, { type: 'handleSocialGraphEvents', events: [follow(500), mute(490)] });
        await request(page, { type: 'close' });
        expect(await retainedBytes(page)).toEqual([...bytes]);
        expect(httpRequests).toBe(mode === 'other-tab' ? 1 : 0);
        expect(errors).toEqual([]);
        return;
      }
      await expect.poll(() => negotiations.length, { timeout: 10000 }).toBeGreaterThan(0);
      if (mode === 'switch-to-custom') expect(negotiationPaths.every(path => path === '/custom')).toBe(true);
      expect(negotiations[0].kinds).toEqual([3, 10000]); expect(negotiations.some(filter => filter.authors.includes(owner))).toBe(true);
      await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: owner })).pubkeys).toEqual([friend]);
      let lateFollow: ReturnType<typeof finalizeEvent> | undefined, lateMute: ReturnType<typeof finalizeEvent> | undefined;
      if (handoff) {
        // Wait for the bounded NEG/fallback sockets to close, then deliver new
        // signed events only over the surviving ordinary live subscription.
        await expect.poll(() => [...negSockets].every(socket => socket.readyState === 3)).toBe(true);
        const listeners = () => [...active].flatMap(([socket, subscriptions]) => socket.readyState === 1
          ? [...subscriptions].filter(([, filters]) => filters.some(filter => filter.authors?.includes(owner) && !filter.ids && filter.kinds?.includes(3) && filter.kinds?.includes(10000))).map(([id]) => ({ socket, id })) : []);
        await expect.poll(() => listeners().length, { timeout: 5000 }).toBeGreaterThan(0);
        lateFollow = finalizeEvent({ kind: 3, created_at: 900, content: '', tags: [['p', muted]] }, key);
        lateMute = finalizeEvent({ kind: 10000, created_at: 890, content: '', tags: [['p', friend]] }, key);
        const versions = await page.evaluate(() => (window as any).policyVersions.length);
        for (const { socket, id } of listeners()) for (const event of [lateFollow, lateMute]) socket.send(JSON.stringify(['EVENT', id, event]));
        await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: owner })).pubkeys).toEqual([muted]);
        await expect.poll(() => page.evaluate(() => (window as any).policyVersions.length)).toBeGreaterThanOrEqual(versions + 2);
      }
      await request(page, { type: 'close' });
      const graph = await saved(page);
      expect(httpRequests).toBe((mode === 'explicit-bootstrap' || mode === 'unchanged-root') || mode === 'switch-to-custom' || mode === 'pending-root-switch' || mode === 'pending-identity-switch' ? 1 : 0);
      expect(graph.getListRevision(owner, 3)).toMatchObject({ id: lateFollow?.id ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? seedFollow.id : localFollow.id),
        created_at: lateFollow?.created_at ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? 100 : 300), complete: mode !== 'retained-partial' });
      expect(graph.getListRevision(owner, 10000)).toMatchObject({ id: lateMute?.id ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? seedMute.id : localMute.id),
        created_at: lateMute?.created_at ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? 90 : 270), complete: true });
      expect([...graph.getMutedByUser(owner)]).toEqual([handoff ? friend : muted]); expect(errors).toEqual([]);
    } finally {
      releaseHttp?.();
      await context.close(); for (const socket of relay.clients) socket.terminate();
      await new Promise<void>(resolve => relay.close(() => resolve()));
    }
  }, 20000);
}
