import { type FipsDatagramEndpoint } from '@fips/tcp';
import { type Hash, type Store } from '@hashtree/core';
export declare const TCP_BLOB_SERVICE_PORT = 39018;
export declare const TCP_BLOB_MAGIC = 72;
export declare const TCP_BLOB_VERSION = 1;
export declare const TCP_BLOB_DEFAULT_HTL = 10;
export declare const TCP_BLOB_MAX_HTL = 10;
export declare const TCP_BLOB_MAX_BYTES: number;
export interface TcpBlobTransportOptions {
    endpoint: FipsDatagramEndpoint;
    localStore: Store;
    timeoutMs?: number;
}
/** Hash-verified Hashtree blobs carried by one reliable TCP/FIPS stream per request. */
export declare class TcpBlobTransport {
    private readonly options;
    private readonly tcp;
    private readonly timeoutMs;
    private readonly timer;
    private pumping;
    private closed;
    constructor(options: TcpBlobTransportOptions);
    get(hash: Hash, peerIds: readonly string[], htl?: number): Promise<Uint8Array | null>;
    close(): Promise<void>;
    private fetchFromPeer;
    private pump;
    private serve;
    private verifiedGet;
    private waitEstablished;
    private writeAll;
    private readExact;
}
export declare function encodeTcpBlobRequest(hash: Uint8Array, htl?: number): Uint8Array;
export declare function decodeTcpBlobRequest(request: Uint8Array): {
    hash: Hash;
    htl: number;
};
export declare function encodeTcpBlobResponseHeader(found: boolean, size: number): Uint8Array;
export interface TcpBlobResponseHeader {
    found: boolean;
    size: number;
}
export declare function decodeTcpBlobResponseHeader(header: Uint8Array): TcpBlobResponseHeader;
//# sourceMappingURL=tcpBlobTransport.d.ts.map