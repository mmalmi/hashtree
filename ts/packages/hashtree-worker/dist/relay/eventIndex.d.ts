import { type Store } from '@hashtree/core';
import { type RuntimeEventStore } from 'nostr-pubsub';
import type { DexieStore } from '@hashtree/dexie';
type WorkerEventStore = RuntimeEventStore & {
    close(): Promise<void>;
};
export declare function createWorkerEventStore(store: Store, storeName: string, onPersisted?: () => void, indexedCache?: boolean): WorkerEventStore;
/** Evict disposable file blocks without breaking durable event and outbox indexes. */
export declare function evictWorkerCache(store: DexieStore, storeName: string, maxBytes: number): Promise<number>;
export {};
//# sourceMappingURL=eventIndex.d.ts.map