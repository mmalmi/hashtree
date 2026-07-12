import type { WorkerResponse } from './protocol.js';
type ExternalP2PRequest = Extract<WorkerResponse, {
    type: 'p2pFetch' | 'p2pPeerList';
}>;
export declare class ExternalP2PBridge {
    private readonly respond;
    private readonly fetchTimeoutMs;
    private readonly peerListTimeoutMs;
    private readonly pendingFetches;
    private readonly pendingPeerLists;
    private requestCounter;
    private enabled;
    constructor(options: {
        respond: (request: ExternalP2PRequest) => void;
        fetchTimeoutMs: number;
        peerListTimeoutMs: number;
    });
    setEnabled(enabled: boolean): void;
    isEnabled(): boolean;
    fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null>;
    listPeers(): Promise<string[]>;
    resolveFetch(requestId: string, data?: Uint8Array, error?: string): void;
    resolvePeerList(requestId: string, peerIds?: string[], error?: string): void;
    clear(): void;
    private nextRequestId;
}
export {};
//# sourceMappingURL=externalP2P.d.ts.map