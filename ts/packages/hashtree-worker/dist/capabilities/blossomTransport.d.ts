import { BlossomStore, type BlossomUploadCallback } from '@hashtree/core';
import type { BlossomServerConfig } from '../protocol.js';
import { type BlossomBandwidthStats, type BlossomBandwidthUpdateHandler } from './blossomBandwidthTracker.js';
export declare const DEFAULT_BLOSSOM_SERVERS: BlossomServerConfig[];
export type { BlossomBandwidthServerStats, BlossomBandwidthStats, BlossomBandwidthUpdateHandler, } from './blossomBandwidthTracker.js';
export declare class BlossomTransport {
    private servers;
    private readonly signer;
    private readonly bandwidthTracker;
    private readonly inflightFetches;
    private readonly fetchTimeoutMs;
    private store;
    constructor(servers?: BlossomServerConfig[], onBandwidthUpdate?: BlossomBandwidthUpdateHandler, fetchTimeoutMs?: number);
    setServers(servers: BlossomServerConfig[]): void;
    getServers(): BlossomServerConfig[];
    getReadServers(): BlossomServerConfig[];
    getWriteServers(): BlossomServerConfig[];
    getBandwidthStats(): BlossomBandwidthStats;
    private createStore;
    createUploadStore(onUploadProgress?: BlossomUploadCallback): BlossomStore;
    upload(hashHex: string, data: Uint8Array, _mimeType?: string, onUploadProgress?: BlossomUploadCallback): Promise<void>;
    fetch(hashHex: string): Promise<Uint8Array | null>;
    fetchFromServer(hashHex: string, serverUrl: string): Promise<Uint8Array | null>;
    private fetchInternal;
}
//# sourceMappingURL=blossomTransport.d.ts.map