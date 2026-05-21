import type { WorkerFactory } from './client.js';
import type { BlossomBandwidthStats, BlossomServerConfig, PeerStats as RelayPeerStats, RelayStats, TreeRootInfo, WorkerConfig as RelayWorkerConfig, WorkerRequest as RelayWorkerRequest, WorkerResponse as RelayWorkerResponse } from './relay/protocol.js';
export interface TreeRootUpdate extends TreeRootInfo {
    npub: string;
    treeName: string;
}
export interface RelayWorkerClientConfig extends RelayWorkerConfig {
    maxWebRTCUploadBytesPerSecond?: number | null;
}
export type { BlossomBandwidthStats, BlossomServerConfig, RelayPeerStats, RelayStats, TreeRootInfo, RelayWorkerConfig, RelayWorkerRequest, RelayWorkerResponse, };
export declare class RelayWorkerClient {
    private readonly workerFactory;
    private readonly config;
    private worker;
    private webrtcProxy;
    private initPromise;
    private initPending;
    private pendingRequests;
    private treeRootListeners;
    private blossomBandwidthListeners;
    constructor(workerFactory: WorkerFactory, config: RelayWorkerClientConfig);
    init(): Promise<void>;
    private spawnWorker;
    private getNostrExtension;
    private handleSignRequest;
    private handleEncryptRequest;
    private handleDecryptRequest;
    private nextRequestId;
    private resolvePending;
    private rejectPending;
    private rejectAllPending;
    private request;
    registerMediaPort(port: MessagePort, debug?: boolean): Promise<void>;
    getTreeRootInfo(npub: string, treeName: string): Promise<TreeRootInfo | null>;
    getPeerStats(): Promise<RelayPeerStats[]>;
    getRelayStats(): Promise<RelayStats[]>;
    setIdentity(pubkey: string, nsecHex?: string): Promise<void>;
    setWebRTCPools(pools: {
        follows: {
            max: number;
            satisfied: number;
        };
        other: {
            max: number;
            satisfied: number;
        };
    }): Promise<void>;
    setUploadLimitBytesPerSecond(maxUploadBytesPerSecond?: number | null): void;
    setFollows(follows: string[]): Promise<void>;
    sendHello(): Promise<void>;
    setBlossomServers(servers: BlossomServerConfig[]): Promise<void>;
    setStorageMaxBytes(maxBytes: number): Promise<void>;
    setRelays(relays: string[]): Promise<void>;
    subscribeTreeRoots(pubkey: string): Promise<void>;
    unsubscribeTreeRoots(pubkey: string): Promise<void>;
    onTreeRootUpdate(listener: (update: TreeRootUpdate) => void): () => void;
    onBlossomBandwidth(listener: (stats: BlossomBandwidthStats) => void): () => void;
    close(): Promise<void>;
}
//# sourceMappingURL=relay-client.d.ts.map