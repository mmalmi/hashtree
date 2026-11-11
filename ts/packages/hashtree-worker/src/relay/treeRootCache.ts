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

import type { CID, Store, TreeVisibility } from '@hashtree/core';
import { sha256 } from '@hashtree/core';
import { encode, decode } from '@msgpack/msgpack';
import { LRUCache } from './utils/lruCache';
import { RootStoreWriter, type StoreEntry } from './rootStoreWriter';

// Cached root entry
interface CachedRoot {
  hash: Uint8Array;        // Root hash
  key?: Uint8Array;        // CHK decryption key (for encrypted trees)
  visibility: TreeVisibility;
  labels?: string[];
  updatedAt: number;       // Unix timestamp
  eventId?: string;        // Source event id for same-second tie-breaking
  snapshotNhash?: string;  // Signed root-event snapshot permalink
  encryptedKey?: string;   // For link-visible trees
  keyId?: string;          // For link-visible trees
  selfEncryptedKey?: string; // For private trees
  selfEncryptedLinkKey?: string; // For link-visible trees
}

export interface SetCachedRootResult {
  applied: boolean;
  record: CachedRoot;
}

// In-memory LRU cache for fast lookups (limited to 1000 entries to prevent memory leak)
// Data is backed by persistent store so eviction is safe
const memoryCache = new LRUCache<string, CachedRoot>(1000);
const updateListeners = new Set<(npub: string, treeName: string, cid: CID | null) => void>();

// Store reference
let store: Store | null = null;
// Persists staged roots behind the in-memory cache, in grouped writes.
let writer = createWriter(null);
// Storage keys are a hash of the cache key; keep them for hot trees.
const storageKeys = new LRUCache<string, Uint8Array>(2000);
// Serializes read-compare-update per tree so the newest root always wins.
const rootLocks = new Map<string, Promise<void>>();
let removals = 0;

function createWriter(target: Store | null): RootStoreWriter<CachedRoot> {
  return new RootStoreWriter<CachedRoot>(
    target,
    (cacheKey) => storageKeyFor(cacheKey),
    (record) => new Uint8Array(encode(record)),
  );
}

function withRootLock<T>(cacheKey: string, task: () => Promise<T>): Promise<T> {
  const run = (rootLocks.get(cacheKey) ?? Promise.resolve()).then(task);
  const tail = run.then(() => undefined, () => undefined);
  rootLocks.set(cacheKey, tail);
  void tail.then(() => {
    if (rootLocks.get(cacheKey) === tail) rootLocks.delete(cacheKey);
  });
  return run;
}

/**
 * Order two versions of a replaceable root: positive when the candidate is
 * newer than the current one, negative when older, zero when they tie.
 */
export function compareReplaceableEventOrder(
  candidateUpdatedAt: number,
  candidateEventId: string | null | undefined,
  currentUpdatedAt: number,
  currentEventId: string | null | undefined,
): number {
  if (candidateUpdatedAt !== currentUpdatedAt) {
    return candidateUpdatedAt - currentUpdatedAt;
  }

  const candidateId = candidateEventId ?? '';
  const currentId = currentEventId ?? '';
  if (candidateId === currentId) {
    return 0;
  }

  if (!candidateId) return -1;
  if (!currentId) return 1;
  return currentId.localeCompare(candidateId);
}

function notifyUpdate(npub: string, treeName: string, cid: CID | null): void {
  for (const listener of updateListeners) {
    listener(npub, treeName, cid);
  }
}

/**
 * Initialize the cache with a store
 */
export function initTreeRootCache(storeImpl: Store): void {
  store = storeImpl;
  writer = createWriter(storeImpl);
}

export function getTreeRootCacheStore(): Store | null {
  return store;
}

/**
 * Generate storage key for a tree root
 */
async function storageKeyFor(cacheKey: string): Promise<Uint8Array> {
  const known = storageKeys.get(cacheKey);
  if (known) return known;
  const storageKey = await sha256(new TextEncoder().encode(`root:${cacheKey}`));
  storageKeys.set(cacheKey, storageKey);
  return storageKey;
}

/**
 * The newest root known without reading the store: the in-memory entry, or a
 * staged value that has not been written yet (`null` for a pending removal).
 * `undefined` means only the store can answer.
 */
function peekCachedRoot(cacheKey: string): CachedRoot | null | undefined {
  const memCached = memoryCache.get(cacheKey);
  if (memCached) return memCached;
  const pending = writer.pending(cacheKey);
  if (pending) memoryCache.set(cacheKey, pending);
  return pending;
}

async function loadCachedRoot(cacheKey: string): Promise<CachedRoot | null> {
  for (;;) {
    const known = peekCachedRoot(cacheKey);
    if (known !== undefined) return known;
    if (!store) return null;

    const removalsBefore = removals;
    const data = await store.get(await storageKeyFor(cacheKey));

    // A newer root may have been staged, or this one removed, during the read.
    const current = peekCachedRoot(cacheKey);
    if (current !== undefined) return current;
    if (removals !== removalsBefore) continue;
    if (!data) return null;

    try {
      const cached = decode(data) as CachedRoot;
      memoryCache.set(cacheKey, cached);
      return cached;
    } catch {
      return null;
    }
  }
}

/**
 * Get a cached tree root
 */
export async function getCachedRoot(npub: string, treeName: string): Promise<CID | null> {
  const cached = await loadCachedRoot(`${npub}/${treeName}`);
  return cached ? { hash: cached.hash, key: cached.key } : null;
}

/**
 * Get full cached root info (including visibility)
 */
export function getCachedRootInfo(npub: string, treeName: string): Promise<CachedRoot | null> {
  return loadCachedRoot(`${npub}/${treeName}`);
}

/**
 * Cached root info when it is known without reading the store; `undefined`
 * when the store would have to be read.
 */
export function peekCachedRootInfo(npub: string, treeName: string): CachedRoot | null | undefined {
  return peekCachedRoot(`${npub}/${treeName}`);
}

/**
 * Whether a root event carries nothing new for a cached record: it is older
 * than the record, or it is the event the record was built from.
 */
export function isRedundantRootEvent(
  record: CachedRoot,
  updatedAt: number,
  eventId: string | undefined,
): boolean {
  const order = compareReplaceableEventOrder(updatedAt, eventId, record.updatedAt, record.eventId);
  if (order < 0) return true;
  return order === 0 && !!eventId && record.eventId === eventId && record.snapshotNhash !== undefined;
}

/**
 * Resolves once every root staged so far has been written to the store.
 */
export function flushCachedRoots(): Promise<void> {
  return writer.flush();
}

interface SetCachedRootOptions {
  updatedAt?: number;
  eventId?: string;
  labels?: string[];
  snapshotNhash?: string;
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
  force?: boolean;
  /** Blocks backing `snapshotNhash`, written together with the root record. */
  snapshotBlobs?: readonly StoreEntry[];
}

/**
 * Cache a tree root and wait until it is persisted
 */
export async function setCachedRoot(
  npub: string,
  treeName: string,
  cid: CID,
  visibility: TreeVisibility = 'public',
  options?: SetCachedRootOptions
): Promise<SetCachedRootResult> {
  const result = await stageCachedRoot(npub, treeName, cid, visibility, options);
  if (result.applied) await writer.flush();
  return result;
}

/**
 * Apply a tree root to the in-memory cache and queue its store write.
 *
 * The cache answers with the new root as soon as this resolves; persistence
 * follows in a grouped write (see `flushCachedRoots`).
 */
export function stageCachedRoot(
  npub: string,
  treeName: string,
  cid: CID,
  visibility: TreeVisibility = 'public',
  options?: SetCachedRootOptions
): Promise<SetCachedRootResult> {
  const cacheKey = `${npub}/${treeName}`;
  return withRootLock(cacheKey, async () => {
    const existing = await loadCachedRoot(cacheKey);
    return applyCachedRoot(npub, treeName, cid, visibility, options, existing);
  });
}

function applyCachedRoot(
  npub: string,
  treeName: string,
  cid: CID,
  visibility: TreeVisibility,
  options: SetCachedRootOptions | undefined,
  existing: CachedRoot | null,
): SetCachedRootResult {
  const cacheKey = `${npub}/${treeName}`;
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

  if (
    !options?.force
    && existing
    && compareReplaceableEventOrder(updatedAt, eventId, existing.updatedAt, existing.eventId) < 0
  ) {
    return { applied: false, record: existing };
  }

  const cached: CachedRoot = {
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
  writer.queueRecord(cacheKey, cached, options?.snapshotBlobs);

  return { applied: true, record: cached };
}

/**
 * Merge a decrypted key into an existing cache entry (if hash matches).
 */
export async function mergeCachedRootKey(
  npub: string,
  treeName: string,
  hash: Uint8Array,
  key: Uint8Array
): Promise<boolean> {
  const cacheKey = `${npub}/${treeName}`;

  const merged = await withRootLock(cacheKey, async () => {
    const cached = await loadCachedRoot(cacheKey);
    if (!cached) return false;
    if (cached.key) return false;
    if (!hashEquals(cached.hash, hash)) return false;

    const next: CachedRoot = {
      ...cached,
      key,
    };

    memoryCache.set(cacheKey, next);
    notifyUpdate(npub, treeName, { hash: next.hash, key: next.key });
    writer.queueRecord(cacheKey, next);
    return true;
  });

  if (merged) await writer.flush();
  return merged;
}

/**
 * Remove a cached tree root
 */
export async function removeCachedRoot(npub: string, treeName: string): Promise<void> {
  const cacheKey = `${npub}/${treeName}`;

  await withRootLock(cacheKey, async () => {
    removals++;
    // Remove from memory cache
    memoryCache.delete(cacheKey);
    notifyUpdate(npub, treeName, null);
    // Remove from persistent store
    writer.queueRecord(cacheKey, null);
  });

  await writer.flush();
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

export function onCachedRootUpdate(
  listener: (npub: string, treeName: string, cid: CID | null) => void
): () => void {
  updateListeners.add(listener);
  return () => {
    updateListeners.delete(listener);
  };
}

/**
 * Get cache stats
 */
export function getCacheStats(): { memoryEntries: number } {
  return {
    memoryEntries: memoryCache.size,
  };
}

function hashEquals(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

function optionalHashEquals(a?: Uint8Array, b?: Uint8Array): boolean {
  if (!a && !b) return true;
  if (!a || !b) return false;
  return hashEquals(a, b);
}

function labelsEqual(a?: string[], b?: string[]): boolean {
  if (!a && !b) return true;
  if (!a || !b) return false;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

function cachedRootEquals(a: CachedRoot, b: CachedRoot): boolean {
  return (
    hashEquals(a.hash, b.hash) &&
    optionalHashEquals(a.key, b.key) &&
    a.visibility === b.visibility &&
    labelsEqual(a.labels, b.labels) &&
    a.updatedAt === b.updatedAt &&
    a.snapshotNhash === b.snapshotNhash &&
    a.encryptedKey === b.encryptedKey &&
    a.keyId === b.keyId &&
    a.selfEncryptedKey === b.selfEncryptedKey &&
    a.selfEncryptedLinkKey === b.selfEncryptedLinkKey
  );
}
