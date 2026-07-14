import type { CID } from '@hashtree/core';
export interface BlossomServerConfig {
    url: string;
    read?: boolean;
    write?: boolean;
    preferBatchReads?: boolean;
}
export interface WorkerConfig {
    storeName?: string;
    blossomServers?: BlossomServerConfig[];
    relays?: string[];
    storageMaxBytes?: number;
    connectivityProbeIntervalMs?: number;
    diagnosticsEnabled?: boolean;
    diagnosticsMirrorToConsole?: boolean;
}
export type WorkerDiagnosticLevel = 'debug' | 'info' | 'warn' | 'error';
export type WorkerDiagnosticDataValue = string | number | boolean | null;
export interface WorkerDiagnosticEvent {
    scope: string;
    code: string;
    level: WorkerDiagnosticLevel;
    message: string;
    timestamp: number;
    data?: Record<string, WorkerDiagnosticDataValue>;
}
export interface ConnectivityState {
    online: boolean;
    reachableReadServers: number;
    totalReadServers: number;
    reachableWriteServers: number;
    totalWriteServers: number;
    updatedAt: number;
}
export type BlobSource = 'idb' | 'blossom' | 'p2p';
export interface UploadServerStatus {
    url: string;
    uploaded: number;
    skipped: number;
    failed: number;
}
export interface UploadProgressState {
    hashHex: string;
    nhash: string;
    totalServers: number;
    processedServers: number;
    uploadedServers: number;
    skippedServers: number;
    failedServers: number;
    totalChunks?: number;
    processedChunks?: number;
    /** 0..1 normalized progress for chunk upload traversal */
    progressRatio?: number;
    serverStatuses?: UploadServerStatus[];
    complete: boolean;
    error?: string;
}
export interface BlossomBandwidthServerStats {
    url: string;
    bytesSent: number;
    bytesReceived: number;
}
export interface BlossomBandwidthState {
    totalBytesSent: number;
    totalBytesReceived: number;
    updatedAt: number;
    servers: BlossomBandwidthServerStats[];
}
export interface BlobStreamStarted {
    id: string;
    streamId: string;
}
export interface RawBlockInput {
    data: Uint8Array;
    hashHex?: string;
    mimeType?: string;
}
export interface StoredBlockResult {
    hashHex: string;
    nhash: string;
}
export interface RootResolveOptions {
    timeoutMs?: number;
    settleMs?: number;
}
export type WorkerRequest = {
    type: 'init';
    id: string;
    config: WorkerConfig;
} | {
    type: 'close';
    id: string;
} | {
    type: 'putBlob';
    id: string;
    data: Uint8Array;
    mimeType?: string;
    upload?: boolean;
} | {
    type: 'putBlock';
    id: string;
    data: Uint8Array;
    hashHex?: string;
    mimeType?: string;
    upload?: boolean;
} | {
    type: 'putBlocks';
    id: string;
    blocks: RawBlockInput[];
    upload?: boolean;
} | {
    type: 'beginPutBlobStream';
    id: string;
    mimeType?: string;
    upload?: boolean;
} | {
    type: 'appendPutBlobStream';
    id: string;
    streamId: string;
    chunk: Uint8Array;
} | {
    type: 'finishPutBlobStream';
    id: string;
    streamId: string;
} | {
    type: 'cancelPutBlobStream';
    id: string;
    streamId: string;
} | {
    type: 'p2pFetchResult';
    id: string;
    requestId: string;
    data?: Uint8Array;
    error?: string;
} | {
    type: 'p2pPeerListResult';
    id: string;
    requestId: string;
    peerIds?: string[];
    error?: string;
} | {
    type: 'getBlob';
    id: string;
    hashHex: string;
    forPeer?: boolean;
    sourceIds?: string[];
    skipPrimary?: boolean;
} | {
    type: 'hasBlob';
    id: string;
    hashHex: string;
    sourceIds?: string[];
    skipPrimary?: boolean;
} | {
    type: 'registerMediaPort';
    id: string;
    port: MessagePort;
} | {
    type: 'setBlossomServers';
    id: string;
    servers: BlossomServerConfig[];
} | {
    type: 'setStorageMaxBytes';
    id: string;
    maxBytes: number;
} | {
    type: 'getStorageStats';
    id: string;
} | {
    type: 'probeConnectivity';
    id: string;
} | {
    type: 'resolveRoot';
    id: string;
    npub: string;
    path?: string;
    timeoutMs?: number;
    settleMs?: number;
} | {
    type: 'watchRoot';
    id: string;
    npub: string;
    path?: string;
    timeoutMs?: number;
    settleMs?: number;
} | {
    type: 'unwatchRoot';
    id: string;
    watchId: string;
};
export type WorkerResponse = {
    type: 'ready';
    id: string;
} | {
    type: 'error';
    id?: string;
    error: string;
} | {
    type: 'diagnostic';
    event: WorkerDiagnosticEvent;
} | {
    type: 'p2pFetch';
    requestId: string;
    hashHex: string;
    peerId?: string;
} | {
    type: 'p2pPeerList';
    requestId: string;
} | {
    type: 'blobStreamStarted';
    id: string;
    streamId: string;
} | {
    type: 'blobStored';
    id: string;
    hashHex: string;
    nhash: string;
} | {
    type: 'blockStored';
    id: string;
    block: StoredBlockResult;
} | {
    type: 'blocksStored';
    id: string;
    blocks: StoredBlockResult[];
} | {
    type: 'blob';
    id: string;
    data?: Uint8Array;
    source?: BlobSource;
    error?: string;
} | {
    type: 'availability';
    id: string;
    available: boolean;
    size?: number;
    source?: BlobSource;
    error?: string;
} | {
    type: 'cid';
    id: string;
    cid?: CID;
    error?: string;
} | {
    type: 'void';
    id: string;
    error?: string;
} | {
    type: 'storageStats';
    id: string;
    items: number;
    bytes: number;
    maxBytes: number;
    error?: string;
} | {
    type: 'connectivity';
    id: string;
    state?: ConnectivityState;
    error?: string;
} | {
    type: 'rootWatchStarted';
    id: string;
    watchId: string;
    cid?: CID;
    error?: string;
} | {
    type: 'rootUpdate';
    watchId: string;
    cid?: CID;
} | {
    type: 'connectivityUpdate';
    state: ConnectivityState;
} | {
    type: 'blossomBandwidth';
    stats: BlossomBandwidthState;
} | {
    type: 'uploadProgress';
    progress: UploadProgressState;
};
//# sourceMappingURL=protocol.d.ts.map