import type { CID, HashTree } from '@hashtree/core';
import { resolveTreeRootNow } from './treeRootSubscription';

export const DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS = 15_000;

export interface ParsedRootPath {
  treeName: string;
  subPath: string[];
}

function safeDecodePathSegment(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

export function parseRootPath(path?: string): ParsedRootPath {
  const pathParts = path?.split('/').filter(Boolean).map(safeDecodePathSegment) ?? [];
  return {
    treeName: pathParts[0] || 'public',
    subPath: pathParts.slice(1),
  };
}

export async function resolveRootPath(
  tree: Pick<HashTree, 'resolvePath'> | null,
  npub: string,
  path?: string,
  timeoutMs: number = DEFAULT_ROOT_PATH_RESOLVE_TIMEOUT_MS,
): Promise<CID | null> {
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
