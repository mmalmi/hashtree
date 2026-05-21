/**
 * TreeRootRegistry - Browser-side single source of truth for tree root data
 *
 * This module provides:
 * - Unified record format for all root data
 * - Subscription API that emits cached data immediately, then updates
 * - Async resolve with timeout for waiting on first resolution
 * - Local write tracking with dirty flag for publish throttling
 * - Pluggable persistence (localStorage by default)
 *
 * This module lives under @hashtree/worker because it coordinates with worker
 * updates and publish flows, but it runs on the app/main thread.
 */
import { fromHex, toHex } from '@hashtree/core';
const STORAGE_KEY = 'hashtree:localRootCache';
/**
 * Default localStorage persistence
 */
class LocalStoragePersistence {
    cache = null;
    serializeRecord(record) {
        return {
            hash: toHex(record.hash),
            key: record.key ? toHex(record.key) : undefined,
            visibility: record.visibility,
            labels: record.labels,
            updatedAt: record.updatedAt,
            source: record.source,
            dirty: record.dirty,
            encryptedKey: record.encryptedKey,
            keyId: record.keyId,
            selfEncryptedKey: record.selfEncryptedKey,
            selfEncryptedLinkKey: record.selfEncryptedLinkKey,
        };
    }
    deserializeRecord(data) {
        try {
            return {
                hash: fromHex(data.hash),
                key: data.key ? fromHex(data.key) : undefined,
                visibility: data.visibility,
                labels: uniqueLabels(data.labels),
                updatedAt: data.updatedAt,
                source: data.source,
                dirty: data.dirty,
                encryptedKey: data.encryptedKey,
                keyId: data.keyId,
                selfEncryptedKey: data.selfEncryptedKey,
                selfEncryptedLinkKey: data.selfEncryptedLinkKey,
            };
        }
        catch {
            return null;
        }
    }
    save(key, record) {
        if (typeof window === 'undefined')
            return;
        // Update in-memory cache
        if (!this.cache) {
            this.cache = this.loadAll();
        }
        this.cache.set(key, record);
        // Persist to localStorage
        try {
            const data = {};
            for (const [k, r] of this.cache.entries()) {
                data[k] = this.serializeRecord(r);
            }
            window.localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
        }
        catch {
            // Ignore persistence errors (storage may be full/unavailable)
        }
    }
    load(key) {
        if (!this.cache) {
            this.cache = this.loadAll();
        }
        return this.cache.get(key) ?? null;
    }
    delete(key) {
        if (typeof window === 'undefined')
            return;
        if (!this.cache) {
            this.cache = this.loadAll();
        }
        this.cache.delete(key);
        try {
            const data = {};
            for (const [k, r] of this.cache.entries()) {
                data[k] = this.serializeRecord(r);
            }
            window.localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
        }
        catch {
            // Ignore persistence errors
        }
    }
    loadAll() {
        const result = new Map();
        if (typeof window === 'undefined')
            return result;
        try {
            const raw = window.localStorage.getItem(STORAGE_KEY);
            if (!raw)
                return result;
            const data = JSON.parse(raw);
            for (const [key, persisted] of Object.entries(data)) {
                const record = this.deserializeRecord(persisted);
                if (record) {
                    result.set(key, record);
                }
            }
        }
        catch {
            // Ignore parse errors
        }
        this.cache = result;
        return result;
    }
}
/**
 * TreeRootRegistry - singleton class for managing tree root data
 */
class TreeRootRegistryImpl {
    records = new Map();
    listeners = new Map();
    globalListeners = new Set();
    persistence;
    // Publish throttling
    publishTimers = new Map();
    publishFn = null;
    publishDelay = 1000;
    retryDelay = 5000;
    constructor(persistence) {
        this.persistence = persistence ?? new LocalStoragePersistence();
        this.hydrate();
    }
    /**
     * Hydrate from persistence on startup
     */
    hydrate() {
        const persisted = this.persistence.loadAll();
        for (const [key, record] of persisted.entries()) {
            this.records.set(key, record);
            // Schedule publish for dirty entries
            if (record.dirty) {
                const [npub, ...treeNameParts] = key.split('/');
                const treeName = treeNameParts.join('/');
                if (npub && treeName) {
                    this.schedulePublish(npub, treeName);
                }
            }
        }
    }
    /**
     * Set the publish function (called with throttling for dirty records)
     */
    setPublishFn(fn) {
        this.publishFn = fn;
    }
    /**
     * Build cache key from npub and treeName
     */
    makeKey(npub, treeName) {
        return `${npub}/${treeName}`;
    }
    /**
     * Notify listeners of a record change
     */
    notify(key, record) {
        const keyListeners = this.listeners.get(key);
        if (keyListeners) {
            for (const listener of keyListeners) {
                try {
                    listener(record);
                }
                catch (e) {
                    console.error('[TreeRootRegistry] Listener error:', e);
                }
            }
        }
        // Notify global listeners
        for (const listener of this.globalListeners) {
            try {
                listener(key, record);
            }
            catch (e) {
                console.error('[TreeRootRegistry] Global listener error:', e);
            }
        }
    }
    shouldAcceptUpdate(existing, hash, key, updatedAt) {
        if (!existing)
            return true;
        if (existing.dirty)
            return false;
        if (existing.updatedAt > updatedAt)
            return false;
        if (existing.updatedAt === updatedAt) {
            if (toHex(existing.hash) === toHex(hash)) {
                if (!key)
                    return false;
                if (existing.key && toHex(existing.key) === toHex(key))
                    return false;
            }
        }
        return true;
    }
    mergeSameHashMetadata(existing, hash, options) {
        if (!existing)
            return false;
        if (existing.dirty)
            return false;
        if (toHex(existing.hash) !== toHex(hash))
            return false;
        let changed = false;
        if (!existing.key && options?.key) {
            existing.key = options.key;
            changed = true;
        }
        if (existing.visibility === 'public' && options?.visibility && options.visibility !== 'public') {
            existing.visibility = options.visibility;
            changed = true;
        }
        const mergedLabels = mergeLabels(options?.labels, existing.labels);
        if (mergedLabels && JSON.stringify(mergedLabels) !== JSON.stringify(existing.labels)) {
            existing.labels = mergedLabels;
            changed = true;
        }
        if (!existing.encryptedKey && options?.encryptedKey) {
            existing.encryptedKey = options.encryptedKey;
            changed = true;
        }
        if (!existing.keyId && options?.keyId) {
            existing.keyId = options.keyId;
            changed = true;
        }
        if (!existing.selfEncryptedKey && options?.selfEncryptedKey) {
            existing.selfEncryptedKey = options.selfEncryptedKey;
            changed = true;
        }
        if (!existing.selfEncryptedLinkKey && options?.selfEncryptedLinkKey) {
            existing.selfEncryptedLinkKey = options.selfEncryptedLinkKey;
            changed = true;
        }
        return changed;
    }
    /**
     * Sync lookup - returns cached record or null (no side effects)
     */
    get(npub, treeName) {
        return this.records.get(this.makeKey(npub, treeName)) ?? null;
    }
    /**
     * Get by key directly
     */
    getByKey(key) {
        return this.records.get(key) ?? null;
    }
    /**
     * Async resolve - returns current record if cached, otherwise waits for first resolve
     */
    async resolve(npub, treeName, options) {
        const key = this.makeKey(npub, treeName);
        const existing = this.records.get(key);
        if (existing) {
            return existing;
        }
        const timeoutMs = options?.timeoutMs ?? 10000;
        return new Promise((resolve) => {
            let resolved = false;
            let unsubscribe = null;
            const timeout = setTimeout(() => {
                if (!resolved) {
                    resolved = true;
                    unsubscribe?.();
                    resolve(null);
                }
            }, timeoutMs);
            unsubscribe = this.subscribe(npub, treeName, (record) => {
                if (!resolved && record) {
                    resolved = true;
                    clearTimeout(timeout);
                    unsubscribe?.();
                    resolve(record);
                }
            });
        });
    }
    /**
     * Subscribe to updates for a specific tree
     * Emits current snapshot immediately if available, then future updates
     */
    subscribe(npub, treeName, callback) {
        const key = this.makeKey(npub, treeName);
        let keyListeners = this.listeners.get(key);
        if (!keyListeners) {
            keyListeners = new Set();
            this.listeners.set(key, keyListeners);
        }
        keyListeners.add(callback);
        // Emit current snapshot if available
        const existing = this.records.get(key);
        if (existing) {
            // Use queueMicrotask to ensure callback is async
            queueMicrotask(() => callback(existing));
        }
        return () => {
            const listeners = this.listeners.get(key);
            if (listeners) {
                listeners.delete(callback);
                if (listeners.size === 0) {
                    this.listeners.delete(key);
                }
            }
        };
    }
    /**
     * Subscribe to all registry updates (for bridges like Tauri/worker)
     */
    subscribeAll(callback) {
        this.globalListeners.add(callback);
        return () => {
            this.globalListeners.delete(callback);
        };
    }
    /**
     * Set record from local write - marks dirty and schedules publish
     */
    setLocal(npub, treeName, hash, options) {
        const cacheKey = this.makeKey(npub, treeName);
        const existing = this.records.get(cacheKey);
        // Preserve existing visibility if not provided
        const visibility = options?.visibility ?? existing?.visibility ?? 'public';
        const record = {
            hash,
            key: options?.key,
            visibility,
            labels: uniqueLabels(options?.labels) ?? existing?.labels,
            updatedAt: Math.floor(Date.now() / 1000),
            source: 'local-write',
            dirty: true,
            encryptedKey: options?.encryptedKey ?? existing?.encryptedKey,
            keyId: options?.keyId ?? existing?.keyId,
            selfEncryptedKey: options?.selfEncryptedKey ?? existing?.selfEncryptedKey,
            selfEncryptedLinkKey: options?.selfEncryptedLinkKey ?? existing?.selfEncryptedLinkKey,
        };
        this.records.set(cacheKey, record);
        this.persistence.save(cacheKey, record);
        this.notify(cacheKey, record);
        this.schedulePublish(npub, treeName);
    }
    /**
     * Set record from resolver (Nostr event) - only updates if newer
     */
    setFromResolver(npub, treeName, hash, updatedAt, options) {
        const cacheKey = this.makeKey(npub, treeName);
        const existing = this.records.get(cacheKey);
        const sameHash = !!existing && toHex(existing.hash) === toHex(hash);
        // Only update if newer (based on updatedAt timestamp), or same timestamp with new hash/key
        if (!this.shouldAcceptUpdate(existing ?? undefined, hash, options?.key, updatedAt)) {
            if (this.mergeSameHashMetadata(existing ?? undefined, hash, options)) {
                this.persistence.save(cacheKey, existing);
                this.notify(cacheKey, existing);
                return true;
            }
            return false;
        }
        const record = {
            hash,
            // Preserve known key when newer resolver updates omit it for the same hash.
            key: options?.key ?? (sameHash ? existing?.key : undefined),
            visibility: options?.visibility ?? 'public',
            labels: uniqueLabels(options?.labels) ?? existing?.labels,
            updatedAt,
            source: 'nostr',
            dirty: false,
            encryptedKey: options?.encryptedKey ?? (sameHash ? existing?.encryptedKey : undefined),
            keyId: options?.keyId ?? (sameHash ? existing?.keyId : undefined),
            selfEncryptedKey: options?.selfEncryptedKey ?? (sameHash ? existing?.selfEncryptedKey : undefined),
            selfEncryptedLinkKey: options?.selfEncryptedLinkKey ?? (sameHash ? existing?.selfEncryptedLinkKey : undefined),
        };
        this.records.set(cacheKey, record);
        this.persistence.save(cacheKey, record);
        this.notify(cacheKey, record);
        return true;
    }
    /**
     * Merge a decrypted key into an existing record without changing updatedAt/source.
     * Returns true if the record was updated.
     */
    mergeKey(npub, treeName, hash, key) {
        const cacheKey = this.makeKey(npub, treeName);
        const existing = this.records.get(cacheKey);
        if (!existing)
            return false;
        if (toHex(existing.hash) !== toHex(hash))
            return false;
        if (existing.key)
            return false;
        existing.key = key;
        this.persistence.save(cacheKey, existing);
        this.notify(cacheKey, existing);
        return true;
    }
    /**
     * Set record from worker (Nostr subscription routed through worker)
     * Similar to setFromResolver but source is 'worker'
     */
    setFromWorker(npub, treeName, hash, updatedAt, options) {
        const cacheKey = this.makeKey(npub, treeName);
        const existing = this.records.get(cacheKey);
        const sameHash = !!existing && toHex(existing.hash) === toHex(hash);
        // Only update if newer (based on updatedAt timestamp), or same timestamp with new hash/key
        if (!this.shouldAcceptUpdate(existing ?? undefined, hash, options?.key, updatedAt)) {
            if (this.mergeSameHashMetadata(existing ?? undefined, hash, options)) {
                this.persistence.save(cacheKey, existing);
                this.notify(cacheKey, existing);
                return true;
            }
            return false;
        }
        const record = {
            hash,
            // Preserve known key when worker updates omit it for the same hash.
            key: options?.key ?? (sameHash ? existing?.key : undefined),
            visibility: options?.visibility ?? 'public',
            labels: uniqueLabels(options?.labels) ?? existing?.labels,
            updatedAt,
            source: 'worker',
            dirty: false,
            encryptedKey: options?.encryptedKey ?? (sameHash ? existing?.encryptedKey : undefined),
            keyId: options?.keyId ?? (sameHash ? existing?.keyId : undefined),
            selfEncryptedKey: options?.selfEncryptedKey ?? (sameHash ? existing?.selfEncryptedKey : undefined),
            selfEncryptedLinkKey: options?.selfEncryptedLinkKey ?? (sameHash ? existing?.selfEncryptedLinkKey : undefined),
        };
        this.records.set(cacheKey, record);
        this.persistence.save(cacheKey, record);
        this.notify(cacheKey, record);
        return true;
    }
    /**
     * Set record from external source (Tauri, worker, prefetch)
     */
    setFromExternal(npub, treeName, hash, source, options) {
        const cacheKey = this.makeKey(npub, treeName);
        const existing = this.records.get(cacheKey);
        const sameHash = !!existing && toHex(existing.hash) === toHex(hash);
        // Don't overwrite dirty local writes
        if (existing?.dirty) {
            return;
        }
        const updatedAt = options?.updatedAt ?? Math.floor(Date.now() / 1000);
        // Only update if newer (based on updatedAt timestamp), or same timestamp with new hash/key
        if (!this.shouldAcceptUpdate(existing ?? undefined, hash, options?.key, updatedAt)) {
            if (this.mergeSameHashMetadata(existing ?? undefined, hash, options)) {
                this.persistence.save(cacheKey, existing);
                this.notify(cacheKey, existing);
            }
            return;
        }
        const record = {
            hash,
            key: options?.key ?? (sameHash ? existing?.key : undefined),
            visibility: options?.visibility ?? existing?.visibility ?? 'public',
            labels: uniqueLabels(options?.labels) ?? existing?.labels,
            updatedAt,
            source,
            dirty: false,
            encryptedKey: options?.encryptedKey ?? (sameHash ? existing?.encryptedKey : undefined),
            keyId: options?.keyId ?? (sameHash ? existing?.keyId : undefined),
            selfEncryptedKey: options?.selfEncryptedKey ?? (sameHash ? existing?.selfEncryptedKey : undefined),
            selfEncryptedLinkKey: options?.selfEncryptedLinkKey ?? (sameHash ? existing?.selfEncryptedLinkKey : undefined),
        };
        this.records.set(cacheKey, record);
        this.persistence.save(cacheKey, record);
        this.notify(cacheKey, record);
    }
    /**
     * Delete a record
     */
    delete(npub, treeName) {
        const key = this.makeKey(npub, treeName);
        // Cancel any pending publish
        const timer = this.publishTimers.get(key);
        if (timer) {
            clearTimeout(timer);
            this.publishTimers.delete(key);
        }
        this.records.delete(key);
        this.persistence.delete(key);
        this.notify(key, null);
    }
    /**
     * Schedule a throttled publish
     */
    schedulePublish(npub, treeName, delay = this.publishDelay) {
        const key = this.makeKey(npub, treeName);
        // Clear existing timer
        const existingTimer = this.publishTimers.get(key);
        if (existingTimer) {
            clearTimeout(existingTimer);
        }
        // Schedule new publish
        const timer = setTimeout(() => {
            this.publishTimers.delete(key);
            this.doPublish(npub, treeName);
        }, delay);
        this.publishTimers.set(key, timer);
    }
    /**
     * Execute the publish
     */
    async doPublish(npub, treeName) {
        const key = this.makeKey(npub, treeName);
        const record = this.records.get(key);
        if (!record || !record.dirty || !this.publishFn) {
            return;
        }
        try {
            const success = await this.publishFn(npub, treeName, record);
            if (success) {
                // Mark as clean (published)
                // Re-check record in case it changed during async publish
                const currentRecord = this.records.get(key);
                if (currentRecord && toHex(currentRecord.hash) === toHex(record.hash)) {
                    currentRecord.dirty = false;
                    this.persistence.save(key, currentRecord);
                }
            }
            else if (!this.publishTimers.has(key)) {
                this.schedulePublish(npub, treeName, this.retryDelay);
            }
        }
        catch (e) {
            console.error('[TreeRootRegistry] Publish failed:', e);
            if (!this.publishTimers.has(key)) {
                this.schedulePublish(npub, treeName, this.retryDelay);
            }
        }
    }
    /**
     * Force immediate publish of all dirty records
     */
    async flushPendingPublishes() {
        if (!this.publishFn) {
            console.warn('[TreeRootRegistry] flushPendingPublishes: publishFn not set');
            return;
        }
        const promises = [];
        for (const [key, timer] of this.publishTimers) {
            clearTimeout(timer);
            this.publishTimers.delete(key);
            const [npub, ...treeNameParts] = key.split('/');
            const treeName = treeNameParts.join('/');
            if (npub && treeName) {
                promises.push(this.doPublish(npub, treeName));
            }
        }
        await Promise.all(promises);
    }
    /**
     * Cancel pending publish (call before delete to prevent "undelete")
     */
    cancelPendingPublish(npub, treeName) {
        const key = this.makeKey(npub, treeName);
        const timer = this.publishTimers.get(key);
        if (timer) {
            clearTimeout(timer);
            this.publishTimers.delete(key);
        }
    }
    /**
     * Get all records (for debugging/migration)
     */
    getAllRecords() {
        return new Map(this.records);
    }
    /**
     * Check if a record exists
     */
    has(npub, treeName) {
        return this.records.has(this.makeKey(npub, treeName));
    }
    /**
     * Get visibility for a tree
     */
    getVisibility(npub, treeName) {
        return this.records.get(this.makeKey(npub, treeName))?.visibility;
    }
    /**
     * Get labels for a tree
     */
    getLabels(npub, treeName) {
        return this.records.get(this.makeKey(npub, treeName))?.labels;
    }
}
function getRegistry() {
    if (typeof window !== 'undefined' && window.__treeRootRegistry) {
        return window.__treeRootRegistry;
    }
    const registry = new TreeRootRegistryImpl();
    if (typeof window !== 'undefined') {
        window.__treeRootRegistry = registry;
    }
    return registry;
}
function uniqueLabels(labels) {
    if (!labels?.length)
        return undefined;
    const deduped = [];
    const seen = new Set();
    for (const label of labels) {
        if (!label || seen.has(label))
            continue;
        seen.add(label);
        deduped.push(label);
    }
    return deduped.length > 0 ? deduped : undefined;
}
function mergeLabels(primary, fallback) {
    if (!primary?.length)
        return uniqueLabels(fallback);
    if (!fallback?.length)
        return uniqueLabels(primary);
    return uniqueLabels([...primary, ...fallback]);
}
// Export singleton instance
export const treeRootRegistry = getRegistry();
//# sourceMappingURL=tree-root.js.map