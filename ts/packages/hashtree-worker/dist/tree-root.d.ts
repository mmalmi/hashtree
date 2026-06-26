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
import type { Hash, TreeVisibility } from '@hashtree/core';
/**
 * Source of the tree root update
 */
export type TreeRootSource = 'local-write' | 'nostr' | 'prefetch' | 'worker';
/**
 * Core record format - single source of truth for all root data
 */
export interface TreeRootRecord {
    hash: Hash;
    key?: Hash;
    visibility: TreeVisibility;
    labels?: string[];
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
/**
 * TreeRootRegistry - singleton class for managing tree root data
 */
declare class TreeRootRegistryImpl {
    private records;
    private listeners;
    private globalListeners;
    private persistence;
    private publishTimers;
    private publishFn;
    private publishDelay;
    private retryDelay;
    private lastLocalUpdatedAt;
    constructor(persistence?: RegistryPersistence);
    /**
     * Hydrate from persistence on startup
     */
    private hydrate;
    /**
     * Set the publish function (called with throttling for dirty records)
     */
    setPublishFn(fn: (npub: string, treeName: string, record: TreeRootRecord) => Promise<boolean>): void;
    /**
     * Build cache key from npub and treeName
     */
    private makeKey;
    /**
     * Notify listeners of a record change
     */
    private notify;
    private shouldAcceptUpdate;
    private mergeSameHashMetadata;
    private nextLocalUpdatedAt;
    /**
     * Sync lookup - returns cached record or null (no side effects)
     */
    get(npub: string, treeName: string): TreeRootRecord | null;
    /**
     * Get by key directly
     */
    getByKey(key: string): TreeRootRecord | null;
    /**
     * Async resolve - returns current record if cached, otherwise waits for first resolve
     */
    resolve(npub: string, treeName: string, options?: {
        timeoutMs?: number;
    }): Promise<TreeRootRecord | null>;
    /**
     * Subscribe to updates for a specific tree
     * Emits current snapshot immediately if available, then future updates
     */
    subscribe(npub: string, treeName: string, callback: Listener): () => void;
    /**
     * Subscribe to all registry updates (for bridges like Tauri/worker)
     */
    subscribeAll(callback: (key: string, record: TreeRootRecord | null) => void): () => void;
    /**
     * Set record from local write - marks dirty and schedules publish
     */
    setLocal(npub: string, treeName: string, hash: Hash, options?: {
        key?: Hash;
        visibility?: TreeVisibility;
        labels?: string[];
        encryptedKey?: string;
        keyId?: string;
        selfEncryptedKey?: string;
        selfEncryptedLinkKey?: string;
    }): void;
    /**
     * Set record from resolver (Nostr event) - only updates if newer
     */
    setFromResolver(npub: string, treeName: string, hash: Hash, updatedAt: number, options?: {
        key?: Hash;
        visibility?: TreeVisibility;
        labels?: string[];
        encryptedKey?: string;
        keyId?: string;
        selfEncryptedKey?: string;
        selfEncryptedLinkKey?: string;
    }): boolean;
    /**
     * Merge a decrypted key into an existing record without changing updatedAt/source.
     * Returns true if the record was updated.
     */
    mergeKey(npub: string, treeName: string, hash: Hash, key: Hash): boolean;
    /**
     * Set record from worker (Nostr subscription routed through worker)
     * Similar to setFromResolver but source is 'worker'
     */
    setFromWorker(npub: string, treeName: string, hash: Hash, updatedAt: number, options?: {
        key?: Hash;
        visibility?: TreeVisibility;
        labels?: string[];
        encryptedKey?: string;
        keyId?: string;
        selfEncryptedKey?: string;
        selfEncryptedLinkKey?: string;
    }): boolean;
    /**
     * Set record from external source (Tauri, worker, prefetch)
     */
    setFromExternal(npub: string, treeName: string, hash: Hash, source: TreeRootSource, options?: {
        key?: Hash;
        visibility?: TreeVisibility;
        labels?: string[];
        updatedAt?: number;
        encryptedKey?: string;
        keyId?: string;
        selfEncryptedKey?: string;
        selfEncryptedLinkKey?: string;
    }): void;
    /**
     * Delete a record
     */
    delete(npub: string, treeName: string): void;
    /**
     * Schedule a throttled publish
     */
    private schedulePublish;
    /**
     * Execute the publish
     */
    private doPublish;
    /**
     * Force immediate publish of all dirty records
     */
    flushPendingPublishes(): Promise<void>;
    /**
     * Cancel pending publish (call before delete to prevent "undelete")
     */
    cancelPendingPublish(npub: string, treeName: string): void;
    /**
     * Get all records (for debugging/migration)
     */
    getAllRecords(): Map<string, TreeRootRecord>;
    /**
     * Check if a record exists
     */
    has(npub: string, treeName: string): boolean;
    /**
     * Get visibility for a tree
     */
    getVisibility(npub: string, treeName: string): TreeVisibility | undefined;
    /**
     * Get labels for a tree
     */
    getLabels(npub: string, treeName: string): string[] | undefined;
}
declare global {
    interface Window {
        __treeRootRegistry?: TreeRootRegistryImpl;
    }
}
export declare const treeRootRegistry: TreeRootRegistryImpl;
export type { TreeRootRecord as TreeRootEntry };
//# sourceMappingURL=tree-root.d.ts.map