import { type BlobReply, type BlobRequest } from '@hashtree/core';
export type P2PBridgeRequest = {
    type: 'p2pFetch';
    requestId: string;
    hashHex: string;
    htl: number;
    peerId?: string;
} | {
    type: 'p2pPeerList';
    requestId: string;
};
export declare class P2PBridge {
    private readonly respond;
    private readonly fetchTimeoutMs?;
    private readonly peerListTimeoutMs;
    private readonly fetches;
    private readonly peerLists;
    private requestCounter;
    private enabled;
    constructor(options: {
        respond: (request: P2PBridgeRequest) => void;
        fetchTimeoutMs?: number;
        peerListTimeoutMs: number;
    });
    setEnabled(enabled: boolean): void;
    isEnabled(): boolean;
    fetch(request: BlobRequest, peerId?: string, signal?: AbortSignal): Promise<BlobReply>;
    listPeers(): Promise<string[]>;
    resolveFetch(requestId: string, data?: Uint8Array, error?: string): void;
    resolvePeerList(requestId: string, peerIds?: string[], error?: string): void;
    clear(message?: string): void;
    private rejectFetch;
    private rejectPeerList;
    private take;
    private nextRequestId;
}
//# sourceMappingURL=p2pBridge.d.ts.map