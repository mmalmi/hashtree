/**
 * Simple LRU Cache implementation
 */
export declare class LRUCache<K, V> {
    private cache;
    private maxSize;
    constructor(maxSize?: number);
    get(key: K): V | undefined;
    set(key: K, value: V): void;
    has(key: K): boolean;
    delete(key: K): boolean;
    clear(): void;
    get size(): number;
    keys(): IterableIterator<K>;
    values(): IterableIterator<V>;
    /**
     * Iterate over all entries (note: does not update LRU order)
     */
    entries(): IterableIterator<[K, V]>;
    forEach(callback: (value: V, key: K) => void): void;
    /**
     * Make the cache iterable
     */
    [Symbol.iterator](): IterableIterator<[K, V]>;
}
//# sourceMappingURL=lruCache.d.ts.map