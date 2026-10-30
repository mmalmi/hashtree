import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { DexieStore } from '@hashtree/dexie';
import { sha256 } from '@hashtree/core';
import { finalizeEvent, getPublicKey, verifyEvent } from 'nostr-tools/pure';
import { once } from 'node:events';
import { WebSocket, WebSocketServer } from 'ws';
import { expect, it, vi } from 'vitest';
import { initNostrRuntime, closeNostrRuntime, publish, query, ingestEvents } from '../src/relay/nostrRuntime.js';
import { initIdentity, clearIdentity } from '../src/relay/identity.js';
import { signEvent } from '../src/relay/signing.js';
import { createWorkerEventStore, evictWorkerCache } from '../src/relay/eventIndex.js';

it('retains signed roots and pending publications through an offline worker restart with the existing key', async () => {
  const name = `worker-runtime-${crypto.randomUUID()}`;
  const secret = new Uint8Array(32).fill(3);
  const pubkey = getPublicKey(secret);
  const nsecHex = [...secret].map((byte) => byte.toString(16).padStart(2, '0')).join('');
  let blocks = new DexieStore(name);
  try {
    initIdentity(pubkey, nsecHex);
    await initNostrRuntime([], { store: blocks, storeName: name });
    const signed = await signEvent({ kind: 30064, created_at: 123, content: '', tags: [['d', 'offline-doc'], ['l', 'hashtree'], ['hash', 'ab'.repeat(32)]] });
    expect(signed.pubkey).toBe(pubkey);
    expect(verifyEvent(signed)).toBe(true);
    const receipt = await publish(signed);
    expect(receipt.queued).toBe(true);
    expect(receipt.remoteAccepted).toBe(false);
    const disposable = new Uint8Array([1, 2, 3]);
    const disposableHash = await sha256(disposable);
    await blocks.put(disposableHash, disposable);
    expect(await evictWorkerCache(blocks, name, 0)).toBeGreaterThan(0);
    expect(await blocks.get(disposableHash)).toBeNull();
    await closeNostrRuntime();
    blocks.close();

    blocks = new DexieStore(name);
    await initNostrRuntime([], { store: blocks, storeName: name });
    const cached = await query([{ authors: [pubkey], kinds: [30064], '#d': ['offline-doc'] }], { cache: 'cache-only' });
    expect(cached.complete).toBe(true);
    expect(cached.events.map((entry) => entry.id)).toEqual([signed.id]);
    const updated = await signEvent({ ...signed, created_at: 125, content: 'updated root' });
    expect((await publish(updated)).queued).toBe(true);
    expect((await query([{ authors: [pubkey], kinds: [30064] }], { cache: 'cache-only' })).events.map(({ id }) => id)).toEqual([updated.id]);
    const persisted = createWorkerEventStore(blocks, name);
    expect((await persisted.listPending()).map(({ event }) => event.id)).toEqual([updated.id]);
    await persisted.close();
    const afterRestart = await signEvent({ kind: 1, created_at: 124, content: 'same identity', tags: [] });
    expect(afterRestart.pubkey).toBe(pubkey);
    expect(verifyEvent(afterRestart)).toBe(true);
  } finally {
    await closeNostrRuntime();
    blocks.close(); clearIdentity();
    await Dexie.delete(name); await Dexie.delete(`${name}-nostr-index`);
  }
});


it('batches worker interests within common relay filter limits without losing exact events', async () => {
  const relay = new WebSocketServer({ port: 0 });
  await once(relay, 'listening');
  const address = relay.address();
  if (!address || typeof address === 'string') throw new Error('Missing relay port');
  const events = Array.from({ length: 41 }, (_, index) => finalizeEvent({
    kind: 1, created_at: 100 + index, content: `worker interest ${index}`, tags: [],
  }, new Uint8Array(32).fill(5)));
  const receivedFilterCounts: number[] = [];
  relay.on('connection', socket => socket.on('message', data => {
    const frame = JSON.parse(data.toString());
    if (frame[0] !== 'REQ') return;
    const filters = frame.slice(2) as Array<{ ids?: string[] }>;
    receivedFilterCounts.push(filters.length);
    if (filters.length > 20) {
      socket.send(JSON.stringify(['CLOSED', frame[1], 'invalid: max 20 filters']));
      return;
    }
    for (const event of events) {
      if (filters.some(filter => filter.ids?.includes(event.id))) {
        socket.send(JSON.stringify(['EVENT', frame[1], event]));
      }
    }
    socket.send(JSON.stringify(['EOSE', frame[1]]));
  }));
  const name = `worker-relay-batch-${crypto.randomUUID()}`;
  const blocks = new DexieStore(name);
  vi.stubGlobal('WebSocket', WebSocket);
  try {
    await initNostrRuntime([`ws://127.0.0.1:${address.port}`], { store: blocks, storeName: name });
    const results = await Promise.all(events.map(event => query([{ ids: [event.id] }], {
      cache: 'network-only', deadline: Date.now() + 5000,
    })));
    expect(results.every(result => result.complete)).toBe(true);
    expect(results.map(result => result.events.map(event => event.id))).toEqual(events.map(event => [event.id]));
    expect(receivedFilterCounts.length).toBeGreaterThan(1);
    expect(Math.max(...receivedFilterCounts)).toBeLessThanOrEqual(20);
  } finally {
    await closeNostrRuntime();
    blocks.close();
    vi.unstubAllGlobals();
    for (const socket of relay.clients) socket.terminate();
    await new Promise<void>(resolve => relay.close(() => resolve()));
    await Dexie.delete(name);
    await Dexie.delete(`${name}-nostr-index`);
  }
});


it('schedules cache eviction for imported events while retaining current event and outbox roots', async () => {
  const name = `worker-ingest-eviction-${crypto.randomUUID()}`;
  const blocks = new DexieStore(name);
  const evictions: Promise<number>[] = [];
  const events = Array.from({ length: 4 }, (_, index) => finalizeEvent({
    kind: 30064, created_at: 100 + index, content: `revision ${index}`, tags: [['d', 'document']],
  }, new Uint8Array(32).fill(6)));
  try {
    await initNostrRuntime([], {
      store: blocks, storeName: name,
      onStorePersisted: () => { evictions.push(evictWorkerCache(blocks, name, 0)); },
    });
    await publish(events[0]);
    for (const event of events.slice(1)) await ingestEvents([event]);
    await Promise.all(evictions);
    expect(evictions.length).toBeGreaterThanOrEqual(events.length);
    expect((await Promise.all(evictions)).some(count => count > 0)).toBe(true);
    expect((await query([{ kinds: [30064] }], { cache: 'cache-only' })).events.map(event => event.id)).toEqual([events[3].id]);
    const persisted = createWorkerEventStore(blocks, name);
    try {
      expect((await persisted.listPending()).map(({ event }) => event.id)).toEqual([events[0].id]);
    } finally { await persisted.close(); }
  } finally {
    await closeNostrRuntime();
    await Promise.all(evictions);
    blocks.close();
    await Dexie.delete(name); await Dexie.delete(`${name}-nostr-index`);
  }
});


it('does not commit event or outbox roots after failed block writes', async () => {
  const name = `worker-write-failure-${crypto.randomUUID()}`;
  const blocks = new DexieStore(name);
  const events = [100, 101].map(created_at => finalizeEvent({
    kind: 30064, created_at, content: `revision ${created_at}`, tags: [['d', 'document']],
  }, new Uint8Array(32).fill(8)));
  const stored = createWorkerEventStore(blocks, name);
  const pending = (event: typeof events[number]) => ({ event, attempts: 0, updatedAt: 100, relays: [], sources: [] });
  try {
    await stored.put(events[0]);
    await stored.putPending(pending(events[0]));
    const failed = vi.spyOn(blocks, 'put').mockResolvedValue(false);
    try {
      await expect(stored.put(events[1])).rejects.toThrow('Failed to persist event index block');
      await expect(stored.putPending(pending(events[1]))).rejects.toThrow('Failed to persist event index block');
    } finally { failed.mockRestore(); }
    expect((await stored.query([{ kinds: [30064] }])).map(event => event.id)).toEqual([events[0].id]);
    expect((await stored.listPending()).map(({ event }) => event.id)).toEqual([events[0].id]);
    expect(await evictWorkerCache(blocks, name, 0)).toBeGreaterThanOrEqual(0);
    expect((await stored.query([{ kinds: [30064] }])).map(event => event.id)).toEqual([events[0].id]);
    expect((await stored.listPending()).map(({ event }) => event.id)).toEqual([events[0].id]);
    const put = blocks.put.bind(blocks);
    const deduplicated = vi.spyOn(blocks, 'put').mockImplementation(async (hash, data) => {
      await put(hash, data);
      return false;
    });
    try { await stored.put(events[1]); } finally { deduplicated.mockRestore(); }
    expect((await stored.query([{ kinds: [30064] }])).map(event => event.id)).toEqual([events[1].id]);
  } finally {
    await stored.close(); blocks.close();
    await Dexie.delete(name); await Dexie.delete(`${name}-nostr-index`);
  }
});
