import type { Store } from '@hashtree/core';
import { type DataRequest, type PeerHTLConfig } from '@hashtree/mesh';
export interface MeshQueryRouterPeer {
    peerId: string;
    canSend: () => boolean;
    getHtlConfig: () => PeerHTLConfig;
    sendRequest: (hash: Uint8Array, htl: number) => boolean;
    sendResponse: (hash: Uint8Array, data: Uint8Array) => Promise<void>;
    onForwardedRequest?: () => void;
    onForwardedResolved?: () => void;
    onForwardedSuppressed?: () => void;
}
export interface MeshPeerQueryOptions {
    excludePeerId?: string;
    htl: number;
}
export interface MeshQueryRouterConfig {
    localStore: Store;
    requestTimeoutMs: number;
    upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>;
    queryPeers?: (hash: Uint8Array, options: MeshPeerQueryOptions) => Promise<Uint8Array | null>;
    maxForwardsPerPeerWindow?: number;
    forwardRateLimitWindowMs?: number;
}
export interface MeshForwardRateLimitConfig {
    maxForwardsPerPeerWindow?: number;
    windowMs?: number;
}
export declare class MeshQueryRouter {
    private readonly localStore;
    private readonly requestTimeoutMs;
    private rateLimiter;
    private readonly peers;
    private readonly hashesByRequester;
    private readonly inFlightByHash;
    private readonly pendingUpstreamFetches;
    private upstreamFetch?;
    private queryPeers?;
    constructor(config: MeshQueryRouterConfig);
    registerPeer(peer: MeshQueryRouterPeer): void;
    removePeer(peerId: string): void;
    setUpstreamFetch(upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>): void;
    setForwardRateLimit(config?: MeshForwardRateLimitConfig): void;
    hasInFlight(hashKey: string): boolean;
    stop(): void;
    private createRateLimiter;
    handleRequest(requesterId: string, req: DataRequest): Promise<void>;
    resolve(hash: Uint8Array, data: Uint8Array): Promise<void>;
    private beginQuery;
    private shouldAttemptPeerQuery;
    private clearQuery;
    private trackRequester;
    private forwardRequest;
    private startPeerQuery;
    private startUpstreamFetch;
}
export declare function encodeForwardRequest(hash: Uint8Array, htl: number): Uint8Array;
//# sourceMappingURL=meshQueryRouter.d.ts.map