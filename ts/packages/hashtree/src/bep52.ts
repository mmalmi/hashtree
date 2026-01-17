/**
 * BEP52 (BitTorrent v2) compatible merkle tree implementation
 *
 * Key differences from default hashtree:
 * - 16 KiB block size (vs 256KB default)
 * - Binary tree (2 children per node, not variable fanout)
 * - Zero-padding for incomplete trees (pads to power of 2)
 * - SHA256 hash algorithm
 * - Piece layers: intermediate hash layers at piece boundaries
 *
 * @see https://www.bittorrent.org/beps/bep_0052.html
 */

import { Hash, Store, hashEquals } from './types.js';
import { sha256 } from './hash.js';

/**
 * BEP52 block size: 16 KiB
 */
export const BEP52_BLOCK_SIZE = 16 * 1024;

/**
 * Zero hash (32 bytes of zeros) used for padding
 */
export const ZERO_HASH: Hash = new Uint8Array(32);

/**
 * Result of building a BEP52 merkle tree
 */
export interface Bep52Result {
  /** Root hash (pieces root) */
  root: Hash;
  /** Total file size in bytes */
  size: number;
  /** Number of 16KB blocks */
  blockCount: number;
  /** Leaf hashes (one per block) */
  leafHashes: Hash[];
  /**
   * Piece layer hashes at specified piece size
   * Only populated if pieceSize > blockSize
   */
  pieceLayers?: Hash[];
}

/**
 * Configuration for BEP52 tree building
 */
export interface Bep52Config {
  /** Store for persisting blocks (optional - if not provided, only computes hashes) */
  store?: Store;
  /**
   * Piece size for piece layers (must be power of 2, >= 16KB)
   * Common values: 16KB (same as block), 32KB, 64KB, 128KB, 256KB, 512KB, 1MB, etc.
   * If not specified, no piece layers are computed.
   */
  pieceSize?: number;
}

/**
 * Compute the number of leaves needed (rounds up to power of 2)
 */
export function merkleNumLeafs(blocks: number): number {
  if (blocks <= 0) return 0;
  let n = 1;
  while (n < blocks) n <<= 1;
  return n;
}

/**
 * Get parent index in flat tree representation
 * Tree layout: [0=root, 1=left, 2=right, 3=left-left, 4=left-right, ...]
 */
export function merkleGetParent(idx: number): number {
  return Math.floor((idx - 1) / 2);
}

/**
 * Get sibling index
 */
export function merkleGetSibling(idx: number): number {
  // Even indices have sibling to left, odd to right
  return idx + ((idx & 1) ? 1 : -1);
}

/**
 * Get first child index
 */
export function merkleGetFirstChild(idx: number): number {
  return idx * 2 + 1;
}

/**
 * Get index of first leaf given number of leaves
 */
export function merkleFirstLeaf(numLeafs: number): number {
  return numLeafs - 1;
}

/**
 * Get total number of nodes in tree given number of leaves
 */
export function merkleNumNodes(numLeafs: number): number {
  return numLeafs * 2 - 1;
}

/**
 * Compute hash of two concatenated hashes (parent = H(left || right))
 */
export async function merkleHashPair(left: Hash, right: Hash): Promise<Hash> {
  const combined = new Uint8Array(64);
  combined.set(left, 0);
  combined.set(right, 32);
  return sha256(combined);
}

/**
 * Compute the pad hash for a given depth
 * pad(0) = zero hash
 * pad(n) = H(pad(n-1) || pad(n-1))
 */
export async function merklePadHash(depth: number): Promise<Hash> {
  let pad = ZERO_HASH;
  for (let i = 0; i < depth; i++) {
    pad = await merkleHashPair(pad, pad);
  }
  return pad;
}

/**
 * Compute merkle root from leaf hashes with zero-padding
 *
 * @param leaves - Array of leaf hashes (one per 16KB block)
 * @param numLeafs - Number of leaves in balanced tree (power of 2)
 * @returns Root hash
 */
export async function merkleRoot(leaves: Hash[], numLeafs?: number): Promise<Hash> {
  if (leaves.length === 0) {
    return ZERO_HASH;
  }

  if (leaves.length === 1 && (numLeafs === undefined || numLeafs === 1)) {
    return leaves[0];
  }

  const targetLeafs = numLeafs ?? merkleNumLeafs(leaves.length);

  // Build tree bottom-up using scratch space
  // This matches libtorrent's merkle_root_scratch approach
  let current = leaves.slice();
  let padHash = ZERO_HASH;
  let levelSize = targetLeafs;

  while (levelSize > 1) {
    const nextLevel: Hash[] = [];

    for (let i = 0; i < current.length; i += 2) {
      if (i + 1 < current.length) {
        // Both children present
        nextLevel.push(await merkleHashPair(current[i], current[i + 1]));
      } else {
        // Odd leaf - pair with pad
        nextLevel.push(await merkleHashPair(current[i], padHash));
      }
    }

    // Compute next level's pad hash (H(pad || pad))
    padHash = await merkleHashPair(padHash, padHash);

    current = nextLevel;
    levelSize = levelSize / 2;
  }

  return current[0];
}

/**
 * Build full merkle tree from leaves, returning all nodes
 *
 * @param leaves - Leaf hashes
 * @returns Flat array of tree nodes [root, layer1..., leaves]
 */
export async function merkleBuildTree(leaves: Hash[]): Promise<Hash[]> {
  if (leaves.length === 0) {
    return [ZERO_HASH];
  }

  const numLeafs = merkleNumLeafs(leaves.length);
  const numNodes = merkleNumNodes(numLeafs);
  const tree: Hash[] = new Array(numNodes);

  // Fill leaves (with zero-padding)
  const firstLeaf = merkleFirstLeaf(numLeafs);
  for (let i = 0; i < numLeafs; i++) {
    tree[firstLeaf + i] = i < leaves.length ? leaves[i] : ZERO_HASH;
  }

  // Build parents bottom-up
  let levelStart = firstLeaf;
  let levelSize = numLeafs;

  while (levelSize > 1) {
    let parent = merkleGetParent(levelStart);
    for (let i = levelStart; i < levelStart + levelSize; i += 2) {
      tree[parent] = await merkleHashPair(tree[i], tree[i + 1]);
      parent++;
    }
    levelStart = merkleGetParent(levelStart);
    levelSize = levelSize / 2;
  }

  return tree;
}

/**
 * Generate uncle hashes (proof) for a leaf
 *
 * @param tree - Full merkle tree
 * @param leafIndex - Index of leaf in leaf layer (0-based)
 * @param numLeafs - Number of leaves in tree
 * @returns Array of uncle hashes from leaf to root
 */
export function merkleGetProof(tree: Hash[], leafIndex: number, numLeafs: number): Hash[] {
  const proofs: Hash[] = [];
  let idx = merkleFirstLeaf(numLeafs) + leafIndex;

  while (idx > 0) {
    const siblingIdx = merkleGetSibling(idx);
    proofs.push(tree[siblingIdx]);
    idx = merkleGetParent(idx);
  }

  return proofs;
}

/**
 * Verify a merkle proof
 *
 * @param leaf - Leaf hash to verify
 * @param leafIndex - Position of leaf (0-based)
 * @param proof - Uncle hashes from leaf to root
 * @param root - Expected root hash
 * @param numLeafs - Number of leaves in tree
 * @returns True if proof is valid
 */
export async function merkleVerifyProof(
  leaf: Hash,
  leafIndex: number,
  proof: Hash[],
  root: Hash,
  numLeafs: number,
): Promise<boolean> {
  let hash = leaf;
  let idx = merkleFirstLeaf(numLeafs) + leafIndex;

  for (const uncle of proof) {
    // In flat tree: children of parent P are at 2P+1 (left) and 2P+2 (right)
    // So odd indices are LEFT children, even indices are RIGHT children
    if (idx & 1) {
      // Odd index - we're on the left, uncle is on right
      hash = await merkleHashPair(hash, uncle);
    } else {
      // Even index - we're on the right, uncle is on left
      hash = await merkleHashPair(uncle, hash);
    }
    idx = merkleGetParent(idx);
  }

  return hashEquals(hash, root);
}

/**
 * BEP52 Tree Builder
 *
 * Builds a binary merkle tree compatible with BitTorrent v2.
 */
export class Bep52TreeBuilder {
  private store?: Store;
  private pieceSize: number;
  private blocksPerPiece: number;

  constructor(config: Bep52Config = {}) {
    this.store = config.store;
    this.pieceSize = config.pieceSize ?? BEP52_BLOCK_SIZE;

    if (this.pieceSize < BEP52_BLOCK_SIZE) {
      throw new Error(`Piece size must be >= ${BEP52_BLOCK_SIZE} (16KB)`);
    }
    if ((this.pieceSize & (this.pieceSize - 1)) !== 0) {
      throw new Error('Piece size must be a power of 2');
    }

    this.blocksPerPiece = this.pieceSize / BEP52_BLOCK_SIZE;
  }

  /**
   * Build BEP52 merkle tree from file data
   *
   * @param data - File content
   * @returns Tree result with root, leaf hashes, and optional piece layers
   */
  async buildFromData(data: Uint8Array): Promise<Bep52Result> {
    const size = data.length;
    const blockCount = Math.ceil(size / BEP52_BLOCK_SIZE);

    if (blockCount === 0) {
      return {
        root: ZERO_HASH,
        size: 0,
        blockCount: 0,
        leafHashes: [],
      };
    }

    // Hash each 16KB block
    const leafHashes: Hash[] = [];
    for (let i = 0; i < blockCount; i++) {
      const start = i * BEP52_BLOCK_SIZE;
      const end = Math.min(start + BEP52_BLOCK_SIZE, size);
      const block = data.slice(start, end);

      // Store block if store is provided
      const hash = await sha256(block);
      if (this.store) {
        await this.store.put(hash, block);
      }
      leafHashes.push(hash);
    }

    // Compute root
    const numLeafs = merkleNumLeafs(blockCount);
    const root = await merkleRoot(leafHashes, numLeafs);

    // Compute piece layers if piece size > block size
    let pieceLayers: Hash[] | undefined;
    if (this.pieceSize > BEP52_BLOCK_SIZE) {
      pieceLayers = await this.computePieceLayers(leafHashes, numLeafs);
    }

    return {
      root,
      size,
      blockCount,
      leafHashes,
      pieceLayers,
    };
  }

  /**
   * Compute piece layer hashes
   * Each piece layer hash is the root of a subtree covering blocksPerPiece blocks
   */
  private async computePieceLayers(leafHashes: Hash[], _numLeafs: number): Promise<Hash[]> {
    const pieceCount = Math.ceil(leafHashes.length / this.blocksPerPiece);
    const layers: Hash[] = [];

    for (let p = 0; p < pieceCount; p++) {
      const start = p * this.blocksPerPiece;
      const end = Math.min(start + this.blocksPerPiece, leafHashes.length);
      const pieceLeaves = leafHashes.slice(start, end);

      // Compute subtree root for this piece
      const pieceRoot = await merkleRoot(pieceLeaves, this.blocksPerPiece);
      layers.push(pieceRoot);
    }

    return layers;
  }

  /**
   * Build tree from pre-computed leaf hashes
   */
  async buildFromHashes(leafHashes: Hash[], size: number): Promise<Bep52Result> {
    const blockCount = leafHashes.length;
    const numLeafs = merkleNumLeafs(blockCount);
    const root = await merkleRoot(leafHashes, numLeafs);

    let pieceLayers: Hash[] | undefined;
    if (this.pieceSize > BEP52_BLOCK_SIZE) {
      pieceLayers = await this.computePieceLayers(leafHashes, numLeafs);
    }

    return {
      root,
      size,
      blockCount,
      leafHashes,
      pieceLayers,
    };
  }
}

/**
 * Streaming BEP52 tree builder
 * Allows incremental hashing without loading entire file into memory
 */
export class Bep52StreamBuilder {
  private store?: Store;
  private buffer: Uint8Array;
  private bufferOffset: number = 0;
  private leafHashes: Hash[] = [];
  private totalSize: number = 0;

  constructor(config: Bep52Config = {}) {
    this.store = config.store;
    this.buffer = new Uint8Array(BEP52_BLOCK_SIZE);
  }

  /**
   * Append data to the stream
   */
  async append(data: Uint8Array): Promise<void> {
    let offset = 0;

    while (offset < data.length) {
      const space = BEP52_BLOCK_SIZE - this.bufferOffset;
      const toWrite = Math.min(space, data.length - offset);

      this.buffer.set(data.subarray(offset, offset + toWrite), this.bufferOffset);
      this.bufferOffset += toWrite;
      offset += toWrite;

      if (this.bufferOffset === BEP52_BLOCK_SIZE) {
        await this.flushBlock();
      }
    }

    this.totalSize += data.length;
  }

  private async flushBlock(): Promise<void> {
    if (this.bufferOffset === 0) return;

    const block = this.buffer.slice(0, this.bufferOffset);
    const hash = await sha256(block);

    if (this.store) {
      await this.store.put(hash, new Uint8Array(block));
    }

    this.leafHashes.push(hash);
    this.bufferOffset = 0;
  }

  /**
   * Finalize and return the tree result
   */
  async finalize(): Promise<Bep52Result> {
    // Flush any remaining data
    await this.flushBlock();

    const blockCount = this.leafHashes.length;

    if (blockCount === 0) {
      return {
        root: ZERO_HASH,
        size: 0,
        blockCount: 0,
        leafHashes: [],
      };
    }

    const numLeafs = merkleNumLeafs(blockCount);
    const root = await merkleRoot(this.leafHashes, numLeafs);

    return {
      root,
      size: this.totalSize,
      blockCount,
      leafHashes: this.leafHashes,
    };
  }

  /**
   * Get current stats
   */
  get stats(): { blocks: number; buffered: number; totalSize: number } {
    return {
      blocks: this.leafHashes.length,
      buffered: this.bufferOffset,
      totalSize: this.totalSize,
    };
  }
}
