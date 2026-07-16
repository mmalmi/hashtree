// @ts-nocheck
/**
 * Tree Root Cache
 *
 * Persists npub/treeName → CID mappings using any Store implementation.
 * This allows quick resolution of tree roots without waiting for Nostr.
 *
 * Storage format:
 * - Key prefix: "root:" (to distinguish from content chunks)
 * - Key: SHA256("root:" + npub + "/" + treeName)
 * - Value: MessagePack { hash, key?, visibility, updatedAt }
 */
import { sha256 } from '@hashtree/core';
import { encode, decode } from '@msgpack/msgpack';
import { LRUCache } from './utils/lruCache';
// In-memory LRU cache for fast lookups (limited to 1000 entries to prevent memory leak)
// Data is backed by persistent store so eviction is safe
const memoryCache = new LRUCache(1000);
const updateListeners = new Set();
// Store reference
let store = null;
function compareReplaceableEventOrder(candidateUpdatedAt, candidateEventId, currentUpdatedAt, currentEventId) {
    if (candidateUpdatedAt !== currentUpdatedAt) {
        return candidateUpdatedAt - currentUpdatedAt;
    }
    const candidateId = candidateEventId ?? '';
    const currentId = currentEventId ?? '';
    if (candidateId === currentId) {
        return 0;
    }
    if (!candidateId)
        return -1;
    if (!currentId)
        return 1;
    return currentId.localeCompare(candidateId);
}
function notifyUpdate(npub, treeName, cid) {
    for (const listener of updateListeners) {
        listener(npub, treeName, cid);
    }
}
/**
 * Initialize the cache with a store
 */
export function initTreeRootCache(storeImpl) {
    store = storeImpl;
}
export function getTreeRootCacheStore() {
    return store;
}
/**
 * Generate storage key for a tree root
 */
async function makeStorageKey(npub, treeName) {
    const keyStr = `root:${npub}/${treeName}`;
    return sha256(new TextEncoder().encode(keyStr));
}
/**
 * Get a cached tree root
 */
export async function getCachedRoot(npub, treeName) {
    const cacheKey = `${npub}/${treeName}`;
    // Check memory cache first
    const memCached = memoryCache.get(cacheKey);
    if (memCached) {
        return { hash: memCached.hash, key: memCached.key };
    }
    // Check persistent store
    if (!store)
        return null;
    const storageKey = await makeStorageKey(npub, treeName);
    const data = await store.get(storageKey);
    if (!data)
        return null;
    try {
        const cached = decode(data);
        // Update memory cache
        memoryCache.set(cacheKey, cached);
        return { hash: cached.hash, key: cached.key };
    }
    catch {
        return null;
    }
}
/**
 * Get full cached root info (including visibility)
 */
export async function getCachedRootInfo(npub, treeName) {
    const cacheKey = `${npub}/${treeName}`;
    // Check memory cache first
    const memCached = memoryCache.get(cacheKey);
    if (memCached)
        return memCached;
    // Check persistent store
    if (!store)
        return null;
    const storageKey = await makeStorageKey(npub, treeName);
    const data = await store.get(storageKey);
    if (!data)
        return null;
    try {
        const cached = decode(data);
        memoryCache.set(cacheKey, cached);
        return cached;
    }
    catch {
        return null;
    }
}
/**
 * Cache a tree root
 */
export async function setCachedRoot(npub, treeName, cid, visibility = 'public', options) {
    const cacheKey = `${npub}/${treeName}`;
    const existing = await getCachedRootInfo(npub, treeName);
    const updatedAt = options?.updatedAt ?? Math.floor(Date.now() / 1000);
    const eventId = options?.eventId;
    const sameHash = !!existing && hashEquals(existing.hash, cid.hash);
    const preserveKey = sameHash
        && visibility !== 'public'
        && cid.key === undefined
        && existing?.key !== undefined;
    const preserveLinkVisibleMetadata = sameHash
        && visibility === 'link-visible'
        && options?.encryptedKey === undefined
        && options?.keyId === undefined
        && options?.selfEncryptedLinkKey === undefined;
    const preservePrivateMetadata = sameHash
        && visibility === 'private'
        && options?.selfEncryptedKey === undefined;
    if (!options?.force
        && existing
        && compareReplaceableEventOrder(updatedAt, eventId, existing.updatedAt, existing.eventId) < 0) {
        return { applied: false, record: existing };
    }
    const cached = {
        hash: cid.hash,
        key: cid.key ?? (preserveKey ? existing?.key : undefined),
        visibility,
        labels: options?.labels ?? existing?.labels,
        updatedAt,
        eventId: eventId ?? (sameHash ? existing?.eventId : undefined),
        snapshotNhash: options?.snapshotNhash ?? (sameHash ? existing?.snapshotNhash : undefined),
        encryptedKey: options?.encryptedKey ?? (preserveLinkVisibleMetadata ? existing?.encryptedKey : undefined),
        keyId: options?.keyId ?? (preserveLinkVisibleMetadata ? existing?.keyId : undefined),
        selfEncryptedKey: options?.selfEncryptedKey ?? (preservePrivateMetadata ? existing?.selfEncryptedKey : undefined),
        selfEncryptedLinkKey: options?.selfEncryptedLinkKey
            ?? (preserveLinkVisibleMetadata ? existing?.selfEncryptedLinkKey : undefined),
    };
    if (existing && cachedRootEquals(existing, cached)) {
        return { applied: false, record: existing };
    }
    // Update memory cache
    memoryCache.set(cacheKey, cached);
    notifyUpdate(npub, treeName, { hash: cached.hash, key: cached.key });
    // Persist to store
    if (store) {
        const storageKey = await makeStorageKey(npub, treeName);
        const data = encode(cached);
        await store.put(storageKey, new Uint8Array(data));
    }
    return { applied: true, record: cached };
}
/**
 * Merge a decrypted key into an existing cache entry (if hash matches).
 */
export async function mergeCachedRootKey(npub, treeName, hash, key) {
    const cacheKey = `${npub}/${treeName}`;
    const cached = await getCachedRootInfo(npub, treeName);
    if (!cached)
        return false;
    if (cached.key)
        return false;
    if (!hashEquals(cached.hash, hash))
        return false;
    const merged = {
        ...cached,
        key,
    };
    memoryCache.set(cacheKey, merged);
    notifyUpdate(npub, treeName, { hash: merged.hash, key: merged.key });
    if (store) {
        const storageKey = await makeStorageKey(npub, treeName);
        const data = encode(merged);
        await store.put(storageKey, new Uint8Array(data));
    }
    return true;
}
/**
 * Remove a cached tree root
 */
export async function removeCachedRoot(npub, treeName) {
    const cacheKey = `${npub}/${treeName}`;
    // Remove from memory cache
    memoryCache.delete(cacheKey);
    notifyUpdate(npub, treeName, null);
    // Remove from persistent store
    if (store) {
        const storageKey = await makeStorageKey(npub, treeName);
        await store.delete(storageKey);
    }
}
/**
 * List all cached roots for an npub
 * Note: This scans memory cache only - persistent lookup requires iteration
 */
export function listCachedRoots(npub) {
    const prefix = `${npub}/`;
    const results = [];
    for (const [key, cached] of memoryCache) {
        if (key.startsWith(prefix)) {
            const treeName = key.slice(prefix.length);
            results.push({
                treeName,
                cid: { hash: cached.hash, key: cached.key },
                visibility: cached.visibility,
                updatedAt: cached.updatedAt,
            });
        }
    }
    return results;
}
/**
 * Clear all cached roots (memory only)
 */
export function clearMemoryCache() {
    memoryCache.clear();
}
export function onCachedRootUpdate(listener) {
    updateListeners.add(listener);
    return () => {
        updateListeners.delete(listener);
    };
}
/**
 * Get cache stats
 */
export function getCacheStats() {
    return {
        memoryEntries: memoryCache.size,
    };
}
function hashEquals(a, b) {
    if (a.length !== b.length)
        return false;
    for (let i = 0; i < a.length; i += 1) {
        if (a[i] !== b[i])
            return false;
    }
    return true;
}
function optionalHashEquals(a, b) {
    if (!a && !b)
        return true;
    if (!a || !b)
        return false;
    return hashEquals(a, b);
}
function labelsEqual(a, b) {
    if (!a && !b)
        return true;
    if (!a || !b)
        return false;
    if (a.length !== b.length)
        return false;
    for (let i = 0; i < a.length; i += 1) {
        if (a[i] !== b[i])
            return false;
    }
    return true;
}
function cachedRootEquals(a, b) {
    return (hashEquals(a.hash, b.hash) &&
        optionalHashEquals(a.key, b.key) &&
        a.visibility === b.visibility &&
        labelsEqual(a.labels, b.labels) &&
        a.updatedAt === b.updatedAt &&
        a.snapshotNhash === b.snapshotNhash &&
        a.encryptedKey === b.encryptedKey &&
        a.keyId === b.keyId &&
        a.selfEncryptedKey === b.selfEncryptedKey &&
        a.selfEncryptedLinkKey === b.selfEncryptedLinkKey);
}
//# sourceMappingURL=treeRootCache.js.map