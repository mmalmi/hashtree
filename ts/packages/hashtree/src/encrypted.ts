import { Store, Hash, TreeNode, Link, LinkType, toHex } from './types.js';
import { sha256 } from './hash.js';
import { encodeAndHash } from './codec.js';
import { type EncryptionKey } from './crypto.js';
import { encryptObject, type EncryptionMode } from './encryption.js';
import { compareNames } from './compare.js';
import * as read from './tree/read.js';
export { decryptRootNodeOrBlob } from './tree/readSupport.js';

const DEFAULT_MAX_LINKS = 174;

export interface EncryptedTreeConfig {
  store: Store;
  chunkSize: number;
  encryptionMode?: EncryptionMode;
  maxLinks?: number;
}

export interface ReadEncryptedOptions {
  /** Maximum plaintext bytes to return before throwing */
  maxBytes?: number;
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
 * Store a file using the configured encryption mode
 *
 * CHK derives each key from its plaintext. The randomized mode generates fresh keys.
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
    const { ciphertext, key } = await encryptObject(data, config.encryptionMode);
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
    const { ciphertext, key: chunkKey } = await encryptObject(chunk, config.encryptionMode);
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
  // Encrypt the tree node
  const { ciphertext, key: nodeKey } = await encryptObject(data, config.encryptionMode);
  const hash = await sha256(ciphertext);
  await store.put(hash, ciphertext);
  return { hash, key: nodeKey };
}

/** Read a file using the root key and each child's own encryption descriptor. */
export function readFileEncrypted(store: Store, hash: Hash, key: EncryptionKey, options: ReadEncryptedOptions = {}) {
  return read.readFile(store, hash, options, key);
}

export type StreamOptions = read.StreamOptions;

export function readFileEncryptedStream(store: Store, hash: Hash, key: EncryptionKey, options: StreamOptions = {}) {
  return read.readFileStream(store, hash, options, key);
}

export function readFileEncryptedRange(store: Store, hash: Hash, key: EncryptionKey, start: number, end?: number) {
  return read.readFileRange(store, hash, start, end, key);
}

/**
 * Directory entry with optional encryption key
 */
export interface EncryptedDirEntry {
  name: string;
  hash: Hash;
  size: number;
  /** Object key for encrypted children */
  key?: Uint8Array;
  /** Type of this entry: Blob, File, or Dir */
  type: LinkType;
  /** Optional metadata (createdAt, mimeType, thumbnail, etc.) */
  meta?: Record<string, unknown>;
}

/**
 * Store a directory using the configured encryption mode
 *
 * The directory node itself is encrypted. Child entries already have their own keys.
 * Large directories are split into canonical BUD-17 fanout nodes.
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
  return putEncryptedTreeNode(config, LinkType.Dir, links);
}

async function putEncryptedTreeNode(
  config: EncryptedTreeConfig,
  nodeType: LinkType.Dir | LinkType.Fanout,
  links: Link[]
): Promise<EncryptedPutResult> {
  const totalSize = links.reduce((sum, l) => sum + (l.size ?? 0), 0);

  const node: TreeNode = {
    type: nodeType,
    links,
  };
  const { data } = await encodeAndHash(node);

  const { ciphertext, key } = await encryptObject(data, config.encryptionMode);
  const hash = await sha256(ciphertext);
  await config.store.put(hash, ciphertext);
  return { hash, size: totalSize, key };
}

type IndexedEncryptedLink = {
  link: Link;
  count: number;
  first: string;
  last: string;
};

function fanoutMeta(count: number, first: string, last: string): Record<string, unknown> {
  return { count, first, last };
}

async function buildEncryptedDirectoryByChunks(
  config: EncryptedTreeConfig,
  links: Link[]
): Promise<EncryptedPutResult> {
  const spans = links.map((link) => {
    const name = link.name ?? toHex(link.hash);
    return { link, count: 1, first: name, last: name };
  });
  return buildEncryptedDirectoryFanoutLevel(config, spans, LinkType.Dir);
}

async function buildEncryptedDirectoryFanoutLevel(
  config: EncryptedTreeConfig,
  spans: IndexedEncryptedLink[],
  childNodeType: LinkType.Dir | LinkType.Fanout
): Promise<EncryptedPutResult> {
  const maxLinks = getMaxLinks(config);
  const subTrees: IndexedEncryptedLink[] = [];

  for (let offset = 0; offset < spans.length; offset += maxLinks) {
    const batch = spans.slice(offset, offset + maxLinks);
    const firstSpan = batch[0];
    const lastSpan = batch[batch.length - 1];
    const count = batch.reduce((sum, span) => sum + span.count, 0);
    const batchLinks = batch.map(({ link }) => link);
    const child = await putEncryptedTreeNode(config, childNodeType, batchLinks);

    subTrees.push({
      link: {
        hash: child.hash,
        size: child.size,
        key: child.key,
        type: childNodeType,
        meta: fanoutMeta(count, firstSpan.first, lastSpan.last),
      },
      count,
      first: firstSpan.first,
      last: lastSpan.last,
    });
  }

  if (subTrees.length <= maxLinks) {
    return putEncryptedTreeNode(config, LinkType.Fanout, subTrees.map(({ link }) => link));
  }

  return buildEncryptedDirectoryFanoutLevel(config, subTrees, LinkType.Fanout);
}

/**
 * List directory entries from an encrypted directory
 *
 * Handles regular directories, canonical fanout directories, and historical
 * byte-chunked directory blobs.
 *
 * @param store - Storage backend
 * @param hash - Hash of encrypted directory
 * @param key - Object decryption key
 * @returns Directory entries with their encryption keys
 */
export async function listDirectoryEncrypted(
  store: Store,
  hash: Hash,
  key: EncryptionKey,
  signal?: AbortSignal
): Promise<EncryptedDirEntry[]> {
  const entries = await read.listDirectory(store, hash, signal, key);
  return entries.map(({ cid, ...entry }) => ({ ...entry, hash: cid.hash, key: cid.key }));
}

/**
 * Get a tree node from encrypted storage
 *
 * @param store - Storage backend
 * @param hash - Hash of encrypted node
 * @param key - Object decryption key
 * @returns Decrypted tree node
 */
export async function getTreeNodeEncrypted(
  store: Store,
  hash: Hash,
  key: EncryptionKey
): Promise<TreeNode | null> {
  return read.getTreeNode(store, hash, key);
}

function getMaxLinks(config: EncryptedTreeConfig): number {
  const maxLinks = config.maxLinks ?? DEFAULT_MAX_LINKS;
  if (!Number.isInteger(maxLinks) || maxLinks < 1) {
    throw new Error(`Invalid maxLinks: ${config.maxLinks}`);
  }
  return maxLinks;
}
