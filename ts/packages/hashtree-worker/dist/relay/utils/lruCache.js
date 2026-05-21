// @ts-nocheck
/**
 * Simple LRU Cache implementation
 */
export class LRUCache {
    cache = new Map();
    maxSize;
    constructor(maxSize = 100) {
        this.maxSize = maxSize;
    }
    get(key) {
        const value = this.cache.get(key);
        if (value !== undefined) {
            // Move to end (most recently used)
            this.cache.delete(key);
            this.cache.set(key, value);
        }
        return value;
    }
    set(key, value) {
        // Delete first to reset position if exists
        this.cache.delete(key);
        // Evict oldest if at capacity
        if (this.cache.size >= this.maxSize) {
            const oldest = this.cache.keys().next().value;
            if (oldest !== undefined) {
                this.cache.delete(oldest);
            }
        }
        this.cache.set(key, value);
    }
    has(key) {
        return this.cache.has(key);
    }
    delete(key) {
        return this.cache.delete(key);
    }
    clear() {
        this.cache.clear();
    }
    get size() {
        return this.cache.size;
    }
    keys() {
        return this.cache.keys();
    }
    values() {
        return this.cache.values();
    }
    /**
     * Iterate over all entries (note: does not update LRU order)
     */
    *entries() {
        yield* this.cache.entries();
    }
    forEach(callback) {
        this.cache.forEach(callback);
    }
    /**
     * Make the cache iterable
     */
    [Symbol.iterator]() {
        return this.entries();
    }
}
//# sourceMappingURL=lruCache.js.map