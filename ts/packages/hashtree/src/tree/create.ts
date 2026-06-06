/**
 * Tree creation operations
 */

import { Store, Hash, TreeNode, Link, LinkType, CID } from '../types.js';
import { sha256 } from '../hash.js';
import { encodeAndHash } from '../codec.js';
import { compareNames } from '../compare.js';

const DEFAULT_MAX_LINKS = 174;

export interface CreateConfig {
  store: Store;
  chunkSize: number;
  maxLinks?: number;
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
 * Directories with more than maxLinks entries are split into canonical
 * _chunk_<start> fanout directory nodes.
 */
export async function putDirectory(
  config: CreateConfig,
  entries: DirEntry[]
): Promise<Hash> {
  const sorted = [...entries].sort((a, b) => compareNames(a.name, b.name));

  const links: Link[] = sorted.map(e => ({
    hash: e.cid.hash,
    key: e.cid.key,
    name: e.name,
    size: e.size ?? 0,
    type: e.type ?? LinkType.Blob,
    meta: e.meta,
  }));

  if (links.length <= getMaxLinks(config)) {
    return putDirectoryNode(config, links);
  }

  return buildDirectoryByChunks(config, links);
}

async function putDirectoryNode(
  config: CreateConfig,
  links: Link[]
): Promise<Hash> {
  const node: TreeNode = {
    type: LinkType.Dir,
    links,
  };
  const { data, hash } = await encodeAndHash(node);
  await config.store.put(hash, data);
  return hash;
}

type IndexedLink = {
  start: number;
  link: Link;
};

async function buildDirectoryByChunks(
  config: CreateConfig,
  links: Link[]
): Promise<Hash> {
  const indexedLinks = links.map((link, start) => ({ start, link }));
  return buildIndexedDirectoryChunks(config, indexedLinks);
}

async function buildIndexedDirectoryChunks(
  config: CreateConfig,
  links: IndexedLink[]
): Promise<Hash> {
  const maxLinks = getMaxLinks(config);
  const subTrees: IndexedLink[] = [];

  for (let offset = 0; offset < links.length; offset += maxLinks) {
    const batch = links.slice(offset, offset + maxLinks);
    const start = batch[0]?.start ?? offset;
    const batchLinks = batch.map(({ link }) => link);
    const batchSize = batchLinks.reduce((sum, link) => sum + (link.size ?? 0), 0);
    const hash = await putDirectoryNode(config, batchLinks);

    subTrees.push({
      start,
      link: {
        hash,
        name: `_chunk_${start}`,
        size: batchSize,
        type: LinkType.Dir,
      },
    });
  }

  if (subTrees.length <= maxLinks) {
    return putDirectoryNode(config, subTrees.map(({ link }) => link));
  }

  return buildIndexedDirectoryChunks(config, subTrees);
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

function getMaxLinks(config: CreateConfig): number {
  const maxLinks = config.maxLinks ?? DEFAULT_MAX_LINKS;
  if (!Number.isInteger(maxLinks) || maxLinks < 1) {
    throw new Error(`Invalid maxLinks: ${config.maxLinks}`);
  }
  return maxLinks;
}
