import { type Store } from '@hashtree/core';
import { HashtreeFipsTransport, type FipsEndpoint, type FipsNodeLike, type HashtreeFipsTransportOptions } from './index.js';
export interface HashtreeWorkerP2PProvider {
    fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null>;
    listPeerIds(): string[] | Promise<string[]>;
}
export interface FipsWorkerP2PProviderOptions extends Omit<HashtreeFipsTransportOptions, 'endpoint' | 'localStore' | 'peers'> {
    node: FipsNodeLike;
    localStore: Store;
}
/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Peer selection remains dynamic: the provider reads authenticated FIPS links
 * for every request instead of maintaining a second discovery mesh.
 */
export declare class FipsWorkerP2PProvider implements HashtreeWorkerP2PProvider {
    readonly endpoint: FipsEndpoint;
    readonly transport: HashtreeFipsTransport;
    private closed;
    constructor(options: FipsWorkerP2PProviderOptions);
    fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null>;
    listPeerIds(): Promise<string[]>;
    close(): void;
}
export declare function createFipsWorkerP2PProvider(options: FipsWorkerP2PProviderOptions): FipsWorkerP2PProvider;
//# sourceMappingURL=workerProvider.d.ts.map