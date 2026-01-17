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

import type { CID, Store } from '../../../../ts/packages/hashtree/src/types';
import { sha256 } from '../../../../ts/packages/hashtree/src/hash';
import { encode, decode } from '@msgpack/msgpack';
import type { TreeVisibility } from '../../../../ts/packages/hashtree/src/visibility';
import { LRUCache } from '../utils/lruCache';

// Cached root entry
interface CachedRoot {
  hash: Uint8Array;        // Root hash
  key?: Uint8Array;        // CHK decryption key (for encrypted trees)
  visibility: TreeVisibility;
  updatedAt: number;       // Unix timestamp
  encryptedKey?: string;   // For link-visible trees
  keyId?: string;          // For link-visible trees
  selfEncryptedKey?: string; // For private trees
}

// In-memory LRU cache for fast lookups (limited to 1000 entries to prevent memory leak)
// Data is backed by persistent store so eviction is safe
const memoryCache = new LRUCache<string, CachedRoot>(1000);

// Store reference
let store: Store | null = null;

/**
 * Initialize the cache with a store
 */
export function initTreeRootCache(storeImpl: Store): void {
  store = storeImpl;
}

/**
 * Generate storage key for a tree root
 */
async function makeStorageKey(npub: string, treeName: string): Promise<Uint8Array> {
  const keyStr = `root:${npub}/${treeName}`;
  return sha256(new TextEncoder().encode(keyStr));
}

/**
 * Get a cached tree root
 */
export async function getCachedRoot(npub: string, treeName: string): Promise<CID | null> {
  const cacheKey = `${npub}/${treeName}`;

  // Check memory cache first
  const memCached = memoryCache.get(cacheKey);
  if (memCached) {
    return { hash: memCached.hash, key: memCached.key };
  }

  // Check persistent store
  if (!store) return null;

  const storageKey = await makeStorageKey(npub, treeName);
  const data = await store.get(storageKey);
  if (!data) return null;

  try {
    const cached = decode(data) as CachedRoot;
    // Update memory cache
    memoryCache.set(cacheKey, cached);
    return { hash: cached.hash, key: cached.key };
  } catch {
    return null;
  }
}

/**
 * Get full cached root info (including visibility)
 */
export async function getCachedRootInfo(npub: string, treeName: string): Promise<CachedRoot | null> {
  const cacheKey = `${npub}/${treeName}`;

  // Check memory cache first
  const memCached = memoryCache.get(cacheKey);
  if (memCached) return memCached;

  // Check persistent store
  if (!store) return null;

  const storageKey = await makeStorageKey(npub, treeName);
  const data = await store.get(storageKey);
  if (!data) return null;

  try {
    const cached = decode(data) as CachedRoot;
    memoryCache.set(cacheKey, cached);
    return cached;
  } catch {
    return null;
  }
}

/**
 * Cache a tree root
 */
export async function setCachedRoot(
  npub: string,
  treeName: string,
  cid: CID,
  visibility: TreeVisibility = 'public',
  options?: {
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
  }
): Promise<void> {
  const cacheKey = `${npub}/${treeName}`;
  const now = Math.floor(Date.now() / 1000);

  const cached: CachedRoot = {
    hash: cid.hash,
    key: cid.key,
    visibility,
    updatedAt: now,
    encryptedKey: options?.encryptedKey,
    keyId: options?.keyId,
    selfEncryptedKey: options?.selfEncryptedKey,
  };

  // Update memory cache
  memoryCache.set(cacheKey, cached);

  // Persist to store
  if (store) {
    const storageKey = await makeStorageKey(npub, treeName);
    const data = encode(cached);
    await store.put(storageKey, new Uint8Array(data));
  }
}

/**
 * Remove a cached tree root
 */
export async function removeCachedRoot(npub: string, treeName: string): Promise<void> {
  const cacheKey = `${npub}/${treeName}`;

  // Remove from memory cache
  memoryCache.delete(cacheKey);

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
export function listCachedRoots(npub: string): Array<{
  treeName: string;
  cid: CID;
  visibility: TreeVisibility;
  updatedAt: number;
}> {
  const prefix = `${npub}/`;
  const results: Array<{
    treeName: string;
    cid: CID;
    visibility: TreeVisibility;
    updatedAt: number;
  }> = [];

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
export function clearMemoryCache(): void {
  memoryCache.clear();
}

/**
 * Get cache stats
 */
export function getCacheStats(): { memoryEntries: number } {
  return {
    memoryEntries: memoryCache.size,
  };
}
