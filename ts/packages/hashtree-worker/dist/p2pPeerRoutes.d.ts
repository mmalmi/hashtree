import { type BlobRequest, type BlobRoute, type BlobRouteContext } from '@hashtree/core';
import type { P2PBridge } from './p2pBridge.js';
/** One route over listed peers, or an aggregate provider that owns peer discovery. */
export declare class P2PPeerRoutes implements BlobRoute {
    private readonly bridge;
    private readonly cacheMs;
    readonly id = "p2p";
    private peerIds;
    private peerRoutes;
    private readonly router;
    private refreshedAt;
    private inflight;
    private generation;
    private peerListSupported;
    constructor(bridge: P2PBridge, cacheMs?: number);
    isAvailable: () => boolean;
    setEnabled(enabled: boolean, peerListSupported?: boolean): void;
    read(request: BlobRequest, context?: BlobRouteContext): Promise<import("@hashtree/core").BlobReply>;
    peerList(): Promise<string[]>;
    private syncPeerRoutes;
}
//# sourceMappingURL=p2pPeerRoutes.d.ts.map