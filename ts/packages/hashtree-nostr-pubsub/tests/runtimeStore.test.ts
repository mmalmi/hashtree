import { describe, expect, it } from 'vitest';
import { MemoryStore } from '@hashtree/core';
import { createNostrRuntime, verifyNostrEvent } from 'nostr-pubsub';
import { finalizeEvent } from 'nostr-tools/pure';
import { HashtreeRuntimeEventStore, type HashtreeRuntimeState } from '../src/runtimeStore.js';

const secret = new Uint8Array(32).fill(7);
const event = (created_at: number, kind = 1) => finalizeEvent({
  created_at, kind, tags: [['d', 'docs']], content: `entry ${created_at}`,
}, secret);

function fixture() {
  const blocks = new MemoryStore();
  let state: HashtreeRuntimeState | null = null;
  const metadata = {
    load: async () => structuredClone(state),
    save: async (next: HashtreeRuntimeState) => { state = structuredClone(next); },
  };
  return { blocks, open: () => new HashtreeRuntimeEventStore(blocks, metadata) };
}

describe('Hashtree runtime event persistence', () => {
  it('queries saved indexes and pending publications after reopening without a network', async () => {
    const db = fixture();
    const first = db.open();
    const signed = event(10);
    await first.put(signed);
    await first.putPending({ event: JSON.parse(JSON.stringify(signed)), attempts: 2, updatedAt: 100, relays: ['wss://relay.example'] });
    await first.close();
    const reopened = db.open();
    expect((await reopened.query([{ authors: [signed.pubkey], '#d': ['docs'] }])).map((entry) => entry.id)).toEqual([signed.id]);
    expect(await reopened.listPending()).toEqual([{ event: JSON.parse(JSON.stringify(signed)), attempts: 2, updatedAt: 100, relays: ['wss://relay.example'] }]);
    await reopened.deletePending(signed.id);
    expect(await db.open().listPending()).toEqual([]);
    expect(await db.open().query([{ ids: [signed.id] }])).toHaveLength(1);
  });

  it('retains duplicate relay history and updates retry metadata after reopening', async () => {
    const db = fixture();
    const signed = event(10, 7368);
    const first = db.open();
    await first.put(signed);
    await first.putPending({ event: signed, attempts: 1, updatedAt: 10 });
    await first.close();
    const reopened = db.open();
    await reopened.put(structuredClone(signed));
    await reopened.putPending({ event: structuredClone(signed), attempts: 2, updatedAt: 20 });
    expect((await reopened.query([{ kinds: [7368], authors: [signed.pubkey] }])).map(value => value.id)).toEqual([signed.id]);
    expect(await reopened.listPending()).toMatchObject([{ event: { id: signed.id }, attempts: 2, updatedAt: 20 }]);
  });

  it('returns repeated remote history through the production runtime after an index restart', async () => {
    const db = fixture();
    const signed = event(10, 7368);
    const source = {
      id: 'relay-fixture',
      query: async () => ({ complete: true, events: [{ event: verifyNostrEvent(signed), source: { id: 'relay-fixture', kind: 'relay' as const }, priority: 0 }] }),
    };
    for (let attempt = 0; attempt < 2; attempt++) {
      const runtime = createNostrRuntime({ store: db.open(), sources: [source] });
      try {
        const result = await runtime.query([{ kinds: [7368], authors: [signed.pubkey] }], { cache: 'network-only' });
        expect(result.complete).toBe(true);
        expect(result.events.map(value => value.id)).toEqual([signed.id]);
      } finally { await runtime.close(); }
    }
  });

  it('removes events from all indexes and keeps the newest replaceable root', async () => {
    const db = fixture();
    const store = db.open();
    const oldRoot = event(10, 30064);
    const newRoot = event(20, 30064);
    await store.put(oldRoot);
    await store.put(newRoot);
    expect((await store.query([{ kinds: [30064] }])).map((entry) => entry.id)).toEqual([newRoot.id]);
    await store.delete([oldRoot.id]);
    expect(await store.query([{ authors: [newRoot.pubkey] }])).toHaveLength(1);
    await store.delete([newRoot.id]);
    expect(await db.open().query([{}])).toEqual([]);
  });

  it('keeps event blocks referenced by another retained index when replacing outbox history', async () => {
    const db = fixture();
    const store = db.open();
    const saved = event(10, 30064);
    const queued = event(20, 30064);
    await store.put(saved);
    await store.putPending({ event: saved, attempts: 0, updatedAt: 10 });
    await store.putPending({ event: queued, attempts: 0, updatedAt: 20 });
    await store.close();
    const reopened = db.open();
    expect((await reopened.query([{ kinds: [30064] }])).map((entry) => entry.id)).toEqual([saved.id]);
    expect((await reopened.listPending()).map(({ event }) => event.id)).toEqual([queued.id]);
  });

  it('serializes concurrent writes without losing an index entry', async () => {
    const store = fixture().open();
    await Promise.all([store.put(event(1)), store.put(event(2)), store.put(event(3))]);
    expect((await store.query([{}])).map((entry) => entry.created_at)).toEqual([3, 2, 1]);
  });
});
