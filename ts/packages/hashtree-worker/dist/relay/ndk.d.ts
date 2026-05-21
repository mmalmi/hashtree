/**
 * NDK instance for Worker
 *
 * Runs NDK with:
 * - Real relay connections
 * - ndk-cache (Dexie) for IndexedDB caching
 * - nostr-wasm for fast signature verification
 *
 * Main thread communicates via WorkerAdapter postMessage.
 */
import { type NDKSubscriptionOptions } from 'ndk';
import type { SignedEvent, NostrFilter } from './protocol';
/**
 * Initialize NDK with cache and nostr-wasm
 */
export declare function initNdk(relays: string[], options?: {
    pubkey?: string;
    nsec?: string;
}): Promise<void>;
/**
 * Get the NDK instance
 */
export declare function getNdk(): NDK | null;
/**
 * Set event callback
 */
export declare function setOnEvent(callback: (subId: string, event: SignedEvent) => void | Promise<void>): void;
/**
 * Set EOSE callback
 */
export declare function setOnEose(callback: (subId: string) => void): void;
/**
 * Subscribe to events
 */
export declare function subscribe(subId: string, filters: NostrFilter[], opts?: NDKSubscriptionOptions): void;
/**
 * Unsubscribe
 */
export declare function unsubscribe(subId: string): void;
/**
 * Publish an event
 */
export declare function publish(event: SignedEvent): Promise<void>;
/**
 * Close NDK and all subscriptions
 */
export declare function closeNdk(): void;
/**
 * Update relays dynamically
 * Disconnects old relays and connects to new ones
 */
export declare function setRelays(relays: string[]): Promise<void>;
/**
 * Get relay stats
 */
export declare function getRelayStats(): {
    url: string;
    connected: boolean;
    eventsReceived: number;
    eventsSent: number;
}[];
/**
 * Republish all user's hashtree events from cache to relays
 * This helps recover when events exist locally but weren't properly published
 *
 * For unsigned events (never signed), we sign them using the worker's signing flow.
 * For signed events, we republish directly.
 * Also pushes blob data to Blossom servers.
 *
 * @param prefix - Optional URL-encoded prefix to filter trees by d-tag
 */
export declare function republishTrees(pubkey: string, signFn: (template: {
    kind: number;
    created_at: number;
    content: string;
    tags: string[][];
}) => Promise<{
    id: string;
    pubkey: string;
    kind: number;
    content: string;
    tags: string[][];
    created_at: number;
    sig: string;
}>, pushToBlossomFn?: (hash: Uint8Array, key?: Uint8Array, treeName?: string) => Promise<{
    pushed: number;
    skipped: number;
    failed: number;
}>, prefix?: string): Promise<number>;
/**
 * Republish a single tree's event from cache to relays
 * This republishes the original event as-is (preserves signature/timestamp)
 * Works for any user's tree, not just own.
 */
export declare function republishTree(pubkey: string, treeName: string): Promise<boolean>;
//# sourceMappingURL=ndk.d.ts.map