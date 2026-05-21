import type { CID, HashTree } from '@hashtree/core';
export declare const DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS = 15000;
export interface ParsedRootPath {
    treeName: string;
    subPath: string[];
}
export declare function parseRootPath(path?: string): ParsedRootPath;
export declare function resolveRootPath(tree: Pick<HashTree, 'resolvePath'> | null, npub: string, path?: string, timeoutMs?: number): Promise<CID | null>;
//# sourceMappingURL=rootPathResolver.d.ts.map