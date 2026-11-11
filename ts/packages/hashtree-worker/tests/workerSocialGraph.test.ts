import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { once } from 'node:events';
import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { WebSocket, WebSocketServer } from 'ws';
import { SocialGraph } from 'nostr-social-graph';
import { finalizeEvent, getPublicKey } from 'nostr-tools/pure';
import { afterAll, beforeAll, expect, it, vi } from 'vitest';
import type { WorkerRequest, WorkerResponse } from '../src/relay/protocol.js';

const key = new Uint8Array(32).fill(42), owner = getPublicKey(key);
const friend = getPublicKey(new Uint8Array(32).fill(43));
const muted = getPublicKey(new Uint8Array(32).fill(44));
const responses: WorkerResponse[] = [];
const scope = {
  onmessage: null as null | ((event: MessageEvent<WorkerRequest>) => Promise<void>),
  postMessage: (message: WorkerResponse) => { responses.push(message); },
};
let serial = 0;
async function request(payload: Record<string, unknown>) {
  const id = `graph-${++serial}`;
  await scope.onmessage!({ data: { ...payload, id } } as MessageEvent<WorkerRequest>);
  return responses.find(message => 'id' in message && message.id === id);
}
async function init(storeName: string, relays: string[] = [], graphReconciliation = false) {
  const result = await request({ type: 'init', config: { storeName, pubkey: owner, relays, graphReconciliation, blossomServers: [] } });
  expect(result?.type).not.toBe('error');
}
async function saved() {
  const db = new Dexie('hashtree-social-graph');
  db.version(1).stores({ socialGraph: '&id' });
  try {
    const record = await db.table('socialGraph').get('main');
    return await SocialGraph.fromBinary(owner, record.data);
  } finally { db.close(); }
}
beforeAll(async () => {
  vi.stubGlobal('self', scope);
  vi.stubGlobal('WebSocket', WebSocket);
  await import('../src/relay/worker.js');
});
afterAll(() => { vi.unstubAllGlobals(); });

it('keeps the personal graph on init/root changes and flushes follow/mute revision IDs before offline restart', async () => {
  const store = `graph-restart-${crypto.randomUUID()}`;
  const follow = finalizeEvent({ kind: 3, created_at: 100, content: '', tags: [['p', friend]] }, key);
  const mute = finalizeEvent({ kind: 10000, created_at: 90, content: '', tags: [['p', muted]] }, key);
  try {
    await init(store);
    await request({ type: 'handleSocialGraphEvents', events: [follow, mute] });
    await request({ type: 'initSocialGraph', rootPubkey: friend });
    expect(await request({ type: 'getFollows', pubkey: owner })).toMatchObject({ pubkeys: [friend] });
    await request({ type: 'close' });
    const graph = await saved();
    expect(graph.getListRevision(owner, 3)?.id).toBe(follow.id);
    expect(graph.getListRevision(owner, 10000)?.id).toBe(mute.id);
    expect([...graph.getMutedByUser(owner)]).toEqual([muted]);
    await init(store);
    expect(await request({ type: 'getFollows', pubkey: owner })).toMatchObject({ pubkeys: [friend] });
  } finally {
    await request({ type: 'close' });
    await Dexie.delete(store); await Dexie.delete(`${store}-nostr-index`);
  }
});

it('reconciles optional public lists and accepts mutes independently of newer follow timestamps', async () => {
  const relay = new WebSocketServer({ port: 0 });
  await once(relay, 'listening');
  const address = relay.address();
  if (!address || typeof address === 'string') throw new Error('Missing local relay port');
  const heads = [
    finalizeEvent({ kind: 3, created_at: 200, content: '', tags: [['p', muted]] }, key),
    finalizeEvent({ kind: 10000, created_at: 150, content: '', tags: [['p', friend]] }, key),
  ];
  const require = createRequire(import.meta.url);
  const codec = await import(pathToFileURL(path.join(path.dirname(require.resolve('nostr-social-graph/package.json')), 'src/vendor/negentropy.js')).href);
  let negotiations = 0, releaseMute!: () => void, observed: SocialGraph | undefined;
  const pendingMute = new Promise<void>(resolve => { releaseMute = resolve; });
  const exactIds: string[] = [], errors: string[] = [];
  const handleEvent = SocialGraph.prototype.handleEvent;
  const ingest = vi.spyOn(SocialGraph.prototype, 'handleEvent').mockImplementation(function(...args) {
    const result = handleEvent.apply(this, args);
    if (this.getListRevision(owner, 3)?.id === heads[0]!.id) observed = this;
    return result;
  });
  relay.on('connection', socket => {
    const sessions = new Map<string, any>(); let queue = Promise.resolve(), negotiated = false;
    socket.on('message', bytes => {
      queue = queue.then(async () => {
        const frame = JSON.parse(bytes.toString()), id = frame[1];
        if (frame[0] === 'NEG-OPEN') {
          negotiations++; negotiated = true;
          const storage = new codec.NegentropyStorageVector();
          for (const event of heads) if (frame[2].authors.includes(event.pubkey) && frame[2].kinds.includes(event.kind)) storage.insert(event.created_at, event.id);
          storage.seal(); const session = new codec.Negentropy(storage, 16384); sessions.set(id, session);
          socket.send(JSON.stringify(['NEG-MSG', id, (await session.reconcile(frame[3]))[0]]));
        } else if (frame[0] === 'NEG-MSG') {
          socket.send(JSON.stringify(['NEG-MSG', id, (await sessions.get(id).reconcile(frame[2]))[0]]));
        } else if (frame[0] === 'REQ') {
          if (negotiated) {
            expect(frame.slice(2).every((filter: any) => Array.isArray(filter.ids) && Object.keys(filter).every(key => key === 'ids'))).toBe(true);
            for (const event of heads) if (frame.slice(2).some((filter: any) => filter.ids.includes(event.id))) {
              if (event.kind === 10000) await pendingMute;
              exactIds.push(event.id); socket.send(JSON.stringify(['EVENT', id, event]));
            }
          }
          // The ordinary live subscription cannot make this reconciliation test pass.
          socket.send(JSON.stringify(['EOSE', id]));
        }
      }).catch(error => { errors.push(String(error)); socket.close(); });
    });
  });
  const store = `graph-relay-${crypto.randomUUID()}`;
  try {
    await init(store, [`ws://127.0.0.1:${address.port}`], true);
    await expect.poll(() => negotiations, { timeout: 5000 }).toBeGreaterThan(0);
    await expect.poll(async () => (await request({ type: 'getFollows', pubkey: owner }) as { pubkeys?: string[] })?.pubkeys).toEqual([muted]);
    // A received follow does not establish that the independently delivered mute arrived.
    expect(observed?.getListRevision(owner, 10000)?.id).not.toBe(heads[1]!.id);
    releaseMute();
    await expect.poll(() => observed?.getListRevision(owner, 10000)?.id).toBe(heads[1]!.id);
    expect([...observed!.getMutedByUser(owner)]).toEqual([friend]);
    await request({ type: 'close' });
    const graph = await saved();
    expect(exactIds.sort()).toEqual(heads.map(event => event.id).sort()); expect(errors).toEqual([]);
    expect(graph.getListRevision(owner, 3)?.id).toBe(heads[0]!.id);
    expect(graph.getListRevision(owner, 10000)?.id).toBe(heads[1]!.id);
  } finally {
    releaseMute(); ingest.mockRestore();
    await request({ type: 'close' });
    for (const socket of relay.clients) socket.terminate();
    await new Promise<void>(resolve => relay.close(() => resolve()));
    await Dexie.delete(store); await Dexie.delete(`${store}-nostr-index`);
  }
});

for (const action of ['setIdentity', 'close'] as const) {
  it(`does not resume stale initialization after ${action} during the retained-cache read`, async () => {
    const db = new Dexie('hashtree-social-graph'); db.version(1).stores({ socialGraph: '&id' });
    await db.table('socialGraph').clear();
    const graph = new SocialGraph(owner); graph.handleEvent(finalizeEvent({ kind: 3, created_at: 600, content: '', tags: [['p', friend]] }, key), true);
    const retained = { id: 'main', data: await graph.toBinary(), updatedAt: Date.now() };
    const proto = Object.getPrototypeOf(Object.getPrototypeOf(db.table('socialGraph')));
    const original = proto.get;
    let release: (() => void) | undefined, waiting = false;
    const barrier = new Promise<void>(resolve => { release = resolve; });
    const read = vi.spyOn(proto, 'get').mockImplementation(function(this: any, ...args: any[]) {
      if (!waiting && this.db.name === 'hashtree-social-graph' && args[0] === 'main') {
        waiting = true;
        return barrier.then(() => action === 'close' ? retained : undefined);
      }
      return original.apply(this, args);
    });
    const fetch = vi.fn().mockRejectedValue(new Error('Unexpected stale bootstrap'));
    vi.stubGlobal('fetch', fetch);
    const store = `graph-cancel-${action}-${crypto.randomUUID()}`;
    let pending: Promise<unknown> | undefined;
    try {
      pending = request({ type: 'init', config: { storeName: store, pubkey: owner, relays: ['ws://127.0.0.1:1'], graphBootstrap: true, graphReconciliation: false, blossomServers: [] } });
      await expect.poll(() => waiting).toBe(true);
      await request({ type: action, pubkey: friend });
      const versions = responses.filter(message => message.type === 'socialGraphVersion').length;
      release!(); await pending;
      expect(fetch).not.toHaveBeenCalled();
      expect(responses.filter(message => message.type === 'socialGraphVersion')).toHaveLength(versions);
      if (action === 'setIdentity') expect(await request({ type: 'getFollows', pubkey: owner })).toMatchObject({ pubkeys: [] });
    } finally {
      release?.(); await pending; read.mockRestore(); vi.unstubAllGlobals();
      vi.stubGlobal('self', scope); vi.stubGlobal('WebSocket', WebSocket);
      await request({ type: 'close' }); db.close();
      await Dexie.delete(store); await Dexie.delete(`${store}-nostr-index`);
    }
  });
}

it('preserves a newer local event arriving during the retained graph merge', async () => {
  const db = new Dexie('hashtree-social-graph'); db.version(1).stores({ socialGraph: '&id' });
  const retained = new SocialGraph(owner); retained.handleEvent(finalizeEvent({ kind: 3, created_at: 600, content: '', tags: [['p', friend]] }, key), true);
  const bytes = await retained.toBinary(); await db.table('socialGraph').put({ id: 'main', data: bytes, updatedAt: 1 });
  let readRelease!: () => void, mergeRelease!: () => void, reading = false, merging = false;
  const readWait = new Promise<void>(resolve => { readRelease = resolve; });
  const mergeWait = new Promise<void>(resolve => { mergeRelease = resolve; });
  const fromBinary = SocialGraph.fromBinary, merge = SocialGraph.prototype.merge;
  const decode = vi.spyOn(SocialGraph, 'fromBinary').mockImplementation(async (...args) => {
    const result = await fromBinary(...args); reading = true; await readWait; return result;
  });
  const mergingGraph = vi.spyOn(SocialGraph.prototype, 'merge').mockImplementation(async function(...args) {
    const result = await merge.apply(this, args); merging = true; await mergeWait; return result;
  });
  const store = `graph-merge-race-${crypto.randomUUID()}`;
  let pending: Promise<unknown> | undefined;
  try {
    pending = init(store); await expect.poll(() => reading).toBe(true);
    await request({ type: 'handleSocialGraphEvents', events: [finalizeEvent({ kind: 3, created_at: 650, content: '', tags: [['p', friend]] }, key)] });
    readRelease(); await expect.poll(() => merging).toBe(true);
    await request({ type: 'handleSocialGraphEvents', events: [finalizeEvent({ kind: 3, created_at: 700, content: '', tags: [['p', muted]] }, key)] });
    mergeRelease(); await pending;
    expect(await request({ type: 'getFollows', pubkey: owner })).toMatchObject({ pubkeys: [muted] });
    await request({ type: 'close' });
    expect((await db.table('socialGraph').get('main')).data).toEqual(bytes);
  } finally {
    readRelease(); mergeRelease(); await pending; decode.mockRestore(); mergingGraph.mockRestore();
    await request({ type: 'close' }); db.close(); await Dexie.delete(store); await Dexie.delete(`${store}-nostr-index`);
  }
});

it('does not restart graph reconciliation after close invalidates a pending relay change', async () => {
  const runtime = await import('../src/relay/nostrRuntime.js');
  const relay = new WebSocketServer({ port: 0 }); await once(relay, 'listening');
  const address = relay.address(); if (!address || typeof address === 'string') throw new Error('Missing relay');
  let connections = 0; relay.on('connection', () => { connections++; });
  const store = `graph-relays-close-${crypto.randomUUID()}`;
  await init(store, [], true);
  let relayRelease!: () => void, closeRelease!: () => void, closing = false;
  const relayWait = new Promise<void>(resolve => { relayRelease = resolve; });
  const closeWait = new Promise<void>(resolve => { closeRelease = resolve; });
  const originalClose = runtime.closeNostrRuntime;
  const relays = vi.spyOn(runtime, 'setRelays').mockImplementation(() => relayWait as any);
  const close = vi.spyOn(runtime, 'closeNostrRuntime').mockImplementation(async () => { closing = true; await closeWait; await originalClose(); });
  let changing: Promise<unknown> | undefined, stopping: Promise<unknown> | undefined;
  try {
    changing = request({ type: 'setRelays', relays: [`ws://127.0.0.1:${address.port}`], graphBootstrap: true });
    stopping = request({ type: 'close' }); await expect.poll(() => closing).toBe(true);
    relayRelease(); await changing; await Promise.resolve();
    await new Promise(resolve => setTimeout(resolve, 150));
    expect(connections).toBe(0);
    closeRelease(); await stopping;
  } finally {
    relayRelease(); closeRelease(); await changing; await stopping;
    relays.mockRestore(); close.mockRestore();
    await request({ type: 'close' });
    for (const socket of relay.clients) socket.terminate();
    await new Promise<void>(resolve => relay.close(() => resolve()));
    await Dexie.delete(store); await Dexie.delete(`${store}-nostr-index`);
  }
});

it('answers followed-by-friends with only the followers the viewer follows', async () => {
  const db = new Dexie('hashtree-social-graph'); db.version(1).stores({ socialGraph: '&id' });
  await db.table('socialGraph').clear(); db.close();
  const friendKey = new Uint8Array(32).fill(43), strangerKey = new Uint8Array(32).fill(45);
  const stranger = getPublicKey(strangerKey), target = getPublicKey(new Uint8Array(32).fill(46));
  const store = `graph-followed-by-${crypto.randomUUID()}`;
  try {
    await init(store);
    await request({ type: 'handleSocialGraphEvents', events: [
      finalizeEvent({ kind: 3, created_at: 1000, content: '', tags: [['p', friend]] }, key),
      finalizeEvent({ kind: 3, created_at: 1000, content: '', tags: [['p', stranger], ['p', target]] }, friendKey),
      finalizeEvent({ kind: 3, created_at: 1000, content: '', tags: [['p', target]] }, strangerKey),
    ] });
    const followers = await request({ type: 'getFollowers', pubkey: target }) as { pubkeys: string[] };
    expect([...followers.pubkeys].sort()).toEqual([friend, stranger].sort());
    const friends = await request({ type: 'getFollowedByFriends', pubkey: target });
    expect(friends).toMatchObject({ type: 'pubkeyList', pubkeys: [friend] });
    expect(friends).not.toHaveProperty('error');
  } finally {
    await request({ type: 'close' });
    await Dexie.delete(store); await Dexie.delete(`${store}-nostr-index`);
  }
});
