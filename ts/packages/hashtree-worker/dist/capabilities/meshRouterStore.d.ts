import { type BlobRoute, type Hash, type Store } from '@hashtree/core';
import { type RequestDispatchConfig } from '@hashtree/mesh';
export interface MeshReadSource extends BlobRoute {
    canWrite?: boolean;
    isAvailable?: () => boolean;
}
export type MeshReadEndpoint = MeshReadSource;
export type MeshReadEndpointProvider = () => MeshReadEndpoint[];
export interface MeshRouterGetOptions {
    sourceIds?: readonly string[];
    skipPrimary?: boolean;
    htl?: number;
}
export interface MeshRouterGetResult {
    data: Uint8Array;
    sourceId: string;
}
export interface MeshRouterStoreConfig {
    primary: Store;
    sources?: MeshReadSource[];
    sourceProviders?: MeshReadEndpointProvider[];
    dispatch?: RequestDispatchConfig;
    requestTimeoutMs?: number;
    primaryReadTimeoutMs?: number;
    primarySourceId?: string;
}
interface SourceStats {
    requests: number;
    successes: number;
    misses: number;
    failures: number;
    timeouts: number;
    srttMs: number;
    rttvarMs: number;
    backoffLevel: number;
    backedOffUntilMs?: number;
    lastSuccessMs?: number;
    lastFailureMs?: number;
}
export declare class MeshRouterStore implements Store {
    private readonly primary;
    private readonly primarySourceId;
    private readonly dispatch;
    private readonly requestTimeoutMs;
    private readonly primaryReadTimeoutMs;
    private readonly sources;
    private readonly sourceProviders;
    private readonly statsBySource;
    private readonly inflightReads;
    constructor(config: MeshRouterStoreConfig);
    setSources(sources: MeshReadSource[]): void;
    addSource(source: MeshReadSource): void;
    removeSource(sourceId: string): void;
    getDetailed(hash: Hash, options?: MeshRouterGetOptions): Promise<MeshRouterGetResult | null>;
    private loadFromSourcesShared;
    private finishRead;
    getSourceStats(): Record<string, SourceStats>;
    put(hash: Hash, data: Uint8Array): Promise<boolean>;
    get(hash: Hash): Promise<Uint8Array | null>;
    has(hash: Hash): Promise<boolean>;
    delete(hash: Hash): Promise<boolean>;
    private readPrimary;
    private pendingReadKey;
    private verifyData;
    private getCandidateSources;
    private orderedSources;
    private createInFlightSourceRequest;
    private waitForNextResult;
    private loadFromSources;
    private statsFor;
    private recordRequest;
    private recordMiss;
    private recordSuccess;
    private recordFailure;
    private recordTimeout;
    private applyBackoff;
}
export {};
//# sourceMappingURL=meshRouterStore.d.ts.map