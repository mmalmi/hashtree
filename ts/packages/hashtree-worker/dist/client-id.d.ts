export interface HtreeClientIdStorageLike {
    getItem(key: string): string | null;
    setItem(key: string, value: string): void;
}
export interface ResolveHtreeClientIdOptions {
    storageKey?: string;
    prefix?: string;
    storage?: HtreeClientIdStorageLike | null;
    uuidFactory?: () => string;
}
export interface AppendHtreeQueryParamOptions {
    baseOrigin?: string | null;
}
export declare function createHtreeClientId(prefix?: string, uuidFactory?: () => string): string;
export declare function getOrCreateHtreeClientId(options?: ResolveHtreeClientIdOptions): string | null;
export declare function appendHtreeQueryParam(url: string, key: string, value: string | null | undefined, options?: AppendHtreeQueryParamOptions): string;
export declare function appendHtreeClientId(url: string, clientId: string | null | undefined, options?: AppendHtreeQueryParamOptions): string;
//# sourceMappingURL=client-id.d.ts.map