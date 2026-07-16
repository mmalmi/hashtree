import type { BlobRequest, BlobRoute, BlobRouteContext } from '@hashtree/core';
import type { P2PBridge } from './p2pBridge.js';
/** One composite route whose provider owns authenticated peer selection. */
export declare class P2PPeerRoutes implements BlobRoute {
    private readonly bridge;
    private readonly cacheMs;
    readonly id = "p2p";
    private peerIds;
    private refreshedAt;
    private inflight;
    private generation;
    constructor(bridge: P2PBridge, cacheMs?: number);
    isAvailable: () => boolean;
    setEnabled(enabled: boolean): void;
    read(request: BlobRequest, context?: BlobRouteContext): Promise<import("@hashtree/core").BlobReply>;
    peerList(): Promise<string[]>;
}
//# sourceMappingURL=p2pPeerRoutes.d.ts.map