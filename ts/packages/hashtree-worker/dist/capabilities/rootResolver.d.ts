import type { CID, HashTree } from '@hashtree/core';
export declare const DEFAULT_ROOT_RESOLVE_TIMEOUT_MS = 15000;
export declare const DEFAULT_ROOT_RESOLVE_SETTLE_MS = 500;
export interface RootWatchHandle {
    initialCid: CID | null;
    close(): Promise<void>;
}
export declare function watchRootPathFromRelays(tree: Pick<HashTree, 'resolvePath'> | null, relays: string[] | undefined, npub: string, path: string | undefined, onUpdate: (cid: CID | null) => void | Promise<void>, timeoutMs?: number, settleMs?: number): Promise<RootWatchHandle>;
export declare function resolveRootPathFromRelays(tree: Pick<HashTree, 'resolvePath'> | null, relays: string[] | undefined, npub: string, path?: string, timeoutMs?: number, settleMs?: number): Promise<CID | null>;
//# sourceMappingURL=rootResolver.d.ts.map