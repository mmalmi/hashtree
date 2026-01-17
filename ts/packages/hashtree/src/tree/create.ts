/**
 * Tree creation operations
 */

import { Store, Hash, TreeNode, Link, LinkType, CID } from '../types.js';
import { sha256 } from '../hash.js';
import { encodeAndHash } from '../codec.js';

export interface CreateConfig {
  store: Store;
  chunkSize: number;
}

export interface DirEntry {
  name: string;
  cid: CID;
  size: number;
  type: LinkType;
  meta?: Record<string, unknown>;
}

/**
 * Store a blob directly (small data)
 */
export async function putBlob(store: Store, data: Uint8Array): Promise<Hash> {
  const hash = await sha256(data);
  await store.put(hash, data);
  return hash;
}

/**
 * Store a file, chunking if necessary
 */
export async function putFile(
  config: CreateConfig,
  data: Uint8Array
): Promise<{ hash: Hash; size: number }> {
  const { store, chunkSize } = config;
  const size = data.length;

  if (data.length <= chunkSize) {
    const hash = await putBlob(store, data);
    return { hash, size };
  }

  // Process chunks sequentially to avoid memory spikes
  // (For parallel processing of large files, use StreamWriter instead)
  const links: Link[] = [];
  let offset = 0;
  while (offset < data.length) {
    const end = Math.min(offset + chunkSize, data.length);
    // Use subarray to avoid copying
    const chunk = data.subarray(offset, end);
    const hash = await putBlob(store, chunk);
    links.push({
      hash,
      size: chunk.length,
      type: LinkType.Blob,
    });
    offset = end;
  }

  const rootHash = await buildTree(config, links, size);
  return { hash: rootHash, size };
}

/**
 * Build a directory from entries
 *
 * Directories are encoded as MessagePack blobs. If the encoded blob exceeds
 * chunkSize, it's chunked by bytes like files using putFile.
 */
export async function putDirectory(
  config: CreateConfig,
  entries: DirEntry[]
): Promise<Hash> {
  const { store, chunkSize } = config;
  const sorted = [...entries].sort((a, b) => a.name.localeCompare(b.name));

  const links: Link[] = sorted.map(e => ({
    hash: e.cid.hash,
    key: e.cid.key,
    name: e.name,
    size: e.size,
    type: e.type,
    meta: e.meta,
  }));

  const node: TreeNode = {
    type: LinkType.Dir,
    links,
  };
  const { data, hash } = await encodeAndHash(node);

  // Small directory - store directly
  if (data.length <= chunkSize) {
    await store.put(hash, data);
    return hash;
  }

  // Large directory - reuse putFile for chunking
  const { hash: rootHash } = await putFile(config, data);
  return rootHash;
}

export async function buildTree(
  config: CreateConfig,
  links: Link[],
  totalSize?: number
): Promise<Hash> {
  const { store } = config;

  // Single chunk that matches total size - return it directly
  if (links.length === 1 && links[0].size === totalSize) {
    return links[0].hash;
  }

  // Create single flat node with all links
  const node: TreeNode = {
    type: LinkType.File,
    links,
  };
  const { data, hash } = await encodeAndHash(node);
  await store.put(hash, data);
  return hash;
}

