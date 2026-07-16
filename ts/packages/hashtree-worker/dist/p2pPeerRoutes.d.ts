import type { MeshReadSource } from './capabilities/meshRouterStore.js';
import type { P2PBridge } from './p2pBridge.js';
/** Turns authenticated provider identities into explicit, independently verified blob routes. */
export declare class P2PPeerRoutes {
    private readonly bridge;
    private readonly cacheMs;
    private peerIds;
    private listError;
    private refreshedAt;
    private inflight;
    private generation;
    constructor(bridge: P2PBridge, cacheMs?: number);
    setEnabled(enabled: boolean): void;
    sources(): Promise<MeshReadSource[]>;
    peerList(): Promise<string[]>;
    private refresh;
}
//# sourceMappingURL=p2pPeerRoutes.d.ts.map