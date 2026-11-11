import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { nip19 } from 'nostr-tools';
import { finalizeEvent, getPublicKey } from 'nostr-tools/pure';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import type { WorkerRequest, WorkerResponse } from '../src/relay/protocol.js';

const persistence = vi.hoisted(() => ({ blocked: false, calls: 0 }));

vi.mock('../src/relay/treeRootSubscription', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../src/relay/treeRootSubscription')>();
  return {
    ...actual,
    handleTreeRootEvent: (event: Parameters<typeof actual.handleTreeRootEvent>[0]) => {
      persistence.calls++;
      return persistence.blocked ? new Promise<void>(() => {}) : actual.handleTreeRootEvent(event);
    },
  };
});

const secret = new Uint8Array(32).fill(5);
const pubkey = getPublicKey(secret);
const responses: WorkerResponse[] = [];
const scope = {
  onmessage: null as null | ((event: MessageEvent<WorkerRequest>) => Promise<void>),
  postMessage: (message: WorkerResponse) => { responses.push(message); },
};
let storeName: string;
let sequence = 0;

async function request(payload: Record<string, unknown>): Promise<string> {
  const id = `forward-${++sequence}`;
  await scope.onmessage!({ data: { ...payload, id } } as MessageEvent<WorkerRequest>);
  return id;
}

function rootEvent(treeName: string, hash: string, createdAt: number) {
  return finalizeEvent({
    kind: 30064,
    created_at: createdAt,
    content: '',
    tags: [['d', treeName], ['l', 'hashtree'], ['hash', hash]],
  }, secret);
}

function forwarded(subId: string) {
  return responses.filter((message) => message.type === 'event' && message.subId === subId);
}

async function waitFor(condition: () => boolean, timeoutMs = 15_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!condition()) {
    if (Date.now() > deadline) throw new Error('Timed out waiting for worker messages');
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

beforeAll(async () => {
  vi.stubGlobal('self', scope);
  await import('../src/relay/worker.js');
});

beforeEach(async () => {
  responses.length = 0;
  persistence.blocked = false;
  persistence.calls = 0;
  storeName = `event-forwarding-${crypto.randomUUID()}`;
  await request({ type: 'init', config: { storeName, pubkey, relays: [], blossomServers: [] } });
});

afterEach(async () => {
  persistence.blocked = false;
  await request({ type: 'close' });
  await Dexie.delete(storeName);
  await Dexie.delete(`${storeName}-nostr-index`);
});

afterAll(() => { vi.unstubAllGlobals(); });

describe('relay worker event forwarding', () => {
  it('forwards a tree root event while its persistence is still pending', async () => {
    persistence.blocked = true;
    const subId = await request({ type: 'subscribe', filters: [{ kinds: [30064], authors: [pubkey] }] });
    const event = rootEvent('docs', 'ab'.repeat(32), 1_700_000_000);

    await request({ type: 'ingestEvents', events: [event] });
    await waitFor(() => persistence.calls > 0);
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(forwarded(subId)).toMatchObject([{ type: 'event', subId, event: { id: event.id } }]);
  });

  it('forwards a burst in arrival order and still reports each new root', async () => {
    const subId = await request({ type: 'subscribe', filters: [{ kinds: [30064], authors: [pubkey] }] });
    const events = Array.from({ length: 12 }, (_, index) => (
      rootEvent(`tree-${index}`, index.toString(16).padStart(2, '0').repeat(32), 1_700_000_000 + index)
    ));

    await request({ type: 'ingestEvents', events });
    await waitFor(() => responses.filter((message) => message.type === 'treeRootUpdate').length === events.length);

    expect(forwarded(subId).map((message) => (message as { event: { id: string } }).event.id))
      .toEqual(events.map((event) => event.id));
    const updates = responses.filter((message) => message.type === 'treeRootUpdate') as Array<{ treeName: string; updatedAt: number; snapshotNhash?: string }>;
    expect(updates.map((update) => update.treeName).sort()).toEqual(events.map((_, index) => `tree-${index}`).sort());
    for (const update of updates) {
      expect(update.snapshotNhash).toMatch(/^nhash1/);
    }

    // The grouped writes are durable: a restarted worker serves the same roots.
    await request({ type: 'close' });
    await request({ type: 'init', config: { storeName, pubkey, relays: [], blossomServers: [] } });
    for (const update of updates) {
      const id = await request({ type: 'getTreeRootInfo', npub: nip19.npubEncode(pubkey), treeName: update.treeName });
      const info = responses.find((message) => 'id' in message && message.id === id);
      expect(info).toMatchObject({
        type: 'treeRootInfo',
        record: { updatedAt: update.updatedAt, snapshotNhash: update.snapshotNhash },
      });
    }
  }, 20_000);
});
