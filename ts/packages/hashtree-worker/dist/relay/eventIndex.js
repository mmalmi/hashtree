import Dexie from 'dexie';
import { HashTree, toHex } from '@hashtree/core';
import { HashtreeRuntimeEventStore, } from '@hashtree/nostr-pubsub';
class EventIndexState extends Dexie {
    state;
    constructor(name) {
        super(name);
        this.version(1).stores({ state: '&id' });
    }
}
const localLocks = new Map();
async function withIndexLock(name, operation) {
    if (typeof navigator !== 'undefined' && navigator.locks)
        return navigator.locks.request(name, operation);
    const pending = (localLocks.get(name) ?? Promise.resolve()).then(operation);
    const settled = pending.catch(() => undefined);
    localLocks.set(name, settled);
    try {
        return await pending;
    }
    finally {
        if (localLocks.get(name) === settled)
            localLocks.delete(name);
    }
}
/** Event bodies and indexes share the existing Hashtree block cache. */
export function createWorkerEventStore(store, storeName) {
    const name = `${storeName}-nostr-index`;
    const db = new EventIndexState(name);
    return new HashtreeRuntimeEventStore(store, {
        load: async () => (await db.state.get('roots'))?.value ?? null,
        save: async (value) => { await db.state.put({ id: 'roots', value }); },
        withLock: (operation) => withIndexLock(name, operation),
        close: () => db.close(),
    });
}
/** Evict disposable file blocks without breaking durable event and outbox indexes. */
export async function evictWorkerCache(store, storeName, maxBytes) {
    if (await store.totalBytes() <= maxBytes)
        return 0;
    const name = `${storeName}-nostr-index`;
    return withIndexLock(name, async () => {
        const db = new EventIndexState(name);
        try {
            const state = (await db.state.get('roots'))?.value;
            const protectedHashes = new Set();
            const tree = new HashTree({ store });
            for (const root of [state?.root, state?.outboxRoot]) {
                if (root)
                    for await (const block of tree.walkBlocks(root))
                        protectedHashes.add(toHex(block.hash));
            }
            return store.evict(maxBytes, protectedHashes);
        }
        finally {
            db.close();
        }
    });
}
//# sourceMappingURL=eventIndex.js.map