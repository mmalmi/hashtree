import type { CID, CollectionDefinition, CollectionManifest, CollectionMutation, CollectionReindexEntries, CollectionState, CollectionWriteContext, Store } from './types.js';
export declare class CollectionWriter<T> {
    private readonly store;
    private readonly definition;
    private readonly hasDerivedIndexes;
    private readonly byIdIndex;
    private readonly linkIndex;
    private readonly searchIndexes;
    private state;
    constructor(store: Store, definition: CollectionDefinition<T>, initialManifest?: CollectionManifest | null);
    get snapshot(): CollectionState;
    manifest(metadata?: Record<string, unknown>): CollectionManifest;
    normalize(item: unknown, fromVersion?: number): T;
    replace(item: T, cid: CID, previous: T, options?: {
        context?: CollectionWriteContext;
        previousContext?: CollectionWriteContext;
    }): Promise<CollectionState>;
    put(item: T, cid: CID, options?: {
        previous?: T;
        context?: CollectionWriteContext;
        previousContext?: CollectionWriteContext;
    }): Promise<CollectionState>;
    delete(item: T, options?: {
        context?: CollectionWriteContext;
    }): Promise<CollectionState>;
    batch(mutations: Iterable<CollectionMutation<T>>): Promise<CollectionState>;
    rebuild(entries: CollectionReindexEntries<T>): Promise<CollectionState>;
    reindex(entries: CollectionReindexEntries<T>): Promise<CollectionState>;
    private readSearchRootGroup;
    private assignSearchRootGroups;
}
//# sourceMappingURL=writer.d.ts.map