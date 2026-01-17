/**
 * Local cache for tracking the most recent root hash for each tree
 *
 * This is the SINGLE SOURCE OF TRUTH for the current merkle root.
 * All writes go here immediately, publishing to Nostr is throttled.
 *
 * Key: "npub/treeName", Value: { hash, key, visibility, dirty }
 */
import type { Hash, TreeVisibility } from 'hashtree';
import { fromHex, toHex } from 'hashtree';
import { updateSubscriptionCache } from './stores/treeRoot';

interface CacheEntry {
  hash: Hash;
  key?: Hash;
  visibility?: TreeVisibility;
  dirty: boolean; // true if not yet published to Nostr
}

const localRootCache = new Map<string, CacheEntry>();

interface PersistedEntry {
  hash: string;
  key?: string;
  visibility?: TreeVisibility;
  dirty?: boolean;
}

const STORAGE_KEY = 'hashtree:localRootCache';

// Throttle timers per tree
const publishTimers = new Map<string, ReturnType<typeof setTimeout>>();

// Publish delay in ms (throttle)
const PUBLISH_DELAY = 1000;
const RETRY_DELAY = 5000;

// Listeners for cache updates
const listeners = new Set<(npub: string, treeName: string) => void>();

/**
 * Subscribe to cache updates
 */
export function onCacheUpdate(listener: (npub: string, treeName: string) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function notifyListeners(npub: string, treeName: string) {
  for (const listener of listeners) {
    try {
      listener(npub, treeName);
    } catch (e) {
      console.error('Cache listener error:', e);
    }
  }
}

/**
 * Update the local root cache after a write operation.
 * This should be the ONLY place that tracks merkle root changes.
 * Publishing to Nostr is throttled - multiple rapid updates result in one publish.
 */
export function updateLocalRootCache(npub: string, treeName: string, hash: Hash, key?: Hash, visibility?: TreeVisibility) {
  const cacheKey = `${npub}/${treeName}`;
  // Preserve existing visibility if not provided (for incremental updates that don't change visibility)
  const existing = localRootCache.get(cacheKey);
  const finalVisibility = visibility ?? existing?.visibility;
  localRootCache.set(cacheKey, { hash, key, visibility: finalVisibility, dirty: true });
  notifyListeners(npub, treeName);
  schedulePublish(npub, treeName);
  persistCache();

  // Update subscription cache to trigger immediate UI update
  updateSubscriptionCache(cacheKey, hash, key);
}

/**
 * Get the visibility for a cached tree
 */
export function getCachedVisibility(npub: string, treeName: string): TreeVisibility | undefined {
  return localRootCache.get(`${npub}/${treeName}`)?.visibility;
}

/**
 * Update the local root cache (hex version)
 */
export function updateLocalRootCacheHex(npub: string, treeName: string, hashHex: string, keyHex?: string, visibility?: TreeVisibility) {
  updateLocalRootCache(
    npub,
    treeName,
    fromHex(hashHex),
    keyHex ? fromHex(keyHex) : undefined,
    visibility
  );
}

/**
 * Get cached root hash for a tree (if available)
 */
export function getLocalRootCache(npub: string, treeName: string): Hash | undefined {
  return localRootCache.get(`${npub}/${treeName}`)?.hash;
}

/**
 * Get cached root key for a tree (if available)
 */
export function getLocalRootKey(npub: string, treeName: string): Hash | undefined {
  return localRootCache.get(`${npub}/${treeName}`)?.key;
}

/**
 * Get all entries from the local root cache
 */
export function getAllLocalRoots(): Map<string, { hash: Hash; key?: Hash; visibility?: TreeVisibility }> {
  const result = new Map<string, { hash: Hash; key?: Hash; visibility?: TreeVisibility }>();
  for (const [key, entry] of localRootCache.entries()) {
    result.set(key, { hash: entry.hash, key: entry.key, visibility: entry.visibility });
  }
  return result;
}

/**
 * Get full cache entry
 */
export function getLocalRootEntry(npub: string, treeName: string): CacheEntry | undefined {
  return localRootCache.get(`${npub}/${treeName}`);
}

/**
 * Schedule a throttled publish to Nostr
 */
function schedulePublish(npub: string, treeName: string, delay: number = PUBLISH_DELAY) {
  const cacheKey = `${npub}/${treeName}`;

  // Clear existing timer
  const existingTimer = publishTimers.get(cacheKey);
  if (existingTimer) {
    clearTimeout(existingTimer);
  }

  // Schedule new publish
  const timer = setTimeout(() => {
    publishTimers.delete(cacheKey);
    doPublish(npub, treeName);
  }, delay);

  publishTimers.set(cacheKey, timer);
}

/**
 * Cancel any pending publish for a tree (call before delete)
 * This prevents the throttled publish from "undeleting" the tree
 */
export function cancelPendingPublish(npub: string, treeName: string): void {
  const cacheKey = `${npub}/${treeName}`;
  const timer = publishTimers.get(cacheKey);
  if (timer) {
    clearTimeout(timer);
    publishTimers.delete(cacheKey);
  }
  // Also remove from cache to prevent any future publish
  localRootCache.delete(cacheKey);
  persistCache();
}

/**
 * Actually publish to Nostr (called after throttle delay)
 * Also pushes blob data to Blossom servers
 */
async function doPublish(npub: string, treeName: string) {
  const cacheKey = `${npub}/${treeName}`;
  const entry = localRootCache.get(cacheKey);
  if (!entry || !entry.dirty) return;

  try {
    // Dynamic import to avoid circular dependency
    const { publishTreeRoot } = await import('./nostr');
    const { cid } = await import('hashtree');

    // Use cached visibility to ensure correct tags are published even after navigation
    const visibility = entry.visibility;
    const rootCid = cid(entry.hash, entry.key);

    const success = await publishTreeRoot(treeName, rootCid, visibility);

    if (success) {
      // Mark as clean (published)
      // Re-check entry in case it changed during async publish
      const currentEntry = localRootCache.get(cacheKey);
      if (currentEntry && toHex(currentEntry.hash) === toHex(entry.hash)) {
        currentEntry.dirty = false;
        persistCache();
      }
    } else if (!publishTimers.has(cacheKey)) {
      schedulePublish(npub, treeName, RETRY_DELAY);
    }
  } catch (e) {
    console.error('Failed to publish tree root:', e);
    if (!publishTimers.has(cacheKey)) {
      schedulePublish(npub, treeName, RETRY_DELAY);
    }
  }
}

/**
 * Force immediate publish (for critical operations like logout)
 */
export async function flushPendingPublishes(): Promise<void> {
  if (import.meta.env.VITE_TEST_MODE) {
    try {
      const { waitForRelayConnection } = await import('./lib/workerInit');
      await waitForRelayConnection(3000);
    } catch {
      // Ignore relay wait failures in test mode
    }
  }
  const promises: Promise<void>[] = [];

  for (const [cacheKey, timer] of publishTimers) {
    clearTimeout(timer);
    publishTimers.delete(cacheKey);

    const [npub, treeName] = cacheKey.split('/');
    promises.push(doPublish(npub, treeName));
  }

  await Promise.all(promises);
}

function persistCache(): void {
  if (typeof window === 'undefined') return;
  try {
    const data: Record<string, PersistedEntry> = {};
    for (const [cacheKey, entry] of localRootCache.entries()) {
      data[cacheKey] = {
        hash: toHex(entry.hash),
        key: entry.key ? toHex(entry.key) : undefined,
        visibility: entry.visibility,
        dirty: entry.dirty,
      };
    }
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
  } catch {
    // Ignore persistence errors (storage may be unavailable)
  }
}

function hydrateCache(): void {
  if (typeof window === 'undefined') return;
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return;
    const data = JSON.parse(raw) as Record<string, PersistedEntry>;
    for (const [cacheKey, entry] of Object.entries(data)) {
      if (!entry?.hash) continue;
      try {
        const hash = fromHex(entry.hash);
        const key = entry.key ? fromHex(entry.key) : undefined;
        const visibility = entry.visibility;
        const dirty = entry.dirty ?? false;
        localRootCache.set(cacheKey, { hash, key, visibility, dirty });
        updateSubscriptionCache(cacheKey, hash, key);

        if (dirty) {
          const [npub, ...treeNameParts] = cacheKey.split('/');
          const treeName = treeNameParts.join('/');
          if (npub && treeName) {
            schedulePublish(npub, treeName);
          }
        }
      } catch {
        // Skip malformed entries
      }
    }
  } catch {
    // Ignore hydration errors
  }
}

hydrateCache();
