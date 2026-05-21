import { type CID } from '@hashtree/core';
import type { BlossomBandwidthState, BlossomServerConfig, BlobSource, ConnectivityState, RawBlockInput, RootResolveOptions, StoredBlockResult, WorkerDiagnosticEvent, UploadProgressState, WorkerConfig } from './protocol.js';
export type WorkerFactory = URL | string | (new () => Worker);
export type P2PFetchHandler = (hashHex: string, peerId?: string) => Promise<Uint8Array | null>;
export type P2PPeerListHandler = () => string[] | Promise<string[]>;
export interface WorkerP2PProvider {
    fetch: P2PFetchHandler;
    listPeerIds: P2PPeerListHandler;
}
export declare class HashtreeWorkerClient {
    private readonly workerFactory;
    private readonly config;
    private worker;
    private initPromise;
    private pending;
    private connectivityListeners;
    private uploadProgressListeners;
    private blossomBandwidthListeners;
    private diagnosticListeners;
    private rootWatchListeners;
    private pendingRootWatchUpdates;
    private p2pFetchHandler;
    private p2pPeerListHandler;
    constructor(workerFactory: WorkerFactory, config?: WorkerConfig);
    init(): Promise<void>;
    private spawnWorker;
    private rejectAllPending;
    private resolvePending;
    private rejectPending;
    private handleP2PFetch;
    private handleP2PPeerList;
    private request;
    private initIfNeeded;
    putBlob(data: Uint8Array, mimeType?: string, upload?: boolean): Promise<{
        hashHex: string;
        nhash: string;
    }>;
    putBlock(data: Uint8Array, options?: {
        hashHex?: string;
        mimeType?: string;
        upload?: boolean;
    }): Promise<StoredBlockResult>;
    putBlocks(blocks: RawBlockInput[], options?: {
        upload?: boolean;
    }): Promise<StoredBlockResult[]>;
    beginPutBlobStream(mimeType?: string, upload?: boolean): Promise<string>;
    appendPutBlobStream(streamId: string, chunk: Uint8Array): Promise<void>;
    finishPutBlobStream(streamId: string): Promise<{
        hashHex: string;
        nhash: string;
    }>;
    cancelPutBlobStream(streamId: string): Promise<void>;
    getBlob(hashHex: string): Promise<{
        data: Uint8Array;
        source: BlobSource;
    }>;
    getBlobForPeer(hashHex: string): Promise<Uint8Array | null>;
    setBlossomServers(servers: BlossomServerConfig[]): Promise<void>;
    registerMediaPort(port: MessagePort): Promise<void>;
    setStorageMaxBytes(maxBytes: number): Promise<void>;
    getStorageStats(): Promise<{
        items: number;
        bytes: number;
        maxBytes: number;
    }>;
    probeConnectivity(): Promise<ConnectivityState>;
    resolveRoot(npub: string, path?: string, options?: RootResolveOptions): Promise<CID | null>;
    watchRoot(npub: string, path: string | undefined, listener: (cid: CID | null) => void, options?: RootResolveOptions): Promise<() => Promise<void>>;
    onConnectivityUpdate(listener: (state: ConnectivityState) => void): () => void;
    onUploadProgress(listener: (progress: UploadProgressState) => void): () => void;
    onBlossomBandwidth(listener: (stats: BlossomBandwidthState) => void): () => void;
    onDiagnostic(listener: (event: WorkerDiagnosticEvent) => void): () => void;
    setP2PFetchHandler(handler: P2PFetchHandler | null): void;
    setP2PPeerListHandler(handler: P2PPeerListHandler | null): void;
    setP2PProvider(provider: WorkerP2PProvider | null): void;
    close(): Promise<void>;
}
//# sourceMappingURL=client.d.ts.map