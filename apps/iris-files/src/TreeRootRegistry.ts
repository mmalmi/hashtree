/**
 * TreeRootRegistry - Single source of truth for tree root data
 *
 * This module provides:
 * - Unified record format for all root data
 * - Subscription API that emits cached data immediately, then updates
 * - Async resolve with timeout for waiting on first resolution
 * - Local write tracking with dirty flag for publish throttling
 * - Pluggable persistence (localStorage by default)
 *
 * @see tree-root-caching-plan.md for architecture details
 */

import type { Hash, TreeVisibility } from 'hashtree';
import { fromHex, toHex } from 'hashtree';

/**
 * Source of the tree root update
 */
export type TreeRootSource = 'local-write' | 'nostr' | 'prefetch' | 'tauri' | 'worker';

/**
 * Core record format - single source of truth for all root data
 */
export interface TreeRootRecord {
  hash: Hash;
  key?: Hash;
  visibility: TreeVisibility;
  updatedAt: number; // Unix seconds (event created_at or local timestamp)
  source: TreeRootSource;
  dirty: boolean; // Local writes pending publish

  // Visibility-specific fields
  encryptedKey?: string; // For link-visible: XOR(contentKey, linkKey)
  keyId?: string; // For link-visible: derived from linkKey
  selfEncryptedKey?: string; // For private: NIP-44 encrypted content key
  selfEncryptedLinkKey?: string; // For link-visible: NIP-44 encrypted link key
}

/**
 * Serialized format for localStorage persistence
 */
interface PersistedRecord {
  hash: string; // hex
  key?: string; // hex
  visibility: TreeVisibility;
  updatedAt: number;
  source: TreeRootSource;
  dirty: boolean;
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
}

/**
 * Listener callback type
 */
type Listener = (record: TreeRootRecord | null) => void;

/**
 * Persistence interface - allows swapping localStorage for IndexedDB/etc
 */
export interface RegistryPersistence {
  save(key: string, record: TreeRootRecord): void;
  load(key: string): TreeRootRecord | null;
  delete(key: string): void;
  loadAll(): Map<string, TreeRootRecord>;
}

const STORAGE_KEY = 'hashtree:localRootCache';

/**
 * Default localStorage persistence
 */
class LocalStoragePersistence implements RegistryPersistence {
  private cache: Map<string, TreeRootRecord> | null = null;

  private serializeRecord(record: TreeRootRecord): PersistedRecord {
    return {
      hash: toHex(record.hash),
      key: record.key ? toHex(record.key) : undefined,
      visibility: record.visibility,
      updatedAt: record.updatedAt,
      source: record.source,
      dirty: record.dirty,
      encryptedKey: record.encryptedKey,
      keyId: record.keyId,
      selfEncryptedKey: record.selfEncryptedKey,
      selfEncryptedLinkKey: record.selfEncryptedLinkKey,
    };
  }

  private deserializeRecord(data: PersistedRecord): TreeRootRecord | null {
    try {
      return {
        hash: fromHex(data.hash),
        key: data.key ? fromHex(data.key) : undefined,
        visibility: data.visibility,
        updatedAt: data.updatedAt,
        source: data.source,
        dirty: data.dirty,
        encryptedKey: data.encryptedKey,
        keyId: data.keyId,
        selfEncryptedKey: data.selfEncryptedKey,
        selfEncryptedLinkKey: data.selfEncryptedLinkKey,
      };
    } catch {
      return null;
    }
  }

  save(key: string, record: TreeRootRecord): void {
    if (typeof window === 'undefined') return;

    // Update in-memory cache
    if (!this.cache) {
      this.cache = this.loadAll();
    }
    this.cache.set(key, record);

    // Persist to localStorage
    try {
      const data: Record<string, PersistedRecord> = {};
      for (const [k, r] of this.cache.entries()) {
        data[k] = this.serializeRecord(r);
      }
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
    } catch {
      // Ignore persistence errors (storage may be full/unavailable)
    }
  }

  load(key: string): TreeRootRecord | null {
    if (!this.cache) {
      this.cache = this.loadAll();
    }
    return this.cache.get(key) ?? null;
  }

  delete(key: string): void {
    if (typeof window === 'undefined') return;

    if (!this.cache) {
      this.cache = this.loadAll();
    }
    this.cache.delete(key);

    try {
      const data: Record<string, PersistedRecord> = {};
      for (const [k, r] of this.cache.entries()) {
        data[k] = this.serializeRecord(r);
      }
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
    } catch {
      // Ignore persistence errors
    }
  }

  loadAll(): Map<string, TreeRootRecord> {
    const result = new Map<string, TreeRootRecord>();
    if (typeof window === 'undefined') return result;

    try {
      const raw = window.localStorage.getItem(STORAGE_KEY);
      if (!raw) return result;

      const data = JSON.parse(raw) as Record<string, PersistedRecord>;
      for (const [key, persisted] of Object.entries(data)) {
        const record = this.deserializeRecord(persisted);
        if (record) {
          result.set(key, record);
        }
      }
    } catch {
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
  private records = new Map<string, TreeRootRecord>();
  private listeners = new Map<string, Set<Listener>>();
  private globalListeners = new Set<(key: string, record: TreeRootRecord | null) => void>();
  private persistence: RegistryPersistence;

  // Publish throttling
  private publishTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private publishFn: ((npub: string, treeName: string, record: TreeRootRecord) => Promise<boolean>) | null = null;
  private publishDelay = 1000;
  private retryDelay = 5000;

  constructor(persistence?: RegistryPersistence) {
    this.persistence = persistence ?? new LocalStoragePersistence();
    this.hydrate();
  }

  /**
   * Hydrate from persistence on startup
   */
  private hydrate(): void {
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
  setPublishFn(fn: (npub: string, treeName: string, record: TreeRootRecord) => Promise<boolean>): void {
    this.publishFn = fn;
  }

  /**
   * Build cache key from npub and treeName
   */
  private makeKey(npub: string, treeName: string): string {
    return `${npub}/${treeName}`;
  }

  /**
   * Notify listeners of a record change
   */
  private notify(key: string, record: TreeRootRecord | null): void {
    const keyListeners = this.listeners.get(key);
    if (keyListeners) {
      for (const listener of keyListeners) {
        try {
          listener(record);
        } catch (e) {
          console.error('[TreeRootRegistry] Listener error:', e);
        }
      }
    }

    // Notify global listeners
    for (const listener of this.globalListeners) {
      try {
        listener(key, record);
      } catch (e) {
        console.error('[TreeRootRegistry] Global listener error:', e);
      }
    }
  }

  /**
   * Sync lookup - returns cached record or null (no side effects)
   */
  get(npub: string, treeName: string): TreeRootRecord | null {
    return this.records.get(this.makeKey(npub, treeName)) ?? null;
  }

  /**
   * Get by key directly
   */
  getByKey(key: string): TreeRootRecord | null {
    return this.records.get(key) ?? null;
  }

  /**
   * Async resolve - returns current record if cached, otherwise waits for first resolve
   */
  async resolve(
    npub: string,
    treeName: string,
    options?: { timeoutMs?: number }
  ): Promise<TreeRootRecord | null> {
    const key = this.makeKey(npub, treeName);
    const existing = this.records.get(key);
    if (existing) {
      return existing;
    }

    const timeoutMs = options?.timeoutMs ?? 10000;

    return new Promise((resolve) => {
      let resolved = false;
      let unsubscribe: (() => void) | null = null;

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
  subscribe(npub: string, treeName: string, callback: Listener): () => void {
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
  subscribeAll(callback: (key: string, record: TreeRootRecord | null) => void): () => void {
    this.globalListeners.add(callback);
    return () => {
      this.globalListeners.delete(callback);
    };
  }

  /**
   * Set record from local write - marks dirty and schedules publish
   */
  setLocal(
    npub: string,
    treeName: string,
    hash: Hash,
    options?: {
      key?: Hash;
      visibility?: TreeVisibility;
      encryptedKey?: string;
      keyId?: string;
      selfEncryptedKey?: string;
      selfEncryptedLinkKey?: string;
    }
  ): void {
    const cacheKey = this.makeKey(npub, treeName);
    const existing = this.records.get(cacheKey);

    // Preserve existing visibility if not provided
    const visibility = options?.visibility ?? existing?.visibility ?? 'public';

    const record: TreeRootRecord = {
      hash,
      key: options?.key,
      visibility,
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
  setFromResolver(
    npub: string,
    treeName: string,
    hash: Hash,
    updatedAt: number,
    options?: {
      key?: Hash;
      visibility?: TreeVisibility;
      encryptedKey?: string;
      keyId?: string;
      selfEncryptedKey?: string;
      selfEncryptedLinkKey?: string;
    }
  ): boolean {
    const cacheKey = this.makeKey(npub, treeName);
    const existing = this.records.get(cacheKey);

    // Only update if newer (based on updatedAt timestamp)
    // Exception: if existing is dirty (local write), it takes precedence
    if (existing) {
      if (existing.dirty) {
        // Local write takes precedence - don't overwrite
        return false;
      }
      if (existing.updatedAt >= updatedAt) {
        // Existing is newer or same - don't overwrite
        return false;
      }
    }

    const record: TreeRootRecord = {
      hash,
      key: options?.key,
      visibility: options?.visibility ?? 'public',
      updatedAt,
      source: 'nostr',
      dirty: false,
      encryptedKey: options?.encryptedKey,
      keyId: options?.keyId,
      selfEncryptedKey: options?.selfEncryptedKey,
      selfEncryptedLinkKey: options?.selfEncryptedLinkKey,
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
  mergeKey(
    npub: string,
    treeName: string,
    hash: Hash,
    key: Hash
  ): boolean {
    const cacheKey = this.makeKey(npub, treeName);
    const existing = this.records.get(cacheKey);
    if (!existing) return false;

    if (toHex(existing.hash) !== toHex(hash)) return false;
    if (existing.key) return false;

    existing.key = key;
    this.persistence.save(cacheKey, existing);
    this.notify(cacheKey, existing);
    return true;
  }

  /**
   * Set record from worker (Nostr subscription routed through worker)
   * Similar to setFromResolver but source is 'worker'
   */
  setFromWorker(
    npub: string,
    treeName: string,
    hash: Hash,
    updatedAt: number,
    options?: {
      key?: Hash;
      visibility?: TreeVisibility;
      encryptedKey?: string;
      keyId?: string;
      selfEncryptedKey?: string;
      selfEncryptedLinkKey?: string;
    }
  ): boolean {
    const cacheKey = this.makeKey(npub, treeName);
    const existing = this.records.get(cacheKey);

    // Only update if newer (based on updatedAt timestamp)
    // Exception: if existing is dirty (local write), it takes precedence
    if (existing) {
      if (existing.dirty) {
        // Local write takes precedence - don't overwrite
        return false;
      }
      if (existing.updatedAt >= updatedAt) {
        // Existing is newer or same - don't overwrite
        return false;
      }
    }

    const record: TreeRootRecord = {
      hash,
      key: options?.key,
      visibility: options?.visibility ?? 'public',
      updatedAt,
      source: 'worker',
      dirty: false,
      encryptedKey: options?.encryptedKey,
      keyId: options?.keyId,
      selfEncryptedKey: options?.selfEncryptedKey,
      selfEncryptedLinkKey: options?.selfEncryptedLinkKey,
    };

    this.records.set(cacheKey, record);
    this.persistence.save(cacheKey, record);
    this.notify(cacheKey, record);
    return true;
  }

  /**
   * Set record from external source (Tauri, worker, prefetch)
   */
  setFromExternal(
    npub: string,
    treeName: string,
    hash: Hash,
    source: TreeRootSource,
    options?: {
      key?: Hash;
      visibility?: TreeVisibility;
      updatedAt?: number;
    }
  ): void {
    const cacheKey = this.makeKey(npub, treeName);
    const existing = this.records.get(cacheKey);

    // Don't overwrite dirty local writes
    if (existing?.dirty) {
      return;
    }

    const updatedAt = options?.updatedAt ?? Math.floor(Date.now() / 1000);

    // Only update if newer
    if (existing && existing.updatedAt >= updatedAt) {
      return;
    }

    const record: TreeRootRecord = {
      hash,
      key: options?.key,
      visibility: options?.visibility ?? existing?.visibility ?? 'public',
      updatedAt,
      source,
      dirty: false,
    };

    this.records.set(cacheKey, record);
    this.persistence.save(cacheKey, record);
    this.notify(cacheKey, record);
  }

  /**
   * Delete a record
   */
  delete(npub: string, treeName: string): void {
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
  private schedulePublish(npub: string, treeName: string, delay: number = this.publishDelay): void {
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
  private async doPublish(npub: string, treeName: string): Promise<void> {
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
      } else if (!this.publishTimers.has(key)) {
        this.schedulePublish(npub, treeName, this.retryDelay);
      }
    } catch (e) {
      console.error('[TreeRootRegistry] Publish failed:', e);
      if (!this.publishTimers.has(key)) {
        this.schedulePublish(npub, treeName, this.retryDelay);
      }
    }
  }

  /**
   * Force immediate publish of all dirty records
   */
  async flushPendingPublishes(): Promise<void> {
    if (!this.publishFn) {
      console.warn('[TreeRootRegistry] flushPendingPublishes: publishFn not set');
      return;
    }

    const promises: Promise<void>[] = [];

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
  cancelPendingPublish(npub: string, treeName: string): void {
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
  getAllRecords(): Map<string, TreeRootRecord> {
    return new Map(this.records);
  }

  /**
   * Check if a record exists
   */
  has(npub: string, treeName: string): boolean {
    return this.records.has(this.makeKey(npub, treeName));
  }

  /**
   * Get visibility for a tree
   */
  getVisibility(npub: string, treeName: string): TreeVisibility | undefined {
    return this.records.get(this.makeKey(npub, treeName))?.visibility;
  }
}

// Singleton instance - use window to survive HMR
declare global {
  interface Window {
    __treeRootRegistry?: TreeRootRegistryImpl;
  }
}

function getRegistry(): TreeRootRegistryImpl {
  if (typeof window !== 'undefined' && window.__treeRootRegistry) {
    return window.__treeRootRegistry;
  }

  const registry = new TreeRootRegistryImpl();

  if (typeof window !== 'undefined') {
    window.__treeRootRegistry = registry;
  }

  return registry;
}

// Export singleton instance
export const treeRootRegistry = getRegistry();

// Export types for consumers
export type { TreeRootRecord as TreeRootEntry };
