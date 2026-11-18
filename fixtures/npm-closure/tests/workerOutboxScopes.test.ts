import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { DexieStore } from '@hashtree/dexie';
import { createNostrRuntime } from 'nostr-pubsub';
import { finalizeEvent } from 'nostr-tools/pure';
import { expect, it } from 'vitest';
import { createWorkerEventStore } from '@hashtree/worker/event-store';

for (const indexedCache of [false, true]) {
  it(`retains independently queued destinations after worker store restart (indexedCache=${indexedCache})`, async () => {
    const name = `shipping-scopes-${crypto.randomUUID()}`;
    let blocks = new DexieStore(name);
    let runtime = createNostrRuntime({ relays: [], store: createWorkerEventStore(blocks, name, undefined, indexedCache) });
    const event = finalizeEvent({ kind: 1, created_at: 100, tags: [], content: 'independent queued destinations' }, new Uint8Array(32).fill(12));
    try {
      for (const source of ['one', 'two', 'three']) {
        const result = await runtime.publish(event, { relays: [], sources: [source] });
        expect(result.queued).toBe(true);
        expect(result.remoteAccepted).toBe(false);
      }
      await runtime.close();
      blocks.close();
      blocks = new DexieStore(name);
      const stored = createWorkerEventStore(blocks, name, undefined, indexedCache);
      runtime = createNostrRuntime({ relays: [], store: stored });
      const pending = await stored.listPending();
      expect(pending).toHaveLength(1);
      expect(pending[0].event.id).toBe(event.id);
      expect([pending[0], ...(pending[0].additionalScopes ?? [])].map(scope => scope.sources)).toEqual([['one'], ['two'], ['three']]);
      expect((await stored.query([{ ids: [event.id] }])).map(value => value.id)).toEqual([event.id]);
    } finally {
      await runtime.close(); blocks.close();
      await Dexie.delete(name); await Dexie.delete(`${name}-nostr-index`);
    }
  });
}
