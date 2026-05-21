/**
 * Media Streaming Handler for Hashtree Worker
 *
 * Handles media requests from the service worker via MessagePort.
 * Supports both direct CID-based requests and path-based requests with live streaming.
 */
import type { HashTree, CID } from '@hashtree/core';
/**
 * SW FileRequest format (from service worker)
 */
interface SwFileRequest {
    type: 'hashtree-file';
    requestId: string;
    npub?: string;
    nhash?: string;
    snapshot?: boolean;
    linkKey?: string | null;
    treeName?: string;
    path: string;
    start: number;
    end?: number;
    rangeHeader?: string | null;
    mimeType: string;
    download?: boolean;
}
interface ResolvedRootEntry {
    cid: CID;
    size?: number;
    path?: string;
}
/**
 * Initialize the media handler with the HashTree instance
 */
export declare function initMediaHandler(hashTree: HashTree): void;
/**
 * Register a MessagePort from the service worker for media streaming
 */
export declare function registerMediaPort(port: MessagePort, debug?: boolean): void;
/**
 * Handle file request from service worker (hashtree-file format)
 * This is the main entry point for direct SW → Worker communication
 */
declare function handleSwFileRequest(req: SwFileRequest): Promise<void>;
declare function waitForCachedRoot(npub: string, treeName: string): Promise<CID | null>;
declare function resolveMutableTreeEntry(npub: string, treeName: string, path: string, options?: {
    allowSingleSegmentRootFallback?: boolean;
    expectedMimeType?: string;
}): Promise<ResolvedRootEntry | null>;
declare function resolveCidWithinRoot(rootCid: CID, path: string, options?: {
    allowSingleSegmentRootFallback?: boolean;
    expectedMimeType?: string;
}): Promise<CID | null>;
declare function normalizeAliasPath(rootCid: CID, path: string): Promise<string>;
declare function canListDirectory(rootCid: CID): Promise<boolean>;
export declare const __test__: {
    handleSwFileRequest: typeof handleSwFileRequest;
    resolveCidWithinRoot: typeof resolveCidWithinRoot;
    resolveMutableTreeEntry: typeof resolveMutableTreeEntry;
    normalizeAliasPath: typeof normalizeAliasPath;
    canListDirectory: typeof canListDirectory;
    waitForCachedRoot: typeof waitForCachedRoot;
};
export {};
//# sourceMappingURL=mediaHandler.d.ts.map