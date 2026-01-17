/**
 * Tree reading operations
 */

import { Store, Hash, TreeNode, LinkType, toHex, CID, cid } from '../types.js';
import { decodeTreeNode, tryDecodeTreeNode, getNodeType } from '../codec.js';

export interface TreeEntry {
  name: string;
  cid: CID;
  size: number;
  type: LinkType;
  meta?: Record<string, unknown>;
}

/**
 * Get raw data by hash
 */
export async function getBlob(store: Store, hash: Hash): Promise<Uint8Array | null> {
  return store.get(hash);
}

/**
 * Get and decode a tree node
 */
export async function getTreeNode(store: Store, hash: Hash): Promise<TreeNode | null> {
  const data = await store.get(hash);
  if (!data) return null;
  return tryDecodeTreeNode(data);
}

/**
 * Get the type of a chunk by hash: File, Dir, or Blob
 */
export async function getType(store: Store, hash: Hash): Promise<LinkType> {
  if (!hash) return LinkType.Blob;
  const data = await store.get(hash);
  if (!data) return LinkType.Blob;
  return getNodeType(data);
}

/**
 * Check if hash points to a directory (tree with named links)
 */
export async function isDirectory(store: Store, hash: Hash): Promise<boolean> {
  if (!hash) return false;
  const data = await store.get(hash);
  if (!data) return false;
  const node = tryDecodeTreeNode(data);
  return node?.type === LinkType.Dir;
}

/**
 * Read a complete file (reassemble chunks if needed)
 */
export async function readFile(store: Store, hash: Hash): Promise<Uint8Array | null> {
  if (!hash) return null;
  const data = await store.get(hash);
  if (!data) return null;

  const node = tryDecodeTreeNode(data);
  if (!node) {
    return data;
  }

  return assembleChunks(store, node);
}

async function assembleChunks(store: Store, node: TreeNode): Promise<Uint8Array> {
  // Fetch all children in parallel
  const childPromises = node.links.map(async (link) => {
    const childData = await store.get(link.hash);
    if (!childData) {
      throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    }

    if (link.type !== LinkType.Blob) {
      // Intermediate tree node - decode and recurse (parallel recursion)
      const childNode = decodeTreeNode(childData);
      return assembleChunks(store, childNode);
    } else {
      // Leaf chunk - raw blob
      return childData;
    }
  });

  // Wait for all children to complete
  const parts = await Promise.all(childPromises);

  const totalLength = parts.reduce((sum, p) => sum + p.length, 0);
  const result = new Uint8Array(totalLength);
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.length;
  }

  return result;
}

export interface StreamOptions {
  /** Byte offset to start streaming from (default: 0) */
  offset?: number;
  /** Number of chunks to prefetch ahead (default: 1 = no prefetch) */
  prefetch?: number;
}

/**
 * Read a file with streaming
 * Supports prefetching for better network performance
 */
export async function* readFileStream(
  store: Store,
  hash: Hash,
  options: StreamOptions = {}
): AsyncGenerator<Uint8Array> {
  const { offset = 0, prefetch = 1 } = options;

  const data = await store.get(hash);
  if (!data) return;

  const node = tryDecodeTreeNode(data);
  if (!node) {
    if (offset >= data.length) return;
    yield offset > 0 ? data.slice(offset) : data;
    return;
  }

  yield* streamChunksWithOffset(store, node, offset, prefetch);
}

/**
 * Stream chunks starting from an offset with prefetching
 * Uses link.size to efficiently skip chunks before offset
 * Prefetches N chunks ahead for better network performance
 */
async function* streamChunksWithOffset(
  store: Store,
  node: TreeNode,
  offset: number,
  prefetch: number = 1
): AsyncGenerator<Uint8Array> {
  let position = 0;

  // Find first link index that we need (skip those before offset)
  let startIdx = 0;
  for (let i = 0; i < node.links.length; i++) {
    const linkSize = node.links[i].size ?? 0;
    if (linkSize > 0 && position + linkSize <= offset) {
      position += linkSize;
      startIdx = i + 1;
    } else {
      break;
    }
  }

  // Build list of links to process
  const linksToProcess = node.links.slice(startIdx);
  if (linksToProcess.length === 0) return;

  // Prefetch queue: array of { promise, link, position }
  type PrefetchEntry = {
    promise: Promise<Uint8Array | null>;
    link: typeof node.links[0];
    position: number;
  };
  const prefetchQueue: PrefetchEntry[] = [];

  // Start initial prefetch
  let prefetchPosition = position;
  for (let i = 0; i < Math.min(prefetch, linksToProcess.length); i++) {
    const link = linksToProcess[i];
    prefetchQueue.push({
      promise: store.get(link.hash),
      link,
      position: prefetchPosition,
    });
    prefetchPosition += link.size ?? 0;
  }

  // Process links
  let nextPrefetchIdx = prefetch;
  for (const link of linksToProcess) {
    // Get data from prefetch queue (should be first item)
    const entry = prefetchQueue.shift();
    if (!entry) {
      throw new Error('Prefetch queue empty unexpectedly');
    }

    const childData = await entry.promise;
    if (!childData) {
      throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    }

    // Start prefetching next chunk
    if (nextPrefetchIdx < linksToProcess.length) {
      const nextLink = linksToProcess[nextPrefetchIdx];
      prefetchQueue.push({
        promise: store.get(nextLink.hash),
        link: nextLink,
        position: prefetchPosition,
      });
      prefetchPosition += nextLink.size ?? 0;
      nextPrefetchIdx++;
    }

    if (link.type !== LinkType.Blob) {
      // Intermediate tree node - decode and recurse with prefetch
      const childNode = decodeTreeNode(childData);
      const childOffset = Math.max(0, offset - entry.position);
      yield* streamChunksWithOffset(store, childNode, childOffset, prefetch);
      position = entry.position + (link.size ?? 0);
    } else {
      // Leaf chunk - raw blob
      const chunkStart = entry.position;
      const chunkEnd = entry.position + childData.length;
      position = chunkEnd;

      if (chunkEnd <= offset) {
        // Entire chunk is before offset, skip
        continue;
      }

      if (chunkStart >= offset) {
        // Entire chunk is after offset, yield all
        yield childData;
      } else {
        // Partial chunk - slice from offset
        const sliceStart = offset - chunkStart;
        yield childData.slice(sliceStart);
      }
    }
  }
}

/**
 * Read a range of bytes from a file
 */
export async function readFileRange(
  store: Store,
  hash: Hash,
  start: number,
  end?: number
): Promise<Uint8Array | null> {
  const data = await store.get(hash);
  if (!data) return null;

  const node = tryDecodeTreeNode(data);
  if (!node) {
    // Single blob
    if (start >= data.length) return new Uint8Array(0);
    const actualEnd = end !== undefined ? Math.min(end, data.length) : data.length;
    return data.slice(start, actualEnd);
  }

  return readRangeFromNode(store, node, start, end);
}

async function readRangeFromNode(
  store: Store,
  node: TreeNode,
  start: number,
  end?: number
): Promise<Uint8Array> {
  const parts: Uint8Array[] = [];
  let position = 0;
  let bytesCollected = 0;
  const maxBytes = end !== undefined ? end - start : Infinity;

  for (const link of node.links) {
    if (bytesCollected >= maxBytes) break;

    const linkSize = link.size ?? 0;

    // Skip chunks entirely before start
    if (linkSize > 0 && position + linkSize <= start) {
      position += linkSize;
      continue;
    }

    const childData = await store.get(link.hash);
    if (!childData) {
      throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    }

    if (link.type !== LinkType.Blob) {
      // Intermediate tree node - decode and recurse
      const childNode = decodeTreeNode(childData);
      const childStart = Math.max(0, start - position);
      const childEnd = end !== undefined ? end - position : undefined;
      const childData2 = await readRangeFromNode(store, childNode, childStart, childEnd);
      if (childData2.length > 0) {
        const take = Math.min(childData2.length, maxBytes - bytesCollected);
        parts.push(childData2.slice(0, take));
        bytesCollected += take;
      }
      position += linkSize;
    } else {
      // Leaf chunk - raw blob
      const chunkStart = position;
      const chunkEnd = position + childData.length;
      position = chunkEnd;

      if (chunkEnd <= start) {
        continue;
      }

      // Calculate slice bounds within this chunk
      const sliceStart = Math.max(0, start - chunkStart);
      const sliceEnd = end !== undefined
        ? Math.min(childData.length, end - chunkStart)
        : childData.length;

      if (sliceStart < sliceEnd) {
        const take = Math.min(sliceEnd - sliceStart, maxBytes - bytesCollected);
        parts.push(childData.slice(sliceStart, sliceStart + take));
        bytesCollected += take;
      }
    }
  }

  // Concatenate parts
  const totalLength = parts.reduce((sum, p) => sum + p.length, 0);
  const result = new Uint8Array(totalLength);
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.length;
  }

  return result;
}

/**
 * Get directory node, handling chunked directories
 *
 * For chunked directories (encoded blob > chunkSize), the chunks are assembled
 * first, then decoded as a TreeNode.
 */
async function getDirectoryNode(store: Store, hash: Hash): Promise<TreeNode | null> {
  const data = await store.get(hash);
  if (!data) return null;

  const node = tryDecodeTreeNode(data);
  if (!node) {
    return null; // Not a tree node at all
  }

  // Check if it's a directory (has named links) vs chunked blob (no names)
  if (node.type === LinkType.Dir) {
    return node;
  }

  // It's a chunked blob - could be a chunked directory
  // Assemble the chunks and check if result is a directory
  const assembled = await assembleChunks(store, node);

  const assembledNode = tryDecodeTreeNode(assembled);
  if (assembledNode?.type === LinkType.Dir) {
    return assembledNode;
  }

  return null; // Not a directory
}

/**
 * List directory entries
 *
 * Handles both small directories (single node) and large directories
 * (chunked by bytes like files - reassembled then decoded).
 */
export async function listDirectory(store: Store, hash: Hash): Promise<TreeEntry[]> {
  const node = await getDirectoryNode(store, hash);
  if (!node) return [];

  const entries: TreeEntry[] = [];

  for (const link of node.links) {
    if (!link.name) continue; // Skip unnamed links (shouldn't happen in directories)

    entries.push({
      name: link.name,
      cid: cid(link.hash, link.key),
      size: link.size,
      type: link.type,
      meta: link.meta,
    });
  }

  return entries;
}

/**
 * Resolve a path within a tree
 *
 * Handles chunked directories (reassembles bytes to get the full TreeNode).
 */
export async function resolvePath(store: Store, rootHash: Hash, path: string): Promise<Hash | null> {
  const parts = path.split('/').filter(p => p.length > 0);

  let currentHash = rootHash;

  for (const part of parts) {
    const node = await getDirectoryNode(store, currentHash);
    if (!node) return null;

    const link = node.links.find(l => l.name === part);
    if (!link) return null;

    currentHash = link.hash;
  }

  return currentHash;
}

/**
 * Get total size of a tree
 */
export async function getSize(store: Store, hash: Hash): Promise<number> {
  const data = await store.get(hash);
  if (!data) return 0;

  const node = tryDecodeTreeNode(data);
  if (!node) {
    return data.length;
  }

  let total = 0;
  for (const link of node.links) {
    total += link.size;
  }
  return total;
}

/**
 * Walk entire tree depth-first
 *
 * Handles chunked directories (reassembles bytes to get the full TreeNode).
 */
export async function* walk(
  store: Store,
  hash: Hash,
  path: string = ''
): AsyncGenerator<{ path: string; hash: Hash; type: LinkType; size?: number }> {
  const data = await store.get(hash);
  if (!data) return;

  // Check if it's a directory
  const dirNode = await getDirectoryNode(store, hash);
  if (dirNode) {
    const dirSize = dirNode.links.reduce((sum, l) => sum + l.size, 0);
    yield { path, hash, type: LinkType.Dir, size: dirSize };

    for (const link of dirNode.links) {
      if (!link.name) continue;

      const childPath = path ? `${path}/${link.name}` : link.name;
      yield* walk(store, link.hash, childPath);
    }
    return;
  }

  // Not a directory - could be a file (single blob or chunked)
  const node = tryDecodeTreeNode(data);
  if (!node) {
    // Single blob file
    yield { path, hash, type: LinkType.Blob, size: data.length };
    return;
  }

  // Chunked file - sum link sizes
  const fileSize = node.links.reduce((sum, l) => sum + l.size, 0);
  yield { path, hash, type: LinkType.File, size: fileSize };
}
