import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { DexieStore } from '@hashtree/dexie';
import { createNostrRuntime, selectStoredEvents, type NostrFilter } from 'nostr-pubsub';
import { finalizeEvent } from 'nostr-tools/pure';
import { afterEach, expect, it, vi } from 'vitest';
import { createWorkerEventStore } from '../src/relay/eventIndex.js';

const name = 'indexed-events-test';
const secret = new Uint8Array(32).fill(5);
const event = (kind: number, created_at: number, tags: string[][] = [], content = '') =>
  finalizeEvent({ kind, created_at, tags, content }, secret);
const blocks = new DexieStore(name);
function open(indexed = true) {
  return createWorkerEventStore(blocks, name, undefined, indexed);
}
afterEach(async () => {
  vi.restoreAllMocks();
  await Dexie.delete(name + '-nostr-index');
});

it('stores relay history across restarts without rewriting block trees', async () => {
  const put = vi.spyOn(blocks, 'put');
  const data = [
    event(21, 4, [['t', 'both']]), event(21, 4, [['t', 'both']], 'tie'),
    event(22, 3, [['t', 'both']]), event(22, 1), event(21, 9, [['expiration', '1']]),
  ];
  let store = open();
  try {
    for (const e of data) await store.put(e);
    await expect(store.put({ ...data[0], content: 'edited' })).rejects.toThrow();
    await store.close(); store = open();
    const queries: NostrFilter[][] = [
      [{ kinds: [21], limit: 1 }], [{ authors: [data[0].pubkey], kinds: [21], since: 4, until: 4 }],
      [{ '#t': ['both'], limit: 2 }, { kinds: [21, 22], limit: 4 }], [{}], [{ limit: 0 }],
      [{ ids: data.slice(0, 2).map(e => e.id) }], [{ since: 8, until: 1 }],
    ];
    for (const filters of queries) {
      expect((await store.query(filters)).map(e => e.id)).toEqual(selectStoredEvents(data, filters).map(e => e.id));
    }
    expect(put).not.toHaveBeenCalled();
  } finally { await store.close(); }
});

it('keeps old history and pending publications and deletes superseded data in both indexes', async () => {
  const old = event(34235, 10, [['d', 'clip']]), recent = event(34235, 11, [['d', 'clip']]);
  const retained = event(1, 2), other = event(1, 1);
  const previous = open(false);
  await previous.put(old); await previous.put(retained);
  await previous.putPending({ event: old, updatedAt: 10, attempts: 0, relays: [], sources: [] });
  await previous.close();
  let store = open();
  const runtime = createNostrRuntime({ store, relays: [] });
  try {
    await runtime.ingest(recent, 'fixture'); await runtime.ingest(other, 'fixture');
    await runtime.ingest(retained, 'fixture');
    expect((await store.query([{ kinds: [1], limit: 2 }])).map(e => e.id)).toEqual([retained.id, other.id]);
    expect((await store.query([{ kinds: [34235] }])).map(e => e.id)).toEqual([recent.id]);
    await runtime.ingest(event(5, 15, [['e', retained.id], ['e', other.id]]), 'fixture');
    await runtime.close(); store = open();
    expect(await store.query([{ kinds: [1] }])).toEqual([]);
    expect((await store.listPending()).map(p => p.event.id)).toEqual([old.id]);
  } finally { await store.close(); }
});

it('keeps the deterministic replaceable winner across competing local writers', async () => {
  const a = open(), b = open();
  const revisions = [event(0, 7, [], 'a'), event(0, 7, [], 'b')].sort((x,y) => x.id.localeCompare(y.id));
  try {
    await Promise.all([a.put(revisions[0]), b.put(revisions[1])]);
    await a.put(event(0, 6));
    expect((await b.query([{ kinds: [0] }])).map(e => e.id)).toEqual([revisions[0].id]);
  } finally { await a.close(); await b.close(); }
});

it('keeps a cached deletion effective when another tab writes the target', async () => {
  const a = open(), b = open();
  const target = event(21, 5, [], 'deleted');
  const deletion = event(5, 6, [['e', target.id]]);
  try {
    await Promise.all([a.put(deletion), b.put(target)]);
    expect(await b.query([{ ids: [target.id] }])).toEqual([]);
    expect((await a.query([{ kinds: [5] }])).map(e => e.id)).toEqual([deletion.id]);
  } finally { await a.close(); await b.close(); }
});
