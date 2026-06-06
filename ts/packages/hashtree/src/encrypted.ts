/**
 * CHK (Content Hash Key) encrypted file operations for HashTree
 *
 * Everything uses CHK encryption:
 * - Chunks: key = SHA256(plaintext)
 * - Tree nodes: key = SHA256(msgpack_encoded_node)
 *
 * Same content → same ciphertext → deduplication works at all levels.
 * The root key is deterministic: same file = same CID (hash + key).
 */

import { Store, Hash, TreeNode, Link, LinkType, toHex } from './types.js';
import { sha256 } from './hash.js';
import { encodeAndHash, decodeTreeNode, tryDecodeTreeNode } from './codec.js';
import { encryptChk, decryptChk, type EncryptionKey } from './crypto.js';
import { compareNames } from './compare.js';
import { loadBlock } from './tree/loadBlock.js';

const DEFAULT_MAX_LINKS = 174;

export interface EncryptedTreeConfig {
  store: Store;
  chunkSize: number;
  maxLinks?: number;
}

export interface ReadEncryptedOptions {
  /** Maximum plaintext bytes to return before throwing */
  maxBytes?: number;
}

interface ReadState {
  maxBytes?: number;
  bytesRead: number;
}

async function getRootNodeOrBlob(
  store: Store,
  hash: Hash,
  key: EncryptionKey
): Promise<Uint8Array | null> {
  const encryptedData = await store.get(hash);
  if (!encryptedData) return null;
  return decryptRootNodeOrBlob(encryptedData, key);
}

/**
 * Like getRootNodeOrBlob but waits for the block to arrive instead of
 * returning null. Used for directory reads, where a not-yet-synced block
 * is a transient state we don't want to surface as "missing".
 */
async function loadRootNodeOrBlob(
  store: Store,
  hash: Hash,
  key: EncryptionKey,
  signal?: AbortSignal
): Promise<Uint8Array> {
  const encryptedData = await loadBlock(store, hash, signal);
  return decryptRootNodeOrBlob(encryptedData, key);
}

async function decryptRootNodeOrBlob(
  encryptedData: Uint8Array,
  key: EncryptionKey
): Promise<Uint8Array> {
  const rawNode = tryDecodeTreeNode(encryptedData);
  try {
    const decrypted = await decryptChk(encryptedData, key);
    return tryDecodeTreeNode(decrypted) || !rawNode ? decrypted : encryptedData;
  } catch (error) {
    if (rawNode) {
      return encryptedData;
    }
    throw error;
  }
}

function normalizeMaxBytes(maxBytes?: number): number | undefined {
  if (maxBytes === undefined) return undefined;
  if (!Number.isFinite(maxBytes) || maxBytes < 0) {
    throw new Error(`Invalid maxBytes: ${maxBytes}`);
  }
  return Math.floor(maxBytes);
}

function ensureWithinLimit(maxBytes: number | undefined, actualBytes: number): void {
  if (maxBytes !== undefined && actualBytes > maxBytes) {
    throw new Error(`Content size ${actualBytes} exceeds maxBytes ${maxBytes}`);
  }
}

/**
 * Result of encrypted file storage
 */
export interface EncryptedPutResult {
  /** Root hash of encrypted tree */
  hash: Hash;
  /** Original plaintext size */
  size: number;
  /** Encryption key for the root (content hash for CHK) */
  key: EncryptionKey;
}

/**
 * Store a file with CHK encryption
 *
 * Everything is CHK encrypted - deterministic, enables full deduplication.
 * Returns hash + key, both derived from content.
 *
 * @param config - Tree configuration
 * @param data - File data to encrypt and store
 * @returns Hash of encrypted root and the encryption key (content hash)
 */
export async function putFileEncrypted(
  config: EncryptedTreeConfig,
  data: Uint8Array
): Promise<EncryptedPutResult> {
  const { store, chunkSize } = config;
  const size = data.length;

  // Single chunk - use CHK directly
  if (data.length <= chunkSize) {
    const { ciphertext, key } = await encryptChk(data);
    const hash = await sha256(ciphertext);
    await store.put(hash, ciphertext);
    return { hash, size, key };
  }

  // Process chunks sequentially to avoid memory spikes
  // (For parallel processing of large files, use StreamWriter instead)
  const links: Link[] = [];
  let offset = 0;
  while (offset < data.length) {
    const end = Math.min(offset + chunkSize, data.length);
    // Use subarray to avoid copying, encrypt will handle the data
    const chunk = data.subarray(offset, end);
    const { ciphertext, key: chunkKey } = await encryptChk(chunk);
    const hash = await sha256(ciphertext);
    await store.put(hash, ciphertext);
    links.push({
      hash,
      // Store PLAINTEXT size in link for correct range seeking
      size: chunk.length,
      key: chunkKey,
      type: LinkType.Blob,
    });
    offset = end;
  }

  // Build tree - tree nodes also CHK encrypted
  const { hash: rootHash, key: rootKey } = await buildEncryptedTree(config, links, size);

  return { hash: rootHash, size, key: rootKey };
}

/**
 * Build tree structure with CHK-encrypted tree nodes
 * Returns hash and key for the root node
 */
async function buildEncryptedTree(
  config: EncryptedTreeConfig,
  links: Link[],
  totalSize: number | undefined
): Promise<{ hash: Hash; key: EncryptionKey }> {
  const { store } = config;

  // Single link - return its hash and key directly
  if (links.length === 1 && links[0].key) {
    if (totalSize !== undefined && links[0].size === totalSize) {
      return { hash: links[0].hash, key: links[0].key };
    }
  }

  // Create single flat node with all links
  const node: TreeNode = {
    type: LinkType.File,
    links,
  };
  const { data } = await encodeAndHash(node);
  // CHK encrypt the tree node
  const { ciphertext, key: nodeKey } = await encryptChk(data);
  const hash = await sha256(ciphertext);
  await store.put(hash, ciphertext);
  return { hash, key: nodeKey };
}

/**
 * Read an encrypted file
 *
 * Key is always the CHK key (content hash of plaintext)
 *
 * @param store - Storage backend
 * @param hash - Root hash of encrypted file
 * @param key - CHK decryption key (content hash)
 * @returns Decrypted file data
 */
export async function readFileEncrypted(
  store: Store,
  hash: Hash,
  key: EncryptionKey,
  options: ReadEncryptedOptions = {}
): Promise<Uint8Array | null> {
  const maxBytes = normalizeMaxBytes(options.maxBytes);
  const decrypted = await getRootNodeOrBlob(store, hash, key);
  if (!decrypted) return null;

  // Check if it's a tree node
  const node = tryDecodeTreeNode(decrypted);
  if (node) {
    const declaredSize = node.links.reduce((sum, link) => sum + (link.size ?? 0), 0);
    ensureWithinLimit(maxBytes, declaredSize);
    return assembleEncryptedChunks(store, node, { maxBytes, bytesRead: 0 });
  }

  // Single chunk data
  ensureWithinLimit(maxBytes, decrypted.length);
  return decrypted;
}


/**
 * Get directory node from encrypted storage, handling chunked directories
 */
async function getEncryptedDirectoryNode(
  store: Store,
  hash: Hash,
  key: EncryptionKey,
  signal?: AbortSignal
): Promise<TreeNode | null> {
  const decrypted = await loadRootNodeOrBlob(store, hash, key, signal);

  // Check if it's directly a directory (small directory)
  const node = tryDecodeTreeNode(decrypted);
  if (node?.type === LinkType.Dir) {
    return node;
  }

  // It's a chunk tree - could be a chunked directory
  if (node) {
    const assembled = await assembleEncryptedChunks(store, node);

    // Check if assembled result is a directory
    const assembledNode = tryDecodeTreeNode(assembled);
    if (assembledNode?.type === LinkType.Dir) {
      return assembledNode;
    }
  }

  return null; // Not a directory
}

/**
 * Assemble chunks from an encrypted tree.
 * Each link has its own CHK key.
 */
async function assembleEncryptedChunks(
  store: Store,
  node: TreeNode,
  state: ReadState = { bytesRead: 0 }
): Promise<Uint8Array> {
  const parts: Uint8Array[] = [];

  for (const link of node.links) {
    ensureWithinLimit(state.maxBytes, state.bytesRead + (link.size ?? 0));

    const chunkKey = link.key;
    if (!chunkKey) {
      throw new Error(`Missing decryption key for chunk: ${toHex(link.hash)}`);
    }

    const encryptedChild = await store.get(link.hash);
    if (!encryptedChild) {
      throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    }

    const decrypted = await decryptChk(encryptedChild, chunkKey);

    if (link.type !== LinkType.Blob) {
      // Intermediate tree node - decode and recurse
      const childNode = decodeTreeNode(decrypted);
      parts.push(await assembleEncryptedChunks(store, childNode, state));
    } else {
      // Leaf data chunk - raw blob
      ensureWithinLimit(state.maxBytes, state.bytesRead + decrypted.length);
      state.bytesRead += decrypted.length;
      parts.push(decrypted);
    }
  }

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
 * Stream an encrypted file
 * @param store - Storage backend
 * @param hash - Root hash of encrypted file
 * @param key - CHK decryption key (content hash)
 * @param options - Streaming options
 */
export async function* readFileEncryptedStream(
  store: Store,
  hash: Hash,
  key: EncryptionKey,
  options: StreamOptions = {}
): AsyncGenerator<Uint8Array> {
  const { offset = 0, prefetch = 1 } = options;

  const decrypted = await getRootNodeOrBlob(store, hash, key);
  if (!decrypted) return;

  const node = tryDecodeTreeNode(decrypted);
  if (node) {
    yield* streamEncryptedChunksWithOffset(store, node, offset, prefetch);
  } else {
    // Single blob (small file)
    if (offset >= decrypted.length) return;
    yield offset > 0 ? decrypted.slice(offset) : decrypted;
  }
}

/**
 * Stream encrypted chunks starting from an offset with prefetching
 */
async function* streamEncryptedChunksWithOffset(
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
    const chunkKey = link.key;
    if (!chunkKey) {
      throw new Error(`Missing decryption key for chunk: ${toHex(link.hash)}`);
    }

    // Get data from prefetch queue (should be first item)
    const entry = prefetchQueue.shift();
    if (!entry) {
      throw new Error('Prefetch queue empty unexpectedly');
    }

    const encryptedChild = await entry.promise;
    if (!encryptedChild) {
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

    const decrypted = await decryptChk(encryptedChild, chunkKey);

    if (link.type !== LinkType.Blob) {
      // Intermediate tree node - decode and recurse with prefetch
      const childNode = decodeTreeNode(decrypted);
      const childOffset = Math.max(0, offset - entry.position);
      yield* streamEncryptedChunksWithOffset(store, childNode, childOffset, prefetch);
      position = entry.position + (link.size ?? 0);
    } else {
      // Leaf chunk - raw blob
      const chunkStart = entry.position;
      const chunkEnd = entry.position + decrypted.length;
      position = chunkEnd;

      if (chunkEnd <= offset) {
        continue;
      }

      if (chunkStart >= offset) {
        yield decrypted;
      } else {
        const sliceStart = offset - chunkStart;
        yield decrypted.slice(sliceStart);
      }
    }
  }
}

/**
 * Read a range of bytes from an encrypted file
 */
export async function readFileEncryptedRange(
  store: Store,
  hash: Hash,
  key: EncryptionKey,
  start: number,
  end?: number
): Promise<Uint8Array | null> {
  const decrypted = await getRootNodeOrBlob(store, hash, key);
  if (!decrypted) return null;

  const node = tryDecodeTreeNode(decrypted);
  if (!node) {
    // Single blob
    if (start >= decrypted.length) return new Uint8Array(0);
    const actualEnd = end !== undefined ? Math.min(end, decrypted.length) : decrypted.length;
    return decrypted.slice(start, actualEnd);
  }

  return readEncryptedRangeFromNode(store, node, start, end);
}

async function readEncryptedRangeFromNode(
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

    const chunkKey = link.key;
    if (!chunkKey) {
      throw new Error(`Missing decryption key for chunk: ${toHex(link.hash)}`);
    }

    const linkSize = link.size ?? 0;

    // Skip chunks entirely before start
    if (linkSize > 0 && position + linkSize <= start) {
      position += linkSize;
      continue;
    }

    const encryptedChild = await store.get(link.hash);
    if (!encryptedChild) {
      throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    }

    const decrypted = await decryptChk(encryptedChild, chunkKey);

    if (link.type !== LinkType.Blob) {
      // Intermediate tree node - decode and recurse
      const childNode = decodeTreeNode(decrypted);
      const childStart = Math.max(0, start - position);
      const childEnd = end !== undefined ? end - position : undefined;
      const childData = await readEncryptedRangeFromNode(store, childNode, childStart, childEnd);
      if (childData.length > 0) {
        const take = Math.min(childData.length, maxBytes - bytesCollected);
        parts.push(childData.slice(0, take));
        bytesCollected += take;
      }
      position += linkSize;
    } else {
      // Leaf chunk - raw blob
      const chunkStart = position;
      const chunkEnd = position + decrypted.length;
      position = chunkEnd;

      if (chunkEnd <= start) {
        continue;
      }

      const sliceStart = Math.max(0, start - chunkStart);
      const sliceEnd = end !== undefined
        ? Math.min(decrypted.length, end - chunkStart)
        : decrypted.length;

      if (sliceStart < sliceEnd) {
        const take = Math.min(sliceEnd - sliceStart, maxBytes - bytesCollected);
        parts.push(decrypted.slice(sliceStart, sliceStart + take));
        bytesCollected += take;
      }
    }
  }

  // Concatenate parts
  const totalLength = parts.reduce((sum, p) => sum + p.length, 0);
  const result = new Uint8Array(totalLength);
  let concatOffset = 0;
  for (const part of parts) {
    result.set(part, concatOffset);
    concatOffset += part.length;
  }

  return result;
}

/**
 * Directory entry with optional encryption key
 */
export interface EncryptedDirEntry {
  name: string;
  hash: Hash;
  size: number;
  /** CHK key for encrypted children */
  key?: Uint8Array;
  /** Type of this entry: Blob, File, or Dir */
  type: LinkType;
  /** Optional metadata (createdAt, mimeType, thumbnail, etc.) */
  meta?: Record<string, unknown>;
}

/**
 * Store a directory with CHK encryption
 *
 * The directory node itself is encrypted. Child entries already have their own keys.
 * Large directories are split into canonical _chunk_<start> fanout directory nodes.
 *
 * @param config - Tree configuration
 * @param entries - Directory entries (with keys for encrypted children)
 * @returns Hash of encrypted directory and the encryption key
 */
export async function putDirectoryEncrypted(
  config: EncryptedTreeConfig,
  entries: EncryptedDirEntry[]
): Promise<EncryptedPutResult> {
  const sorted = [...entries].sort((a, b) => compareNames(a.name, b.name));

  const links: Link[] = sorted.map(e => ({
    hash: e.hash,
    name: e.name,
    size: e.size ?? 0,
    key: e.key,
    type: e.type ?? LinkType.Blob,
    meta: e.meta,
  }));

  if (links.length <= getMaxLinks(config)) {
    return putEncryptedDirectoryNode(config, links);
  }

  return buildEncryptedDirectoryByChunks(config, links);
}

async function putEncryptedDirectoryNode(
  config: EncryptedTreeConfig,
  links: Link[]
): Promise<EncryptedPutResult> {
  const totalSize = links.reduce((sum, l) => sum + (l.size ?? 0), 0);

  const node: TreeNode = {
    type: LinkType.Dir,
    links,
  };
  const { data } = await encodeAndHash(node);

  const { ciphertext, key } = await encryptChk(data);
  const hash = await sha256(ciphertext);
  await config.store.put(hash, ciphertext);
  return { hash, size: totalSize, key };
}

type IndexedEncryptedLink = {
  start: number;
  link: Link;
};

async function buildEncryptedDirectoryByChunks(
  config: EncryptedTreeConfig,
  links: Link[]
): Promise<EncryptedPutResult> {
  const indexedLinks = links.map((link, start) => ({ start, link }));
  return buildIndexedEncryptedDirectoryChunks(config, indexedLinks);
}

async function buildIndexedEncryptedDirectoryChunks(
  config: EncryptedTreeConfig,
  links: IndexedEncryptedLink[]
): Promise<EncryptedPutResult> {
  const maxLinks = getMaxLinks(config);
  const subTrees: IndexedEncryptedLink[] = [];

  for (let offset = 0; offset < links.length; offset += maxLinks) {
    const batch = links.slice(offset, offset + maxLinks);
    const start = batch[0]?.start ?? offset;
    const batchLinks = batch.map(({ link }) => link);
    const child = await putEncryptedDirectoryNode(config, batchLinks);

    subTrees.push({
      start,
      link: {
        hash: child.hash,
        name: `_chunk_${start}`,
        size: child.size,
        key: child.key,
        type: LinkType.Dir,
      },
    });
  }

  if (subTrees.length <= maxLinks) {
    return putEncryptedDirectoryNode(config, subTrees.map(({ link }) => link));
  }

  return buildIndexedEncryptedDirectoryChunks(config, subTrees);
}

/**
 * List directory entries from an encrypted directory
 *
 * Handles regular directories, canonical fanout directories, and historical
 * byte-chunked directory blobs.
 *
 * @param store - Storage backend
 * @param hash - Hash of encrypted directory
 * @param key - CHK decryption key
 * @returns Directory entries with their encryption keys
 */
export async function listDirectoryEncrypted(
  store: Store,
  hash: Hash,
  key: EncryptionKey,
  signal?: AbortSignal
): Promise<EncryptedDirEntry[]> {
  const node = await getEncryptedDirectoryNode(store, hash, key, signal);
  if (!node) return [];

  const usesFanout = nodeUsesDirectoryFanout(node);
  const entries: EncryptedDirEntry[] = [];
  for (const link of node.links) {
    if (isInternalDirectoryLinkWithFanout(link, usesFanout)) {
      if (!link.key) {
        throw new Error(`Missing decryption key for directory fanout: ${toHex(link.hash)}`);
      }
      entries.push(...await listDirectoryEncrypted(store, link.hash, link.key, signal));
      continue;
    }

    if (link.name === undefined) continue;

    entries.push({
      name: link.name,
      hash: link.hash,
      size: link.size,
      key: link.key,
      type: link.type,
      meta: link.meta,
    });
  }

  return entries;
}

/**
 * Get a tree node from encrypted storage
 *
 * @param store - Storage backend
 * @param hash - Hash of encrypted node
 * @param key - CHK decryption key
 * @returns Decrypted tree node
 */
export async function getTreeNodeEncrypted(
  store: Store,
  hash: Hash,
  key: EncryptionKey
): Promise<TreeNode | null> {
  const decrypted = await getRootNodeOrBlob(store, hash, key);
  if (!decrypted) return null;
  return tryDecodeTreeNode(decrypted);
}

function getMaxLinks(config: EncryptedTreeConfig): number {
  const maxLinks = config.maxLinks ?? DEFAULT_MAX_LINKS;
  if (!Number.isInteger(maxLinks) || maxLinks < 1) {
    throw new Error(`Invalid maxLinks: ${config.maxLinks}`);
  }
  return maxLinks;
}

function internalChunkStart(name: string): number | null {
  const prefix = '_chunk_';
  if (!name.startsWith(prefix)) return null;

  const suffix = name.slice(prefix.length);
  if (suffix.length === 0 || !/^[0-9]+$/.test(suffix)) return null;

  const start = Number(suffix);
  return Number.isSafeInteger(start) ? start : null;
}

function nodeUsesDirectoryFanout(node: TreeNode): boolean {
  return node.links.length > 0 && node.links.every((link) => (
    link.type === LinkType.Dir
      && link.name !== undefined
      && internalChunkStart(link.name) !== null
  ));
}

function isInternalDirectoryLinkWithFanout(link: Link, usesFanout: boolean): boolean {
  return usesFanout
    && link.type === LinkType.Dir
    && link.name !== undefined
    && internalChunkStart(link.name) !== null;
}
