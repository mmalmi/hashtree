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
import { type StoreEntry } from './rootStoreWriter';
interface CachedRoot {
    hash: Uint8Array;
    key?: Uint8Array;
    visibility: TreeVisibility;
    labels?: string[];
    updatedAt: number;
    eventId?: string;
    snapshotNhash?: string;
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
}
export interface SetCachedRootResult {
    applied: boolean;
    record: CachedRoot;
}
/**
 * Order two versions of a replaceable root: positive when the candidate is
 * newer than the current one, negative when older, zero when they tie.
 */
export declare function compareReplaceableEventOrder(candidateUpdatedAt: number, candidateEventId: string | null | undefined, currentUpdatedAt: number, currentEventId: string | null | undefined): number;
/**
 * Initialize the cache with a store
 */
export declare function initTreeRootCache(storeImpl: Store): void;
export declare function getTreeRootCacheStore(): Store | null;
/**
 * Get a cached tree root
 */
export declare function getCachedRoot(npub: string, treeName: string): Promise<CID | null>;
/**
 * Get full cached root info (including visibility)
 */
export declare function getCachedRootInfo(npub: string, treeName: string): Promise<CachedRoot | null>;
/**
 * Cached root info when it is known without reading the store; `undefined`
 * when the store would have to be read.
 */
export declare function peekCachedRootInfo(npub: string, treeName: string): CachedRoot | null | undefined;
/**
 * Whether a root event carries nothing new for a cached record: it is older
 * than the record, or it is the event the record was built from.
 */
export declare function isRedundantRootEvent(record: CachedRoot, updatedAt: number, eventId: string | undefined): boolean;
/**
 * Resolves once every root staged so far has been written to the store.
 */
export declare function flushCachedRoots(): Promise<void>;
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
export declare function setCachedRoot(npub: string, treeName: string, cid: CID, visibility?: TreeVisibility, options?: SetCachedRootOptions): Promise<SetCachedRootResult>;
/**
 * Apply a tree root to the in-memory cache and queue its store write.
 *
 * The cache answers with the new root as soon as this resolves; persistence
 * follows in a grouped write (see `flushCachedRoots`).
 */
export declare function stageCachedRoot(npub: string, treeName: string, cid: CID, visibility?: TreeVisibility, options?: SetCachedRootOptions): Promise<SetCachedRootResult>;
/**
 * Merge a decrypted key into an existing cache entry (if hash matches).
 */
export declare function mergeCachedRootKey(npub: string, treeName: string, hash: Uint8Array, key: Uint8Array): Promise<boolean>;
/**
 * Remove a cached tree root
 */
export declare function removeCachedRoot(npub: string, treeName: string): Promise<void>;
/**
 * List all cached roots for an npub
 * Note: This scans memory cache only - persistent lookup requires iteration
 */
export declare function listCachedRoots(npub: string): Array<{
    treeName: string;
    cid: CID;
    visibility: TreeVisibility;
    updatedAt: number;
}>;
/**
 * Clear all cached roots (memory only)
 */
export declare function clearMemoryCache(): void;
export declare function onCachedRootUpdate(listener: (npub: string, treeName: string, cid: CID | null) => void): () => void;
/**
 * Get cache stats
 */
export declare function getCacheStats(): {
    memoryEntries: number;
};
export {};
//# sourceMappingURL=treeRootCache.d.ts.map