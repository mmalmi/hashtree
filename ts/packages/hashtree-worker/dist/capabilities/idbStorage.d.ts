export interface StorageStats {
    items: number;
    bytes: number;
    maxBytes: number;
}
type PeerShareRevocationHandler = (hashHexes: readonly string[]) => void;
export declare class IdbBlobStorage {
    private readonly store;
    private readonly peerShare;
    private readonly onPeerShareRevoked;
    private maxBytes;
    private writesSinceEviction;
    private evictionPromise;
    private static readonly EVICTION_WRITE_INTERVAL;
    constructor(dbName: string, maxBytes: number, onPeerShareRevoked?: PeerShareRevocationHandler);
    setMaxBytes(maxBytes: number): void;
    getMaxBytes(): number;
    put(data: Uint8Array): Promise<string>;
    putByHash(hashHex: string, data: Uint8Array): Promise<void>;
    putByHashTrusted(hashHex: string, data: Uint8Array): Promise<void>;
    get(hashHex: string): Promise<Uint8Array | null>;
    has(hashHex: string): Promise<boolean>;
    delete(hashHex: string): Promise<boolean>;
    authorizePeerSharing(hashHexes: Iterable<string>): Promise<void>;
    loadPeerShareAuthorizations(): Promise<string[]>;
    getStats(): Promise<StorageStats>;
    close(): void;
    private revokePeerSharing;
    private scheduleEviction;
}
export {};
//# sourceMappingURL=idbStorage.d.ts.map