import { type RuntimeCacheMode, type RuntimeCompletion, type RuntimeEventInfo, type RuntimePublishResult, type RuntimeQueryOptions, type RuntimeQueryResult, type RuntimeSource } from 'nostr-pubsub';
import { type Store } from '@hashtree/core';
import type { SignedEvent, NostrFilter } from './protocol.js';
declare let onEvent: ((subId: string, event: SignedEvent, info: RuntimeEventInfo) => void | Promise<void>) | null;
declare let onEose: ((subId: string, status: RuntimeCompletion) => void) | null;
export declare function initNostrRuntime(relays: string[], options: {
    store: Store;
    storeName: string;
}): Promise<void>;
export declare function setOnEvent(callback: typeof onEvent): void;
export declare function setOnEose(callback: typeof onEose): void;
export declare function subscribe(subId: string, filters: NostrFilter[], options?: {
    cache?: RuntimeCacheMode;
}): void;
export declare function unsubscribe(subId: string): void;
export declare function publish(event: SignedEvent): Promise<RuntimePublishResult>;
export declare function query(filters: NostrFilter[], options?: RuntimeQueryOptions): Promise<RuntimeQueryResult>;
export declare function addSource(source: RuntimeSource): void;
export declare function removeSource(id: string): void;
export declare function setRelays(relays: string[]): void;
export declare function getRelayStats(): {
    eventsReceived: number;
    eventsSent: number;
    url: string;
    connected: boolean;
}[];
export declare function closeNostrRuntime(): Promise<void>;
type Signer = (event: {
    kind: number;
    created_at: number;
    content: string;
    tags: string[][];
}) => Promise<SignedEvent>;
type BlossomPush = (hash: Uint8Array, key?: Uint8Array, treeName?: string) => Promise<{
    pushed: number;
    skipped: number;
    failed: number;
}>;
export declare function republishTrees(pubkey: string, _sign: Signer, push?: BlossomPush, prefix?: string): Promise<number>;
export declare function republishTree(pubkey: string, treeName: string): Promise<boolean>;
export {};
//# sourceMappingURL=nostrRuntime.d.ts.map