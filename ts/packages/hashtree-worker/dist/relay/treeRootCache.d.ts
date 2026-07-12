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
 * Cache a tree root
 */
export declare function setCachedRoot(npub: string, treeName: string, cid: CID, visibility?: TreeVisibility, options?: {
    updatedAt?: number;
    eventId?: string;
    labels?: string[];
    snapshotNhash?: string;
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
    force?: boolean;
}): Promise<SetCachedRootResult>;
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