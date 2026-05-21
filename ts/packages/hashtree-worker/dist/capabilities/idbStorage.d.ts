export interface StorageStats {
    items: number;
    bytes: number;
    maxBytes: number;
}
export declare class IdbBlobStorage {
    private readonly store;
    private maxBytes;
    private writesSinceEviction;
    private evictionPromise;
    private static readonly EVICTION_WRITE_INTERVAL;
    constructor(dbName: string, maxBytes: number);
    setMaxBytes(maxBytes: number): void;
    getMaxBytes(): number;
    put(data: Uint8Array): Promise<string>;
    putByHash(hashHex: string, data: Uint8Array): Promise<void>;
    putByHashTrusted(hashHex: string, data: Uint8Array): Promise<void>;
    get(hashHex: string): Promise<Uint8Array | null>;
    has(hashHex: string): Promise<boolean>;
    delete(hashHex: string): Promise<boolean>;
    getStats(): Promise<StorageStats>;
    close(): void;
    private scheduleEviction;
}
//# sourceMappingURL=idbStorage.d.ts.map