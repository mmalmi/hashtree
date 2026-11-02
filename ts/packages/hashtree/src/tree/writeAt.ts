/**
 * writeAt - Efficiently patch bytes at a specific offset without rewriting entire file
 *
 * Only affected chunks and their parent nodes are rewritten.
 * For a large file with a small patch, this is much more efficient than putFile.
 */

import { Store, Hash, TreeNode, Link, LinkType, toHex } from '../types.js';
import { sha256 } from '../hash.js';
import { encodeAndHash, decodeTreeNode, tryDecodeTreeNode } from '../codec.js';
import { type EncryptionKey } from '../crypto.js';
import { encryptObject, decryptObject, encryptionMode, type EncryptionMode } from '../encryption.js';

export interface WriteAtConfig {
  store: Store;
  chunkSize: number;
  encryptionMode?: EncryptionMode;
}

export interface WriteAtResult {
  /** New root hash after modification */
  hash: Hash;
  /** Total file size (unchanged) */
  size: number;
}

export interface WriteAtEncryptedResult extends WriteAtResult {
  /** New encryption key for the root */
  key: EncryptionKey;
}

/**
 * Write data at a specific offset in an unencrypted file
 *
 * @param config - Tree configuration
 * @param rootHash - Current root hash of the file
 * @param offset - Byte offset to write at
 * @param data - Data to write
 * @returns New root hash
 */
export async function writeAt(
  config: WriteAtConfig,
  rootHash: Hash,
  offset: number,
  data: Uint8Array
): Promise<WriteAtResult> {
  const { store } = config;

  // Get root data
  const rootData = await store.get(rootHash);
  if (!rootData) {
    throw new Error(`Root not found: ${toHex(rootHash)}`);
  }

  // Check if it's a tree node or single blob
  const node = tryDecodeTreeNode(rootData);
  if (!node) {
    // Single blob - modify in place
    if (offset + data.length > rootData.length) {
      throw new Error(`Write extends beyond file: offset=${offset}, data=${data.length}, file=${rootData.length}`);
    }

    const newData = new Uint8Array(rootData);
    newData.set(data, offset);

    const newHash = await sha256(newData);
    await store.put(newHash, newData);

    return { hash: newHash, size: newData.length };
  }

  // Multi-chunk file - find and modify affected chunks
  const totalSize = node.links.reduce((sum, l) => sum + l.size, 0);

  if (offset + data.length > totalSize) {
    throw new Error(`Write extends beyond file: offset=${offset}, data=${data.length}, file=${totalSize}`);
  }

  const newLinks = await writeAtInNode(config, node, offset, data);

  // Rebuild tree with new links
  const newRootHash = await buildTreeFromLinks(config, newLinks, totalSize);

  return { hash: newRootHash, size: totalSize };
}

/**
 * Write data at a specific offset in an encrypted file
 *
 * @param config - Tree configuration
 * @param rootHash - Current root hash of the encrypted file
 * @param rootKey - Current encryption key for the root
 * @param offset - Byte offset to write at
 * @param data - Data to write (plaintext)
 * @returns New root hash and key
 */
export async function writeAtEncrypted(
  config: WriteAtConfig,
  rootHash: Hash,
  rootKey: EncryptionKey,
  offset: number,
  data: Uint8Array
): Promise<WriteAtEncryptedResult> {
  if (encryptionMode(rootKey) === 'rnd-v1') config = { ...config, encryptionMode: 'rnd-v1' };
  const { store } = config;

  // Get and decrypt root
  const encryptedRoot = await store.get(rootHash);
  if (!encryptedRoot) {
    throw new Error(`Root not found: ${toHex(rootHash)}`);
  }

  const decryptedRoot = await decryptObject(encryptedRoot, rootKey);

  // Check if it's a tree node or single blob
  const node = tryDecodeTreeNode(decryptedRoot);
  if (!node) {
    // Single blob - modify in place
    if (offset + data.length > decryptedRoot.length) {
      throw new Error(`Write extends beyond file: offset=${offset}, data=${data.length}, file=${decryptedRoot.length}`);
    }

    const newData = new Uint8Array(decryptedRoot);
    newData.set(data, offset);

    // Re-encrypt with CHK
    const { ciphertext, key } = await encryptObject(newData, config.encryptionMode);
    const newHash = await sha256(ciphertext);
    await store.put(newHash, ciphertext);

    return { hash: newHash, size: newData.length, key };
  }

  // Multi-chunk file - find and modify affected chunks
  const totalSize = node.links.reduce((sum, l) => sum + l.size, 0);

  if (offset + data.length > totalSize) {
    throw new Error(`Write extends beyond file: offset=${offset}, data=${data.length}, file=${totalSize}`);
  }

  const newLinks = await writeAtInNode(config, node, offset, data);

  // Rebuild encrypted tree with new links
  const { hash: newRootHash, key: newRootKey } = await buildEncryptedTreeFromLinks(config, newLinks, totalSize);

  return { hash: newRootHash, size: totalSize, key: newRootKey };
}

/** Rewrite overlapping links and preserve each child's plaintext or encrypted representation. */
async function writeAtInNode(
  config: WriteAtConfig,
  node: TreeNode,
  offset: number,
  data: Uint8Array
): Promise<Link[]> {
  const links: Link[] = [];
  let position = 0;
  for (const link of node.links) {
    const start = position;
    position += link.size;
    if (position <= offset || start >= offset + data.length) {
      links.push(link);
      continue;
    }

    const stored = await config.store.get(link.hash);
    if (!stored) throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    const original = link.key ? await decryptObject(stored, link.key) : stored;
    const patch = data.subarray(Math.max(0, start - offset), position - offset);
    const localOffset = Math.max(0, offset - start);
    let updated: Uint8Array;
    if (link.type === LinkType.Blob) {
      updated = original.slice();
      updated.set(patch, localOffset);
    } else {
      const child = decodeTreeNode(original);
      const childLinks = await writeAtInNode(config, child, localOffset, patch);
      updated = (await encodeAndHash({ ...child, links: childLinks })).data;
    }

    const mode = link.key && encryptionMode(link.key) === 'rnd-v1' ? 'rnd-v1' : config.encryptionMode;
    const encoded = link.key ? await encryptObject(updated, mode) : { ciphertext: updated, key: undefined };
    const hash = await sha256(encoded.ciphertext);
    await config.store.put(hash, encoded.ciphertext);
    links.push({ ...link, hash, key: encoded.key });
  }
  return links;
}

/**
 * Build tree from links (unencrypted)
 */
async function buildTreeFromLinks(
  config: WriteAtConfig,
  links: Link[],
  totalSize: number
): Promise<Hash> {
  const { store } = config;

  if (links.length === 1 && !links[0].key && links[0].size === totalSize) {
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

/**
 * Build encrypted tree from links
 */
async function buildEncryptedTreeFromLinks(
  config: WriteAtConfig,
  links: Link[],
  totalSize: number
): Promise<{ hash: Hash; key: EncryptionKey }> {
  const { store } = config;

  // Single link with key - return directly
  if (links.length === 1 && links[0].key && encryptionMode(links[0].key) === config.encryptionMode && links[0].size === totalSize) {
    return { hash: links[0].hash, key: links[0].key };
  }

  // Create single flat node with all links
  const node: TreeNode = {
    type: LinkType.File,
    links,
  };
  const { data } = await encodeAndHash(node);
  const { ciphertext, key } = await encryptObject(data, config.encryptionMode);
  const hash = await sha256(ciphertext);
  await store.put(hash, ciphertext);
  return { hash, key };
}
