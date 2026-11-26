import Dexie from 'dexie';
import { HashTree, toHex } from '@hashtree/core';
import { NostrEventStore } from '@hashtree/nostr';
import { selectStoredEvents, storeRuntimeEvent, verifyNostrEvent } from 'nostr-pubsub';
import { EVENT_SCHEMA, eventRow, queryIndexedEvents } from './indexedEventCache.js';
import { HashtreeRuntimeEventStore, } from '@hashtree/nostr-pubsub';
class EventIndexState extends Dexie {
    state;
    events;
    constructor(name) {
        super(name);
        this.version(1).stores({ state: '&id' });
        this.version(2).stores({ events: EVENT_SCHEMA });
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
export function createWorkerEventStore(store, storeName, onPersisted, indexedCache = false) {
    const name = `${storeName}-nostr-index`;
    const db = new EventIndexState(name);
    const checkedStore = {
        get: (hash) => store.get(hash),
        has: (hash) => store.has(hash),
        delete: (hash) => store.delete(hash),
        put: async (hash, data) => {
            const added = await store.put(hash, data);
            // Store.put(false) may mean deduplication or a failed Dexie write.
            if (!added && !await store.has(hash))
                throw new Error('Failed to persist event index block');
            return added;
        },
    };
    const portable = new HashtreeRuntimeEventStore(checkedStore, {
        load: async () => (await db.state.get('roots'))?.value ?? null,
        save: async (value) => {
            await db.state.put({ id: 'roots', value });
            onPersisted?.();
        },
        withLock: (operation) => withIndexLock(name, operation),
        close: () => db.close(),
    });
    if (!indexedCache)
        return portable;
    const legacyEvents = new NostrEventStore(checkedStore);
    const local = {
        query: (filters, options = {}) => queryIndexedEvents(db.events, filters, options),
        put: async (event) => { await db.events.put(eventRow(event)); },
        delete: ids => db.events.bulkDelete(ids),
        listPending: () => portable.listPending(),
        putPending: entry => portable.putPending(entry),
        deletePending: id => portable.deletePending(id),
        close: () => portable.close(),
    };
    return {
        ...local,
        async query(filters, options = {}) {
            const indexed = await local.query(filters, options);
            if (!(await db.state.get('roots'))?.value.root)
                return indexed;
            const previous = await portable.query(filters, options);
            const unique = new Map([...previous, ...indexed].map(event => [event.id, event]));
            return selectStoredEvents(unique.values(), filters, options);
        },
        async put(event) {
            const signed = verifyNostrEvent(event);
            await db.transaction('rw', db.events, async () => {
                if (await db.events.where(':id').equals(signed.id).count())
                    return;
                // Another tab can receive a deletion or a newer revision between
                // runtime admission and this write. Recheck inside the IDB transaction.
                await storeRuntimeEvent(local, signed);
            });
        },
        async delete(ids) {
            if (!ids.length)
                return;
            // A pre-upgrade revision may live in the portable index. Commit its new
            // root and the local deletions together, under the existing writer lock.
            await withIndexLock(name, async () => {
                const saved = await db.state.get('roots');
                let root = saved?.value.root;
                if (root)
                    for (const id of ids)
                        root = await legacyEvents.delete(root, id);
                await db.transaction('rw', db.state, db.events, async () => {
                    if (saved && root !== saved.value.root) {
                        await db.state.put({ ...saved, value: { ...saved.value, root: root ?? null } });
                    }
                    await db.events.bulkDelete(ids);
                });
                if (saved && root !== saved.value.root)
                    onPersisted?.();
            });
        },
    };
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