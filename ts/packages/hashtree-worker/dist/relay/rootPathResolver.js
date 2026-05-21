import { resolveTreeRootNow } from './treeRootSubscription';
export const DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS = 15_000;
function safeDecodePathSegment(segment) {
    try {
        return decodeURIComponent(segment);
    }
    catch {
        return segment;
    }
}
export function parseRootPath(path) {
    const pathParts = path?.split('/').filter(Boolean).map(safeDecodePathSegment) ?? [];
    return {
        treeName: pathParts[0] || 'public',
        subPath: pathParts.slice(1),
    };
}
export async function resolveRootPath(tree, npub, path, timeoutMs = DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS) {
    const { treeName, subPath } = parseRootPath(path);
    const rootCid = await resolveTreeRootNow(npub, treeName, timeoutMs);
    if (!rootCid) {
        return null;
    }
    if (subPath.length === 0) {
        return rootCid;
    }
    if (!tree) {
        throw new Error('Tree not initialized');
    }
    return (await tree.resolvePath(rootCid, subPath))?.cid ?? null;
}
//# sourceMappingURL=rootPathResolver.js.map