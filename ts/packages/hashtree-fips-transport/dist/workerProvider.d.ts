import { type Store } from '@hashtree/core';
import type { FipsDatagramEndpoint } from '@fips/tcp';
import { TcpBlobTransport } from './tcpBlobTransport.js';
export interface HashtreeWorkerP2PProvider {
    fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null>;
    listPeerIds(): string[] | Promise<string[]>;
}
export interface FipsWorkerNode extends FipsDatagramEndpoint {
    on(event: 'peer', handler: (value: unknown) => void): () => void;
}
export interface FipsWorkerP2PProviderOptions {
    node: FipsWorkerNode;
    localStore: Store;
    requestTimeoutMs?: number;
}
/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Peer selection remains dynamic: the provider reads authenticated FIPS links
 * for every request instead of maintaining a second discovery mesh.
 */
export declare class FipsWorkerP2PProvider implements HashtreeWorkerP2PProvider {
    readonly transport: TcpBlobTransport;
    private readonly node;
    private readonly peers;
    private readonly unsubscribePeer;
    private closed;
    constructor(options: FipsWorkerP2PProviderOptions);
    fetch(hashHex: string, peerId?: string): Promise<Uint8Array | null>;
    listPeerIds(): Promise<string[]>;
    close(): void;
}
export declare function createFipsWorkerP2PProvider(options: FipsWorkerP2PProviderOptions): FipsWorkerP2PProvider;
//# sourceMappingURL=workerProvider.d.ts.map