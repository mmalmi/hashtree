/**
 * Tree Root Subscription Handler
 *
 * Worker subscribes directly to tree root events (kind 30064 with legacy 30078 support).
 * Updates local cache and notifies main thread of changes.
 */
import type { CID } from '@hashtree/core';
import type { SignedEvent, TreeVisibility } from './protocol';
export interface TreeRootRecord {
    hash: Uint8Array;
    key?: Uint8Array;
    visibility: TreeVisibility;
    labels?: string[];
    updatedAt: number;
    snapshotNhash?: string;
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
}
export interface ParsedTreeRootEvent {
    hash: string;
    key?: string;
    visibility: TreeVisibility;
    labels?: string[];
    encryptedKey?: string;
    keyId?: string;
    selfEncryptedKey?: string;
    selfEncryptedLinkKey?: string;
}
export declare function parseTreeRootEvent(event: SignedEvent): ParsedTreeRootEvent | null;
export declare function getHistoricalTreeRoots(npub: string, treeName: string, timeoutMs?: number): Promise<CID[]>;
export declare function resolveTreeRootNow(npub: string, treeName: string, timeoutMs?: number): Promise<CID | null>;
/**
 * Set callback to notify main thread of tree root updates
 */
export declare function setNotifyCallback(callback: (npub: string, treeName: string, record: TreeRootRecord) => void): void;
/**
 * Subscribe to tree roots for a specific pubkey
 */
export declare function subscribeToTreeRoots(pubkeyHex: string): () => void;
/**
 * Unsubscribe from tree roots for a specific pubkey
 */
export declare function unsubscribeFromTreeRoots(pubkeyHex: string): void;
export declare function handleTreeRootEvent(event: SignedEvent): Promise<void>;
/**
 * Check if an event is a tree root event
 */
export declare function isTreeRootEvent(event: SignedEvent): boolean;
/**
 * Get all active subscription pubkeys
 */
export declare function getActiveSubscriptions(): string[];
//# sourceMappingURL=treeRootSubscription.d.ts.map