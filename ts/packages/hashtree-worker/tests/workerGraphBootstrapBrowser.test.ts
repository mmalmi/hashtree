import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer, type Server } from 'node:http';
import { once } from 'node:events';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { chromium, type Browser, type Page } from '@playwright/test';
import { WebSocketServer } from 'ws';
import { serveSignedGraph } from './namedGraphFixture.js';
import { createNamedGraphFixture } from 'nostr-social-graph/testing/namedGraph';
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
    worker: { format: 'iife', plugins: () => [{ name: 'readonly-graph-fixture-observer', enforce: 'pre', transform(source: string, id: string) {
      if (id !== fileURLToPath(new URL('../src/relay/worker.ts', import.meta.url))) return;
      // Test-only observation of the actual live instance. No production API or lifecycle changes.
      const call = /loadGraphBootstrap\s*\(\s*\{\s*root\s*:\s*DEFAULT_BOOTSTRAP_PUBKEY/;
      if (!call.test(source)) throw new Error('Missing worker graph bootstrap call');
      // Only the fixture signer and relay differ from the default signed reader.
      const observed = source.replace(call,
        "loadGraphBootstrap({ snapshot: JSON.parse(new URL(self.location.href).searchParams.get('snapshot')), minUsers: Number(new URL(self.location.href).searchParams.get('minUsers')) || undefined, root: DEFAULT_BOOTSTRAP_PUBKEY");
      return observed + `\nself.addEventListener('message', event => {
        if (event.data.type !== 'fixtureGraphState') return;
        const { id, pubkey } = event.data;
        self.postMessage({ type: 'fixtureGraphState', id, bootstrapPending: graphBootstrapAbort !== undefined,
          follow: socialGraph.getListRevision(pubkey, 3), mute: socialGraph.getListRevision(pubkey, 10000),
          followed: [...socialGraph.getFollowedByUser(pubkey)], muted: [...socialGraph.getMutedByUser(pubkey)] });
      });`;
    } }] }, build: { write: false, minify: false, rollupOptions: { input: path.join(fixture, 'main.js') } } });
  const outputs = (Array.isArray(result) ? result : [result]).flatMap(result => result.output);
  const asset = outputs.find(output => output.type === 'asset' && String(output.source).includes('NEG-OPEN'));
  if (!asset || asset.type !== 'asset') throw new Error('Production worker bundle missing');
  server = createServer((request, response) => {
    const worker = request.url?.split('?')[0] === '/worker.js';
    response.setHeader('Content-Type', worker ? 'application/javascript' : 'text/html');
    response.end(worker ? asset.source : '<!doctype html><title>Worker policy fixture</title>');
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

async function expectIngestedLists(page: Page, followEvent: ReturnType<typeof finalizeEvent>, muteEvent: ReturnType<typeof finalizeEvent>) {
  await expect.poll(async () => {
    const state = await request(page, { type: 'fixtureGraphState', pubkey: owner });
    return { followId: state.follow?.id, followTime: state.follow?.created_at,
      muteId: state.mute?.id, muteTime: state.mute?.created_at, followed: state.followed, muted: state.muted };
  }).toEqual({ followId: followEvent.id, followTime: followEvent.created_at,
    muteId: muteEvent.id, muteTime: muteEvent.created_at,
    followed: followEvent.tags.filter(tag => tag[0] === 'p').map(tag => tag[1]),
    muted: muteEvent.tags.filter(tag => tag[0] === 'p').map(tag => tag[1]) });
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
    let releaseDirectory!: () => void;
    const pendingDirectory = new Promise<void>(resolve => { releaseDirectory = resolve; });
    const delay = ['switch-to-custom', 'other-tab', 'pending-root-switch', 'pending-identity-switch', 'unchanged-root'].includes(mode);
    const snapshot = await serveSignedGraph(context, DEFAULT_BOOTSTRAP_PUBKEY, seedBytes, delay ? pendingDirectory : undefined);
    const handoff = mode === 'root-switch' || mode === 'pending-root-switch' || mode === 'pending-identity-switch';
    const negotiations: any[] = [], negotiationPaths: string[] = [];
    const active = new Map<any, Map<string, any[]>>(), negSockets = new Set<any>();
    const delayedInitialMute = mode === 'custom-default' || mode === 'switch-to-custom';
    let initialMuteReleased = false; const pendingInitialMutes: Array<() => void> = [];
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
          const send = () => { if (socket.readyState === 1) socket.send(JSON.stringify(['EVENT', frame[1], event])); };
          if (delayedInitialMute && event.kind === 10000 && !initialMuteReleased) pendingInitialMutes.push(send);
          else send();
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
      await page.evaluate(workerUrl => {
        const win = window as any; win.policyVersions = []; win.policyWorker = new Worker(workerUrl);
        win.policyWorker.addEventListener('message', (event: MessageEvent) => { if (event.data.type === 'socialGraphVersion') win.policyVersions.push(event.data.version); });
      }, snapshot.workerUrl);
      await request(page, { type: 'init', config: { pubkey: handoff ? muted : owner, storeName: `policy-${mode}`, relays: [`ws://127.0.0.1:${address.port}/initial`],
        ...(mode !== 'custom-default' && mode !== 'root-switch' ? { graphBootstrap: true } : {}), blossomServers: [] } });
      if (mode === 'switch-to-custom') {
        await expect.poll(() => snapshot.counts.directories).toBe(1);
        // Omitting the update policy must revoke the earlier explicit permission.
        await request(page, { type: 'setRelays', relays: [`ws://127.0.0.1:${address.port}/custom`] });
        await expect.poll(() => snapshot.counts.aborted).toBe(1);
        releaseDirectory();
      }
      if (mode === 'unchanged-root') {
        await expect.poll(() => snapshot.counts.directories).toBe(1);
        await request(page, { type: 'setSocialGraphRoot', pubkey: owner });
        expect(snapshot.counts.aborted).toBe(0); releaseDirectory();
      }
      if (handoff) {
        if (mode === 'root-switch') await expect.poll(() => negotiations.some(filter => filter.authors?.includes(muted))).toBe(true);
        else await expect.poll(() => snapshot.counts.directories).toBe(1);
        await request(page, { type: mode === 'pending-identity-switch' ? 'setIdentity' : 'setSocialGraphRoot', pubkey: owner });
        if (mode !== 'root-switch') {
          await expect.poll(() => snapshot.counts.aborted).toBe(1); releaseDirectory();
        }
        await expect.poll(() => negotiations.some(filter => filter.authors?.includes(owner)), { timeout: 5000 }).toBe(true);
      }
      if (mode === 'other-tab' || mode === 'invalid-cache') {
        let bytes: Uint8Array;
        if (mode === 'other-tab') {
          await expect.poll(() => snapshot.counts.directories).toBe(1);
          const otherTab = await context.newPage(); await otherTab.goto(origin);
          const local = new SocialGraph(owner); local.handleEvent([localFollow, localMute], true); local.markListIncomplete(owner, 3);
          bytes = await local.toBinary(); await retain(otherTab, bytes); await otherTab.close();
          releaseDirectory();
          await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: owner })).pubkeys).toEqual([friend]);
        } else bytes = new Uint8Array([0, 1, 2]);
        // Exercise the real five-second persistence debounce, then a later update
        // and close flush: a conflict/invalid row must block every later save.
        await request(page, { type: 'handleSocialGraphEvents', events: [follow(400), mute(390)] });
        await expectIngestedLists(page, follow(400), mute(390));
        await page.waitForTimeout(5500);
        expect(await retainedBytes(page)).toEqual([...bytes]);
        await request(page, { type: 'handleSocialGraphEvents', events: [follow(500), mute(490)] });
        await expectIngestedLists(page, follow(500), mute(490));
        await request(page, { type: 'close' });
        expect(await retainedBytes(page)).toEqual([...bytes]);
        expect(snapshot.counts.directories).toBe(mode === 'other-tab' ? 1 : 0);
        expect(errors).toEqual([]);
        return;
      }
      await expect.poll(() => negotiations.length, { timeout: 10000 }).toBeGreaterThan(0);
      if (mode === 'switch-to-custom') expect(negotiationPaths.every(path => path === '/custom')).toBe(true);
      expect(negotiations[0].kinds).toEqual([3, 10000]); expect(negotiations.some(filter => filter.authors.includes(owner))).toBe(true);
      await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: owner })).pubkeys).toEqual([friend]);
      if (delayedInitialMute) {
        // Reproduce the CI race deterministically: follow readiness cannot imply mute ingestion.
        const beforeMute = await request(page, { type: 'fixtureGraphState', pubkey: owner });
        expect(beforeMute.follow.id).toBe(localFollow.id); expect(beforeMute.followed).toEqual([friend]);
        expect(beforeMute.mute).toBeUndefined(); expect(beforeMute.muted).toEqual([]);
        initialMuteReleased = true; for (const send of pendingInitialMutes.splice(0)) send();
      }
      const seeded = mode === 'explicit-bootstrap' || mode === 'unchanged-root';
      await expectIngestedLists(page, seeded ? seedFollow : localFollow, seeded ? seedMute : localMute);
      let lateFollow: ReturnType<typeof finalizeEvent> | undefined, lateMute: ReturnType<typeof finalizeEvent> | undefined;
      if (handoff) {
        // Wait for the bounded NEG sockets to close, then deliver new
        // signed events only over the surviving ordinary live subscription.
        await expect.poll(() => [...negSockets].every(socket => socket.readyState === 3)).toBe(true);
        const listeners = () => [...active].flatMap(([socket, subscriptions]) => socket.readyState === 1
          ? [...subscriptions].filter(([, filters]) => filters.some(filter => filter.authors?.includes(owner) && !filter.ids && filter.kinds?.includes(3) && filter.kinds?.includes(10000))).map(([id]) => ({ socket, id })) : []);
        await expect.poll(() => listeners().length, { timeout: 5000 }).toBeGreaterThan(0);
        lateFollow = finalizeEvent({ kind: 3, created_at: 900, content: '', tags: [['p', muted]] }, key);
        lateMute = finalizeEvent({ kind: 10000, created_at: 890, content: '', tags: [['p', friend]] }, key);
        for (const { socket, id } of listeners()) socket.send(JSON.stringify(['EVENT', id, lateFollow]));
        await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: owner })).pubkeys).toEqual([muted]);
        // Deterministic delayed-second-event control: follow readiness still has the old mute.
        const beforeMute = await request(page, { type: 'fixtureGraphState', pubkey: owner });
        expect(beforeMute.follow.id).toBe(lateFollow.id); expect(beforeMute.mute.id).toBe(localMute.id);
        expect(beforeMute.muted).toEqual([muted]);
        for (const { socket, id } of listeners()) socket.send(JSON.stringify(['EVENT', id, lateMute]));
        await expectIngestedLists(page, lateFollow, lateMute);
        // Close immediately after exact ingestion: this still tests the pending debounce flush.
      }
      await request(page, { type: 'close' });
      const graph = await saved(page);
      expect(snapshot.counts.directories).toBe((mode === 'explicit-bootstrap' || mode === 'unchanged-root') || mode === 'switch-to-custom' || mode === 'pending-root-switch' || mode === 'pending-identity-switch' ? 1 : 0);
      expect(graph.getListRevision(owner, 3)).toMatchObject({ id: lateFollow?.id ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? seedFollow.id : localFollow.id),
        created_at: lateFollow?.created_at ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? 100 : 300), complete: mode !== 'retained-partial' });
      expect(graph.getListRevision(owner, 10000)).toMatchObject({ id: lateMute?.id ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? seedMute.id : localMute.id),
        created_at: lateMute?.created_at ?? ((mode === 'explicit-bootstrap' || mode === 'unchanged-root') ? 90 : 270), complete: true });
      expect([...graph.getMutedByUser(owner)]).toEqual([handoff ? friend : muted]); expect(errors).toEqual([]);
    } finally {
      releaseDirectory();
      await snapshot.close();
      await context.close(); for (const socket of relay.clients) socket.terminate();
      await new Promise<void>(resolve => relay.close(() => resolve()));
      expect(snapshot.counts.legacy).toBe(0);
      expect(snapshot.counts.names).toBe(snapshot.counts.directories);
    }
  }, 20000);
}

for (const phase of ['pending-snapshot', 'pending-idb', 'catchup', 'changed-signer', 'changed-account'] as const) {
  it(`keeps redundant identity idempotent during ${phase}`, async () => {
    const context = await browser.newContext({ serviceWorkers: 'block' });
    const page = await context.newPage();
    const relay = new WebSocketServer({ port: 0, host: '127.0.0.1' }); await once(relay, 'listening');
    const address = relay.address(); if (!address || typeof address === 'string') throw new Error('Relay missing');
    const seedFollow = follow(100), seedMute = mute(90), nextFollow = follow(300), nextMute = mute(270);
    const seed = new SocialGraph(DEFAULT_BOOTSTRAP_PUBKEY); seed.addFollower(DEFAULT_BOOTSTRAP_PUBKEY, owner);
    seed.handleEvent([seedFollow, seedMute], true); const seedBytes = await seed.toBinary();
    const require = createRequire(import.meta.url);
    const codec = await import(pathToFileURL(path.join(path.dirname(require.resolve('nostr-social-graph/package.json')), 'src/vendor/negentropy.js')).href);
    let releaseDirectory!: () => void, releaseNeg!: () => void;
    const pendingDirectory = new Promise<void>(resolve => { releaseDirectory = resolve; });
    const pendingNeg = new Promise<void>(resolve => { releaseNeg = resolve; });
    const negotiations: Array<{ socket: any; filter: any; closed: boolean; requests: any[]; ids: string[] }> = [];
    const errors: string[] = [];
    const snapshot = await serveSignedGraph(context, DEFAULT_BOOTSTRAP_PUBKEY, seedBytes, pendingDirectory);
    relay.on('connection', socket => {
      let record: typeof negotiations[number] | undefined, queue = Promise.resolve();
      const sessions = new Map<string, any>();
      socket.on('close', () => { if (record) record.closed = true; });
      socket.on('message', bytes => {
        queue = queue.then(async () => {
          const frame = JSON.parse(bytes.toString()), id = frame[1];
          if (frame[0] === 'NEG-OPEN') {
            record = { socket, filter: frame[2], closed: false, requests: [], ids: [] }; negotiations.push(record);
            const storage = new codec.NegentropyStorageVector();
            if (phase === 'catchup') for (const event of [nextFollow, nextMute]) {
              if (frame[2].authors.includes(event.pubkey) && frame[2].kinds.includes(event.kind)) storage.insert(event.created_at, event.id);
            }
            storage.seal(); const session = new codec.Negentropy(storage, 16384); sessions.set(id, session);
            const [reply] = await session.reconcile(frame[3]);
            if (phase === 'catchup') await pendingNeg;
            if (socket.readyState === 1) socket.send(JSON.stringify(['NEG-MSG', id, reply]));
          } else if (frame[0] === 'NEG-MSG') {
            const [reply] = await sessions.get(id).reconcile(frame[2]);
            socket.send(JSON.stringify(['NEG-MSG', id, reply]));
          } else if (frame[0] === 'REQ') {
            const filters = frame.slice(2);
            if (record) {
              record.requests.push(filters);
              expect(filters.every((filter: any) => Array.isArray(filter.ids) && Object.keys(filter).every(key => key === 'ids'))).toBe(true);
              for (const event of [nextFollow, nextMute]) if (filters.some((filter: any) => filter.ids.includes(event.id))) {
                record.ids.push(event.id); socket.send(JSON.stringify(['EVENT', id, event]));
              }
            }
            // Ordinary live subscriptions receive no fixture bodies, so this proof requires real NEG.
            socket.send(JSON.stringify(['EOSE', id]));
          }
        }).catch(error => { errors.push(String(error)); socket.close(); });
      });
    });
    try {
      await page.goto(origin);
      if (phase === 'pending-idb') await page.evaluate(async () => {
        const db = await new Promise<IDBDatabase>((resolve, reject) => {
          const req = indexedDB.open('hashtree-social-graph', 10);
          req.onupgradeneeded = () => req.result.createObjectStore('socialGraph', { keyPath: 'id' });
          req.onsuccess = () => resolve(req.result); req.onerror = () => reject(req.error);
        });
        const tx = db.transaction('socialGraph', 'readwrite'); let held = true;
        const keep = () => { const req = tx.objectStore('socialGraph').get('main'); req.onsuccess = () => { if (held) keep(); }; };
        keep(); (window as any).releaseIdentityRead = () => { held = false; };
        tx.oncomplete = () => db.close();
      });
      await page.evaluate(workerUrl => { (window as any).policyWorker = new Worker(workerUrl); }, snapshot.workerUrl);
      const nsec = Buffer.from(key).toString('hex');
      await request(page, { type: 'init', config: { pubkey: owner, nsec, storeName: `identity-${phase}`,
        relays: [`ws://127.0.0.1:${address.port}`], graphBootstrap: phase !== 'catchup', blossomServers: [] } });
      if (phase === 'pending-idb') {
        const repeated = request(page, { type: 'setIdentity', pubkey: owner, nsec });
        await page.waitForTimeout(150); expect(snapshot.counts.directories).toBe(0);
        await page.evaluate(() => (window as any).releaseIdentityRead()); await repeated;
      } else if (phase === 'catchup') {
        await expect.poll(() => negotiations.length).toBe(1);
        await request(page, { type: 'setIdentity', pubkey: owner, nsec });
        await page.waitForTimeout(150);
        expect(negotiations).toHaveLength(1); expect(negotiations[0].closed).toBe(false);
        releaseNeg();
      } else {
        await expect.poll(() => snapshot.counts.directories).toBe(1);
        await request(page, { type: 'setIdentity', pubkey: phase === 'changed-account' ? muted : owner,
          ...(phase === 'changed-signer' ? {} : { nsec: phase === 'changed-account' ? Buffer.from(new Uint8Array(32).fill(44)).toString('hex') : nsec }) });
      }
      const changed = phase === 'changed-signer' || phase === 'changed-account';
      if (changed) {
        await expect.poll(() => snapshot.counts.aborted).toBe(1); releaseDirectory();
        await expect.poll(() => negotiations.length).toBeGreaterThan(0);
        const activeRoot = phase === 'changed-account' ? muted : owner;
        expect(negotiations.some(record => record.filter.authors.includes(activeRoot))).toBe(true);
        await request(page, { type: 'close' });
        const graph = await saved(page); expect(graph.getListRevision(owner, 3)).toBeUndefined();
        expect(graph.getListRevision(owner, 10000)).toBeUndefined();
      } else {
        if (phase !== 'catchup') { await expect.poll(() => snapshot.counts.directories).toBe(1); expect(snapshot.counts.aborted).toBe(0); releaseDirectory(); }
        await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: owner })).pubkeys).toEqual([friend]);
        await expect.poll(() => negotiations.length).toBeGreaterThan(0);
        await expect.poll(() => negotiations.every(record => record.closed)).toBe(true);
        await expectIngestedLists(page, phase === 'catchup' ? nextFollow : seedFollow, phase === 'catchup' ? nextMute : seedMute);
        await request(page, { type: 'close' }); const graph = await saved(page);
        expect(graph.getListRevision(owner, 3)).toMatchObject({ id: phase === 'catchup' ? nextFollow.id : seedFollow.id, complete: true });
        expect(graph.getListRevision(owner, 10000)).toMatchObject({ id: phase === 'catchup' ? nextMute.id : seedMute.id, complete: true });
        expect([...graph.getMutedByUser(owner)]).toEqual([muted]);
        if (phase === 'catchup') expect(negotiations.flatMap(record => record.ids).sort()).toEqual([nextFollow.id, nextMute.id].sort());
        expect(snapshot.counts.directories).toBe(phase === 'catchup' ? 0 : 1); expect(snapshot.counts.aborted).toBe(0);
      }
      expect(errors).toEqual([]);
    } finally {
      releaseDirectory(); releaseNeg(); await snapshot.close(); await context.close();
      for (const socket of relay.clients) socket.terminate(); await new Promise<void>(resolve => relay.close(() => resolve()));
      expect(snapshot.counts.legacy).toBe(0);
      expect(snapshot.counts.names).toBe(snapshot.counts.directories);
    }
  }, 15000);
}


for (const mode of ['delayed-upgrade', 'invalid-upgrade', 'small-upgrade', 'retained', 'custom-relays', 'cancel-upgrade', 'untouched-restart'] as const) {
  it(`keeps packaged discovery usable with ${mode}`, async () => {
    const context = await browser.newContext({ serviceWorkers: 'block' });
    const page = await context.newPage(), errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    const require = createRequire(import.meta.url);
    const bundledBytes = await readFile(require.resolve('nostr-social-graph/data/socialGraph.bin'));
    const receipt = JSON.parse(await readFile(require.resolve('nostr-social-graph/data/socialGraph.json'), 'utf8'));
    const bundled = await SocialGraph.fromBinary(receipt.root, bundledBytes);
    const expectedSeedFollows = [...bundled.getFollowedByUser(receipt.root)];
    expect(receipt.users).toBeGreaterThanOrEqual(10_000);
    let bundledRequests = 0;
    await context.route(`${origin}/bundled-graph.bin`, route => {
      bundledRequests++;
      return route.fulfill({ body: bundledBytes, contentType: 'application/octet-stream' });
    });
    const relay = new WebSocketServer({ port: 0, host: '127.0.0.1' }); await once(relay, 'listening');
    const address = relay.address(); if (!address || typeof address === 'string') throw new Error('Relay missing');
    let requests = 0;
    relay.on('connection', socket => socket.on('message', bytes => {
      const [type, id] = JSON.parse(bytes.toString());
      if (type === 'REQ') { requests++; socket.send(JSON.stringify(['EOSE', id])); }
    }));
    const freshList = finalizeEvent({ kind: 3, created_at: 500, content: '', tags: [['p', muted]] }, new Uint8Array(32).fill(43));
    const seed = new SocialGraph(DEFAULT_BOOTSTRAP_PUBKEY);
    seed.addFollower(DEFAULT_BOOTSTRAP_PUBKEY, friend); seed.handleEvent(freshList, true);
    if (mode === 'small-upgrade') {
      for (let i = 0; i < 997; i++) seed.addFollower(DEFAULT_BOOTSTRAP_PUBKEY, (i + 100_000).toString(16).padStart(64, '0'));
      expect(seed.size().users).toBe(1000);
    }
    let release!: () => void;
    const gate = new Promise<void>(resolve => { release = resolve; });
    const snapshot = await serveSignedGraph(context, DEFAULT_BOOTSTRAP_PUBKEY, await seed.toBinary(), gate);
    if (mode === 'small-upgrade') snapshot.workerUrl += '&minUsers=10000';
    if (mode === 'invalid-upgrade') await context.route(/^https:\/\/cdn\.iris\.to\/[0-9a-f]{64}\.bin$/, async route => {
      await gate; await route.fulfill({ body: Buffer.from([0, 1, 2]), headers: { 'Access-Control-Allow-Origin': '*' } });
    });
    try {
      await page.goto(origin);
      const local = new SocialGraph(owner); local.handleEvent([follow(700), mute(690)], true);
      const retained = await local.toBinary();
      if (mode === 'retained') await retain(page, retained);
      await page.evaluate(workerUrl => { (window as any).policyWorker = new Worker(workerUrl); }, snapshot.workerUrl);
      await request(page, { type: 'init', config: { pubkey: owner, storeName: `bundled-${mode}`,
        relays: [`ws://127.0.0.1:${address.port}`], blossomServers: [], graphReconciliation: false,
        graphBootstrap: mode !== 'custom-relays', graphBootstrapFallback: { url: `${origin}/bundled-graph.bin`, receipt } } });
      if (mode === 'retained' || mode === 'custom-relays') {
        await expect.poll(() => requests).toBeGreaterThan(0);
        await request(page, { type: 'close' });
        expect(bundledRequests).toBe(0); expect(snapshot.counts.names).toBe(0);
        if (mode === 'retained') expect(await retainedBytes(page)).toEqual([...retained]);
        return;
      }
      // The signed directory is still withheld, but packaged discovery and the
      // selected relay are already usable. There is no remote-timeout readiness gate.
      await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: receipt.root })).pubkeys).toEqual(expectedSeedFollows);
      expect(bundledRequests).toBe(1); await expect.poll(() => requests).toBeGreaterThan(0);
      expect((await request(page, { type: 'getFollows', pubkey: friend })).pubkeys).toEqual([]);
      if (mode === 'untouched-restart') {
        // Close flushes pending durable work. A packaged seed by itself is not
        // user state, and must not disable signed discovery on the next visit.
        await request(page, { type: 'close' });
        const hasSavedGraph = await page.evaluate(async () => {
          const db = await new Promise<IDBDatabase>((resolve, reject) => {
            const request = indexedDB.open('hashtree-social-graph');
            request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
          });
          try { return await new Promise<boolean>((resolve, reject) => {
            const request = db.transaction('socialGraph').objectStore('socialGraph').get('main');
            request.onsuccess = () => resolve(request.result !== undefined); request.onerror = () => reject(request.error);
          }); } finally { db.close(); }
        });
        expect(hasSavedGraph, 'Untouched fallback must not become retained user state').toBe(false);
        await page.evaluate(workerUrl => {
          (window as any).policyWorker.terminate();
          (window as any).policyWorker = new Worker(workerUrl);
        }, snapshot.workerUrl);
        await request(page, { type: 'init', config: { pubkey: owner, storeName: `bundled-${mode}`,
          relays: [`ws://127.0.0.1:${address.port}`], blossomServers: [], graphReconciliation: false,
          graphBootstrap: true, graphBootstrapFallback: { url: `${origin}/bundled-graph.bin`, receipt } } });
        await expect.poll(() => bundledRequests).toBe(2);
        await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: receipt.root })).pubkeys).toEqual(expectedSeedFollows);
        release();
        await expect.poll(async () => (await request(page, { type: 'fixtureGraphState', pubkey: friend })).follow?.id).toBe(freshList.id);
        await request(page, { type: 'close' });
        expect((await saved(page)).getListRevision(friend, 3)?.id).toBe(freshList.id);
        expect(errors).toEqual([]);
        return;
      }
      await request(page, { type: 'handleSocialGraphEvents', events: [follow(700), mute(690)] });
      await expectIngestedLists(page, follow(700), mute(690));
      if (mode === 'cancel-upgrade') await request(page, { type: 'setRelays', relays: [] });
      release();
      if (mode === 'delayed-upgrade') {
        await expect.poll(async () => (await request(page, { type: 'fixtureGraphState', pubkey: friend })).follow?.id).toBe(freshList.id);
        await expectIngestedLists(page, follow(700), mute(690));
      } else {
        // Observe completion/revocation, then assert both active and durable state.
        await expect.poll(async () => (await request(page, { type: 'fixtureGraphState', pubkey: friend })).bootstrapPending).toBe(false);
        expect((await request(page, { type: 'fixtureGraphState', pubkey: friend })).follow).toBeUndefined();
      }
      expect((await request(page, { type: 'getFollows', pubkey: receipt.root })).pubkeys).toEqual(expectedSeedFollows);
      await request(page, { type: 'close' });
      const graph = await saved(page);
      expect(graph.getListRevision(owner, 3)?.id).toBe(follow(700).id);
      expect(graph.getListRevision(owner, 10000)?.id).toBe(mute(690).id);
      expect(graph.getListRevision(friend, 3)?.id).toBe(mode === 'delayed-upgrade' ? freshList.id : undefined);
      expect(errors).toEqual([]);
    } finally {
      release(); await snapshot.close(); await context.close();
      for (const socket of relay.clients) socket.terminate();
      await new Promise<void>(resolve => relay.close(() => resolve()));
    }
  }, 20000);
}


it.runIf(Boolean(process.env.GRAPH_BOOTSTRAP_BENCHMARK))('records a bounded first-discovery comparison with identical mobile bytes', async () => {
  const require = createRequire(import.meta.url);
  const bytes = await readFile(require.resolve('nostr-social-graph/data/socialGraph.bin'));
  const receipt = JSON.parse(await readFile(require.resolve('nostr-social-graph/data/socialGraph.json'), 'utf8'));
  const graph = await SocialGraph.fromBinary(receipt.root, bytes);
  const followed = [...graph.getFollowedByUser(receipt.root)];
  const relay = new WebSocketServer({ port: 0, host: '127.0.0.1' }); await once(relay, 'listening');
  const address = relay.address(); if (!address || typeof address === 'string') throw new Error('Relay missing');
  relay.on('connection', socket => socket.on('message', bytes => {
    const [type, id] = JSON.parse(bytes.toString());
    if (type === 'REQ') socket.send(JSON.stringify(['EOSE', id]));
  }));
  const publication = await createNamedGraphFixture(DEFAULT_BOOTSTRAP_PUBKEY, bytes);
  const results: Array<Record<string, unknown>> = [];
  try {
    for (let repeat = 0; repeat < 3; repeat++) for (const packaged of [false, true]) {
      const context = await browser.newContext({ serviceWorkers: 'block' });
      const page = await context.newPage();
      let release!: () => void, timer: ReturnType<typeof setTimeout> | undefined, bundledReads = 0, released = false;
      const gate = new Promise<void>(resolve => { release = () => { released = true; resolve(); }; });
      const snapshot = await serveSignedGraph(context, DEFAULT_BOOTSTRAP_PUBKEY, bytes, gate, publication);
      await context.route(`${origin}/benchmark-graph.bin`, route => {
        bundledReads++; return route.fulfill({ body: bytes, contentType: 'application/octet-stream' });
      });
      try {
        await page.goto(origin);
        await page.evaluate(workerUrl => { (window as any).policyWorker = new Worker(workerUrl); }, snapshot.workerUrl);
        const started = performance.now();
        // A controlled source delay, not a readiness sleep. All result waits use graph state.
        timer = setTimeout(release, 500);
        await request(page, { type: 'init', config: { pubkey: owner, storeName: 'bootstrap-benchmark',
          relays: [`ws://127.0.0.1:${address.port}`], blossomServers: [], graphBootstrap: true, graphReconciliation: false,
          ...(packaged ? { graphBootstrapFallback: { url: `${origin}/benchmark-graph.bin`, receipt } } : {}) } });
        await expect.poll(async () => (await request(page, { type: 'getFollows', pubkey: receipt.root })).pubkeys).toEqual(followed);
        const firstUsefulMs = performance.now() - started, beforeRemoteRelease = !released;
        await expect.poll(async () => (await request(page, { type: 'fixtureGraphState', pubkey: owner })).bootstrapPending).toBe(false);
        await request(page, { type: 'close' });
        const retained = await saved(page);
        expect([...retained.getFollowedByUser(receipt.root)]).toEqual(followed);
        expect(bundledReads).toBe(packaged ? 1 : 0);
        results.push({ repeat, mode: packaged ? 'packaged-first' : 'remote-only', firstUsefulMs: Math.round(firstUsefulMs),
          beforeRemoteRelease, bundledBytes: bundledReads * bytes.length, signedBytes: snapshot.counts.bytes });

      } finally {
        clearTimeout(timer); release(); await snapshot.close(); await context.close();
      }
    }
  } finally {
    for (const socket of relay.clients) socket.terminate();
    await new Promise<void>(resolve => relay.close(() => resolve()));
  }
  await writeFile(process.env.GRAPH_BOOTSTRAP_BENCHMARK!, JSON.stringify({ version: 1, injectedDirectoryDelayMs: 500, mobileBytes: bytes.length, results }, null, 2) + '\n', { flag: 'wx' });
}, 60000);
