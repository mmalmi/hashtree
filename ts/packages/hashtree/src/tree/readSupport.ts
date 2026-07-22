import { LinkType, type Link, type TreeNode } from '../types.js';

export interface ReadState {
  maxBytes?: number;
  bytesRead: number;
}

export function normalizeMaxBytes(maxBytes?: number): number | undefined {
  if (maxBytes === undefined) return undefined;
  if (!Number.isFinite(maxBytes) || maxBytes < 0) {
    throw new Error(`Invalid maxBytes: ${maxBytes}`);
  }
  return Math.floor(maxBytes);
}

export function ensureWithinLimit(maxBytes: number | undefined, actualBytes: number): void {
  if (maxBytes !== undefined && actualBytes > maxBytes) {
    throw new Error(`Content size ${actualBytes} exceeds maxBytes ${maxBytes}`);
  }
}

export function isDirectoryLikeNode(node: TreeNode | null): node is TreeNode {
  return node?.type === LinkType.Dir || node?.type === LinkType.Fanout;
}

function internalChunkStart(name: string): number | null {
  const prefix = '_chunk_';
  if (!name.startsWith(prefix)) return null;

  const suffix = name.slice(prefix.length);
  if (suffix.length === 0 || !/^[0-9]+$/.test(suffix)) return null;

  const start = Number(suffix);
  return Number.isSafeInteger(start) ? start : null;
}

function nodeUsesLegacyDirectoryFanout(node: TreeNode): boolean {
  return node.type === LinkType.Dir
    && node.links.length > 0
    && node.links.every((link) => (
      link.type === LinkType.Dir
      && link.name !== undefined
      && internalChunkStart(link.name) !== null
    ));
}

export function isInternalDirectoryLink(node: TreeNode, link: Link): boolean {
  if (node.type === LinkType.Fanout) {
    return link.type === LinkType.Dir || link.type === LinkType.Fanout;
  }

  return nodeUsesLegacyDirectoryFanout(node)
    && link.type === LinkType.Dir
    && link.name !== undefined
    && internalChunkStart(link.name) !== null;
}
