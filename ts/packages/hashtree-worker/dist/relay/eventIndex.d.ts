import { type Store } from '@hashtree/core';
import type { DexieStore } from '@hashtree/dexie';
import { HashtreeRuntimeEventStore } from '@hashtree/nostr-pubsub';
/** Event bodies and indexes share the existing Hashtree block cache. */
export declare function createWorkerEventStore(store: Store, storeName: string): HashtreeRuntimeEventStore;
/** Evict disposable file blocks without breaking durable event and outbox indexes. */
export declare function evictWorkerCache(store: DexieStore, storeName: string, maxBytes: number): Promise<number>;
//# sourceMappingURL=eventIndex.d.ts.map