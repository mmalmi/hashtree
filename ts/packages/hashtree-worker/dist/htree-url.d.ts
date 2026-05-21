import { type HtreeRuntimeWindowLike } from './runtime.js';
export type ParsedHtreeUrl = {
    kind: 'mutable';
    npub: string;
    treeName: string;
    path: string;
} | {
    kind: 'immutable';
    nhash: string;
    path: string;
};
export type MutableHtreeRequestStyle = 'htree' | 'gateway';
export interface ResolveHtreeRequestUrlOptions {
    windowLike?: HtreeRuntimeWindowLike;
    fallbackBaseUrl?: string | null;
    baseUrl?: string | null;
    mutableStyle?: MutableHtreeRequestStyle;
}
export declare function parseHtreeUrl(input: string): ParsedHtreeUrl | null;
export declare function buildHtreeRequestPath(input: string | ParsedHtreeUrl, mutableStyle?: MutableHtreeRequestStyle): string | null;
export declare function resolveHtreeRequestUrl(input: string | ParsedHtreeUrl, options?: ResolveHtreeRequestUrlOptions): string;
//# sourceMappingURL=htree-url.d.ts.map