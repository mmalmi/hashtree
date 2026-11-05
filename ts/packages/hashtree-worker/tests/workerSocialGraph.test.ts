import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { once } from 'node:events';
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
  let negotiations = 0;
  relay.on('connection', socket => socket.on('message', bytes => {
    const frame = JSON.parse(bytes.toString());
    if (frame[0] === 'NEG-OPEN') {
      negotiations++;
      socket.send(JSON.stringify(['NEG-ERR', frame[1], 'unsupported']));
    }
    if (frame[0] !== 'REQ') return;
    for (const event of heads) if (frame.slice(2).some((filter: { kinds?: number[]; authors?: string[]; ids?: string[] }) =>
      (!filter.kinds || filter.kinds.includes(event.kind)) && (!filter.authors || filter.authors.includes(event.pubkey)) && (!filter.ids || filter.ids.includes(event.id)))) {
      socket.send(JSON.stringify(['EVENT', frame[1], event]));
    }
    socket.send(JSON.stringify(['EOSE', frame[1]]));
  }));
  const store = `graph-relay-${crypto.randomUUID()}`;
  try {
    await init(store, [`ws://127.0.0.1:${address.port}`], true);
    await expect.poll(() => negotiations, { timeout: 5000 }).toBeGreaterThan(0);
    await expect.poll(async () => (await request({ type: 'getFollows', pubkey: owner }) as { pubkeys?: string[] })?.pubkeys).toEqual([muted]);
    await request({ type: 'close' });
    const graph = await saved();
    expect(graph.getListRevision(owner, 3)?.id).toBe(heads[0]!.id);
    expect(graph.getListRevision(owner, 10000)?.id).toBe(heads[1]!.id);
  } finally {
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
