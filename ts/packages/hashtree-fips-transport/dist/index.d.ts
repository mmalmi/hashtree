import { type Hash, type Store } from '@hashtree/core';
export { DEFAULT_FIPS_DISCOVERY_APP } from './constants.js';
export { TcpBlobTransport, type TcpBlobTransportOptions } from './tcpBlobTransport.js';
export declare const DEFAULT_FIPS_REQUEST_TIMEOUT_MS = 5500;
export declare const DEFAULT_FIPS_REQUEST_RETRY_INTERVAL_MS = 750;
export declare const DEFAULT_FIPS_REQUEST_MAX_ATTEMPTS = 4;
export declare const FIPS_RESPONSE_FRAGMENT_SIZE = 1024;
export interface FipsEndpointMessage {
    peerId: string;
    data: Uint8Array;
}
export interface FipsEndpoint {
    send(peerId: string, data: Uint8Array): Promise<void>;
    onMessage(handler: (message: FipsEndpointMessage) => void | Promise<void>): () => void;
    listPeerIds?(): readonly string[] | Promise<readonly string[]>;
    localPeerId?(): string;
    close?(): void;
}
export interface FipsNodeEndpointData {
    src: string;
    dst: string;
    payload: Uint8Array;
}
export interface FipsNodePeerEvent {
    remotePubkey: string;
    state: 'connected' | 'disconnected';
}
export interface FipsNodeLike {
    identity?: {
        publicKey?: Uint8Array;
    };
    sendEndpointData(args: {
        dst: string;
        payload: Uint8Array;
    }): Promise<void>;
    on(event: 'endpointData', handler: (event: FipsNodeEndpointData) => void): () => void;
    on(event: 'peer', handler: (event: FipsNodePeerEvent) => void): () => void;
}
export interface FipsNodeEndpointOptions {
    initialPeers?: readonly string[];
}
export type FipsPeerSource = readonly string[] | (() => readonly string[] | Promise<readonly string[]>);
export interface HashtreeFipsTransportOptions {
    endpoint: FipsEndpoint;
    localStore?: Store;
    peers?: FipsPeerSource;
    requestTimeoutMs?: number;
    requestRetryIntervalMs?: number;
    requestMaxAttempts?: number;
    requestHtl?: number;
    cacheResponses?: boolean;
}
export interface FipsReadSource {
    id: string;
    get(hash: Hash): Promise<Uint8Array | null>;
    isAvailable?: () => boolean;
}
export declare function createFipsNodeEndpoint(node: FipsNodeLike, options?: FipsNodeEndpointOptions): FipsEndpoint;
export declare class HashtreeFipsTransport {
    private readonly endpoint;
    private readonly localStore;
    private peers?;
    private readonly requestTimeoutMs;
    private readonly requestRetryIntervalMs;
    private readonly requestMaxAttempts;
    private readonly requestHtl;
    private readonly cacheResponses;
    private readonly pending;
    private readonly responseFragments;
    private readonly outgoingResponses;
    private responseSendTail;
    private unsubscribe;
    private closed;
    constructor(options: HashtreeFipsTransportOptions);
    close(): void;
    setPeers(peers: FipsPeerSource): void;
    get(hash: Hash, peers?: FipsPeerSource): Promise<Uint8Array | null>;
    put(hash: Hash, data: Uint8Array): Promise<boolean>;
    handleMessage(message: FipsEndpointMessage): Promise<void>;
    createReadSource(id?: string): FipsReadSource;
    private handleRequest;
    private enqueueResponse;
    private handleResponse;
    private sendResponse;
    private handleResponseFragment;
    private requestFromPeers;
    private requestFromDynamicPeers;
    private sendRequestAttempts;
    private waitForNextAttempt;
    private createPendingRequest;
    private removePendingRequest;
    private sendRequestToPeers;
    private resolvePendingMiss;
}
export interface FipsTransportStoreOptions extends HashtreeFipsTransportOptions {
    localStore: Store;
}
export declare class FipsTransportStore implements Store {
    readonly transport: HashtreeFipsTransport;
    private readonly localStore;
    constructor(options: FipsTransportStoreOptions);
    close(): void;
    put(hash: Hash, data: Uint8Array): Promise<boolean>;
    get(hash: Hash): Promise<Uint8Array | null>;
    has(hash: Hash): Promise<boolean>;
    delete(hash: Hash): Promise<boolean>;
    watch(hash: Hash, callback: (data: Uint8Array) => void): () => void;
}
//# sourceMappingURL=index.d.ts.map