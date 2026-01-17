/**
 * writeAt - Efficiently patch bytes at a specific offset without rewriting entire file
 *
 * Only affected chunks and their parent nodes are rewritten.
 * For a large file with a small patch, this is much more efficient than putFile.
 */

import { Store, Hash, TreeNode, Link, LinkType, toHex } from '../types.js';
import { sha256 } from '../hash.js';
import { encodeAndHash, decodeTreeNode, tryDecodeTreeNode } from '../codec.js';
import { encryptChk, decryptChk, type EncryptionKey } from '../crypto.js';

export interface WriteAtConfig {
  store: Store;
  chunkSize: number;
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
  const { store } = config;

  // Get and decrypt root
  const encryptedRoot = await store.get(rootHash);
  if (!encryptedRoot) {
    throw new Error(`Root not found: ${toHex(rootHash)}`);
  }

  const decryptedRoot = await decryptChk(encryptedRoot, rootKey);

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
    const { ciphertext, key } = await encryptChk(newData);
    const newHash = await sha256(ciphertext);
    await store.put(newHash, ciphertext);

    return { hash: newHash, size: newData.length, key };
  }

  // Multi-chunk file - find and modify affected chunks
  const totalSize = node.links.reduce((sum, l) => sum + l.size, 0);

  if (offset + data.length > totalSize) {
    throw new Error(`Write extends beyond file: offset=${offset}, data=${data.length}, file=${totalSize}`);
  }

  const newLinks = await writeAtInNodeEncrypted(config, node, offset, data);

  // Rebuild encrypted tree with new links
  const { hash: newRootHash, key: newRootKey } = await buildEncryptedTreeFromLinks(config, newLinks, totalSize);

  return { hash: newRootHash, size: totalSize, key: newRootKey };
}

/**
 * Recursively modify chunks in a tree node (unencrypted)
 */
async function writeAtInNode(
  config: WriteAtConfig,
  node: TreeNode,
  offset: number,
  data: Uint8Array
): Promise<Link[]> {
  const { store } = config;
  const newLinks: Link[] = [];
  let position = 0;

  for (const link of node.links) {
    const linkStart = position;
    const linkEnd = position + link.size;
    position = linkEnd;

    // Check if this chunk overlaps with the write range
    const writeStart = offset;
    const writeEnd = offset + data.length;

    if (linkEnd <= writeStart || linkStart >= writeEnd) {
      // No overlap - keep original link
      newLinks.push(link);
      continue;
    }

    // This chunk needs modification
    const chunkData = await store.get(link.hash);
    if (!chunkData) {
      throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    }

    if (link.type !== LinkType.Blob) {
      // Intermediate node - recurse
      const childNode = decodeTreeNode(chunkData);
      const childOffset = Math.max(0, offset - linkStart);
      const childData = data.slice(
        Math.max(0, linkStart - offset),
        Math.min(data.length, linkEnd - offset)
      );

      const newChildLinks = await writeAtInNode(config, childNode, childOffset, childData);

      // Build new intermediate node
      const newChildNode: TreeNode = {
        type: LinkType.File,
        links: newChildLinks,
      };
      const { data: encodedNode, hash: newChildHash } = await encodeAndHash(newChildNode);
      await store.put(newChildHash, encodedNode);

      newLinks.push({
        hash: newChildHash,
        size: link.size,
        type: LinkType.File,
      });
    } else {
      // Leaf blob - modify bytes
      const newChunkData = new Uint8Array(chunkData);

      // Calculate which bytes to modify in this chunk
      const chunkWriteStart = Math.max(0, writeStart - linkStart);
      const chunkWriteEnd = Math.min(chunkData.length, writeEnd - linkStart);
      const dataReadStart = Math.max(0, linkStart - writeStart);
      const dataReadEnd = dataReadStart + (chunkWriteEnd - chunkWriteStart);

      newChunkData.set(data.slice(dataReadStart, dataReadEnd), chunkWriteStart);

      const newHash = await sha256(newChunkData);
      await store.put(newHash, newChunkData);

      newLinks.push({
        hash: newHash,
        size: newChunkData.length,
        type: LinkType.Blob,
      });
    }
  }

  return newLinks;
}

/**
 * Recursively modify chunks in an encrypted tree node
 *
 * NOTE: For encrypted files, link.size stores CIPHERTEXT size (includes AES-GCM tag),
 * but offset/data are in PLAINTEXT terms. We need to decrypt each chunk to get
 * the actual plaintext size for offset calculations.
 */
async function writeAtInNodeEncrypted(
  config: WriteAtConfig,
  node: TreeNode,
  offset: number,
  data: Uint8Array
): Promise<Link[]> {
  const { store } = config;
  const newLinks: Link[] = [];

  // First pass: collect plaintext sizes by decrypting headers or using heuristics
  // AES-GCM adds 16 bytes tag, so plaintext size = ciphertext size - 16
  const AES_GCM_TAG_SIZE = 16;

  let position = 0; // Track plaintext position

  for (const link of node.links) {
    // For blobs, plaintext size = ciphertext size - tag
    // For intermediate nodes, the size stored is the sum of child plaintext sizes
    const plaintextSize = link.type === LinkType.Blob
      ? link.size - AES_GCM_TAG_SIZE
      : link.size; // Intermediate nodes store total plaintext size of subtree

    const linkStart = position;
    const linkEnd = position + plaintextSize;
    position = linkEnd;

    // Check if this chunk overlaps with the write range
    const writeStart = offset;
    const writeEnd = offset + data.length;

    if (linkEnd <= writeStart || linkStart >= writeEnd) {
      // No overlap - keep original link
      newLinks.push(link);
      continue;
    }

    // This chunk needs modification
    if (!link.key) {
      throw new Error(`Missing decryption key for chunk: ${toHex(link.hash)}`);
    }

    const encryptedChunk = await store.get(link.hash);
    if (!encryptedChunk) {
      throw new Error(`Missing chunk: ${toHex(link.hash)}`);
    }

    const decryptedChunk = await decryptChk(encryptedChunk, link.key);

    if (link.type !== LinkType.Blob) {
      // Intermediate node - recurse
      const childNode = decodeTreeNode(decryptedChunk);
      const childOffset = Math.max(0, offset - linkStart);
      const childData = data.slice(
        Math.max(0, linkStart - offset),
        Math.min(data.length, linkEnd - offset)
      );

      const newChildLinks = await writeAtInNodeEncrypted(config, childNode, childOffset, childData);

      // Build new encrypted intermediate node
      const newChildNode: TreeNode = {
        type: LinkType.File,
        links: newChildLinks,
      };
      const { data: encodedNode } = await encodeAndHash(newChildNode);
      const { ciphertext, key } = await encryptChk(encodedNode);
      const newChildHash = await sha256(ciphertext);
      await store.put(newChildHash, ciphertext);

      // Recalculate total plaintext size from new links
      const newPlaintextSize = newChildLinks.reduce((sum, l) => {
        return sum + (l.type === LinkType.Blob ? l.size - AES_GCM_TAG_SIZE : l.size);
      }, 0);

      newLinks.push({
        hash: newChildHash,
        size: newPlaintextSize, // Store plaintext size for intermediate nodes
        key,
        type: LinkType.File,
      });
    } else {
      // Leaf blob - modify bytes
      const newChunkData = new Uint8Array(decryptedChunk);

      // Calculate which bytes to modify in this chunk
      const chunkWriteStart = Math.max(0, writeStart - linkStart);
      const chunkWriteEnd = Math.min(decryptedChunk.length, writeEnd - linkStart);
      const dataReadStart = Math.max(0, linkStart - writeStart);
      const dataReadEnd = dataReadStart + (chunkWriteEnd - chunkWriteStart);

      newChunkData.set(data.slice(dataReadStart, dataReadEnd), chunkWriteStart);

      // Re-encrypt with CHK
      const { ciphertext, key } = await encryptChk(newChunkData);
      const newHash = await sha256(ciphertext);
      await store.put(newHash, ciphertext);

      newLinks.push({
        hash: newHash,
        size: ciphertext.length, // Store ciphertext size for blobs
        key,
        type: LinkType.Blob,
      });
    }
  }

  return newLinks;
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
  if (links.length === 1 && links[0].key && links[0].size === totalSize) {
    return { hash: links[0].hash, key: links[0].key };
  }

  // Create single flat node with all links
  const node: TreeNode = {
    type: LinkType.File,
    links,
  };
  const { data } = await encodeAndHash(node);
  const { ciphertext, key } = await encryptChk(data);
  const hash = await sha256(ciphertext);
  await store.put(hash, ciphertext);
  return { hash, key };
}
