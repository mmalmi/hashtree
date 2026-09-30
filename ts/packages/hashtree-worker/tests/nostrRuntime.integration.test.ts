import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { DexieStore } from '@hashtree/dexie';
import { sha256 } from '@hashtree/core';
import { getPublicKey, verifyEvent } from 'nostr-tools/pure';
import { expect, it } from 'vitest';
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
    const persisted = createWorkerEventStore(blocks, name);
    expect((await persisted.listPending()).map(({ event }) => event.id)).toEqual([signed.id]);
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
