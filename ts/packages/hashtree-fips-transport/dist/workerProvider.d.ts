import { type Store } from '@hashtree/core';
import type { FipsDatagramEndpoint } from '@fips/tcp';
import { TcpBlobTransport } from './tcpBlobTransport.js';
export declare const HASHTREE_BLOB_CAPABILITY = "hashtree.blob/1";
export interface FipsBlobRoute {
    peerId: string;
    htl: number;
    priority?: number;
}
export interface FipsCapabilityAdvertisement {
    peerId: string;
    capabilities: readonly {
        name: string;
        fspPort?: number;
        priority?: number;
    }[];
}
export type FipsBlobRouteSource = (() => readonly FipsBlobRoute[] | Promise<readonly FipsBlobRoute[]>) | readonly FipsBlobRoute[];
export interface HashtreeWorkerP2PProvider {
    fetch(hashHex: string, peerId?: string, htl?: number): Promise<Uint8Array | null>;
    listPeerIds(): string[] | Promise<string[]>;
}
export type FipsWorkerNode = FipsDatagramEndpoint;
export interface FipsWorkerP2PProviderOptions {
    node: FipsWorkerNode;
    localStore: Store;
    requestTimeoutMs?: number;
    /** Authenticated capability routes or explicitly configured Hashtree peers. */
    providerRoutes?: FipsBlobRouteSource;
    /** Authorize an authenticated FIPS identity before serving local blobs. */
    allowIncomingPeer?: (peerId: string) => boolean | Promise<boolean>;
}
/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Provider selection comes only from the supplied route source; connected FIPS
 * peers are never inferred to be Hashtree providers.
 */
export declare class FipsWorkerP2PProvider implements HashtreeWorkerP2PProvider {
    private readonly options;
    readonly transport: TcpBlobTransport;
    private closed;
    constructor(options: FipsWorkerP2PProviderOptions);
    fetch(hashHex: string, peerId?: string, htl?: number): Promise<Uint8Array | null>;
    private fetchHash;
    listPeerIds(): Promise<string[]>;
    close(): void;
    private routes;
    private fetchRoutes;
}
export declare function createFipsWorkerP2PProvider(options: FipsWorkerP2PProviderOptions): FipsWorkerP2PProvider;
/** Convert an authenticated FSP local-instance roster into local-only blob routes. */
export declare function blobRoutesFromCapabilityRoster(advertisements: readonly FipsCapabilityAdvertisement[]): FipsBlobRoute[];
//# sourceMappingURL=workerProvider.d.ts.map