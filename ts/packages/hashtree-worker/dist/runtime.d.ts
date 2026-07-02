export interface HtreeRuntimeLocationLike {
    protocol?: string;
    hostname?: string;
    search?: string;
}
export interface HtreeRuntimeWindowLike {
    location?: HtreeRuntimeLocationLike;
    __HTREE_SERVER_URL__?: string;
    __HTREE_CANONICAL_URL__?: string | null;
    htree?: {
        htreeBaseUrl?: string;
    };
}
export interface ResolveRuntimeHtreeBaseUrlOptions {
    windowLike?: HtreeRuntimeWindowLike;
    fallbackBaseUrl?: string | null;
}
export declare function getInjectedHtreeServerUrl(windowLike?: HtreeRuntimeWindowLike): string | null;
export declare function shouldEagerLoadMediaInNativeChildRuntime(windowLike?: HtreeRuntimeWindowLike): boolean;
export declare function canUseLocalHtreeRoutes(windowLike?: HtreeRuntimeWindowLike): boolean;
export declare function shouldPreferSameOriginHtreeRoutes(windowLike?: HtreeRuntimeWindowLike): boolean;
export declare function canUseInjectedHtreeServerUrl(windowLike?: HtreeRuntimeWindowLike): boolean;
export declare function canUseSameOriginHtreeProtocolStreaming(windowLike?: HtreeRuntimeWindowLike): boolean;
export declare function resolveRuntimeHtreeBaseUrl(options?: ResolveRuntimeHtreeBaseUrlOptions): string;
//# sourceMappingURL=runtime.d.ts.map