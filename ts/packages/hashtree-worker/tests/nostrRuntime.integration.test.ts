import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { DexieStore } from '@hashtree/dexie';
import { sha256 } from '@hashtree/core';
import { finalizeEvent, getPublicKey, verifyEvent } from 'nostr-tools/pure';
import { once } from 'node:events';
import { WebSocket, WebSocketServer } from 'ws';
import { expect, it, vi } from 'vitest';
import { initNostrRuntime, closeNostrRuntime, publish, query } from '../src/relay/nostrRuntime.js';
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
