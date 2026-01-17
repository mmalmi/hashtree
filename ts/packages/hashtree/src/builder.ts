/**
 * Tree builder with chunking and fanout support
 *
 * - Large files are split into chunks
 * - Large directories are split into sub-trees
 * - Supports streaming appends
 */

import { Store, Hash, TreeNode, Link, LinkType } from './types.js';
import { sha256 } from './hash.js';
import { encodeAndHash } from './codec.js';

/**
 * Default chunk size: 2MB (optimized for blossom uploads)
 */
export const DEFAULT_CHUNK_SIZE = 2 * 1024 * 1024;

/**
 * Chunker function: returns chunk size for a given chunk index
 * @param index - 0-based chunk index
 * @returns chunk size in bytes
 */
export type Chunker = (index: number) => number;

/**
 * Create a fixed-size chunker
 */
export function fixedChunker(size: number): Chunker {
  return () => size;
}

/**
 * Create a video-optimized chunker with smaller first chunk for fast playback start
 * @param firstChunkSize - Size of first chunk (default: 256KB)
 * @param regularChunkSize - Size of remaining chunks (default: 2MB)
 */
export function videoChunker(
  firstChunkSize: number = 256 * 1024,
  regularChunkSize: number = DEFAULT_CHUNK_SIZE
): Chunker {
  return (index: number) => index === 0 ? firstChunkSize : regularChunkSize;
}

export interface BuilderConfig {
  store: Store;
  /** Chunk size for splitting blobs (ignored if chunker is provided) */
  chunkSize?: number;
  /** Custom chunker function for variable chunk sizes */
  chunker?: Chunker;
  /** Hash chunks in parallel (default: true) */
  parallel?: boolean;
}

export interface FileEntry {
  name: string;
  data: Uint8Array;
}

export interface DirEntry {
  name: string;
  hash: Hash;
  size: number;
  type: LinkType;
  meta?: Record<string, unknown>;
}

/**
 * TreeBuilder - builds content-addressed merkle trees
 */
export class TreeBuilder {
  private store: Store;
  private chunker: Chunker;
  private parallel: boolean;

  constructor(config: BuilderConfig) {
    this.store = config.store;
    this.chunker = config.chunker ?? fixedChunker(config.chunkSize ?? DEFAULT_CHUNK_SIZE);
    this.parallel = config.parallel ?? true;
  }

  /**
   * Store a blob directly (small data)
   * Returns the content hash
   */
  async putBlob(data: Uint8Array): Promise<Hash> {
    const hash = await sha256(data);
    await this.store.put(hash, data);
    return hash;
  }

  /**
   * Store a file, chunking if necessary
   * Returns root hash and total size
   */
  async putFile(data: Uint8Array): Promise<{ hash: Hash; size: number }> {
    const size = data.length;
    const firstChunkSize = this.chunker(0);

    // Small file - store as single blob
    if (data.length <= firstChunkSize) {
      const hash = await this.putBlob(data);
      return { hash, size };
    }

    // Split into chunks using chunker
    const chunkList: Uint8Array[] = [];
    const chunkSizes: number[] = [];
    let offset = 0;
    let chunkIndex = 0;

    while (offset < data.length) {
      const chunkSize = this.chunker(chunkIndex);
      const end = Math.min(offset + chunkSize, data.length);
      chunkList.push(data.slice(offset, end));
      chunkSizes.push(end - offset);
      offset = end;
      chunkIndex++;
    }

    // Hash and store chunks (parallel or sequential)
    let chunkHashes: Hash[];
    if (this.parallel) {
      chunkHashes = await Promise.all(chunkList.map(chunk => this.putBlob(chunk)));
    } else {
      chunkHashes = [];
      for (const chunk of chunkList) {
        chunkHashes.push(await this.putBlob(chunk));
      }
    }

    // Build tree from chunks (leaf chunks are raw blobs)
    const chunks: Link[] = chunkHashes.map((hash, i) => ({
      hash,
      size: chunkSizes[i],
      type: LinkType.Blob,
    }));
    const rootHash = await this.buildTree(chunks, size);
    return { hash: rootHash, size };
  }

  /**
   * Build a balanced tree from links (for chunked files)
   * Handles fanout by creating intermediate nodes
   */
  private async buildTree(links: Link[], totalSize?: number): Promise<Hash> {
    // Single link - return it directly
    if (links.length === 1 && links[0].size === totalSize) {
      return links[0].hash;
    }

    // Create single flat node with all links
    const node: TreeNode = {
      type: LinkType.File,
      links,
    };
    const { data, hash } = await encodeAndHash(node);
    await this.store.put(hash, data);
    return hash;
  }

  /**
   * Build a directory from entries
   * Entries can be files or subdirectories
   *
   * Directories are encoded as MessagePack blobs. If the encoded blob exceeds
   * chunkSize, it's chunked by bytes like files using putFile.
   *
   * @param entries Directory entries
   */
  async putDirectory(entries: DirEntry[]): Promise<Hash> {
    // Sort entries by name for deterministic hashing
    const sorted = [...entries].sort((a, b) => a.name.localeCompare(b.name));

    const links: Link[] = sorted.map(e => ({
      hash: e.hash,
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
    if (data.length <= this.chunker(0)) {
      await this.store.put(hash, data);
      return hash;
    }

    // Large directory - reuse putFile for chunking
    const { hash: rootHash } = await this.putFile(data);
    return rootHash;
  }

  /**
   * Create a tree node
   * @param nodeType - LinkType.File or LinkType.Dir
   */
  async putTreeNode(
    nodeType: LinkType.File | LinkType.Dir,
    links: Link[]
  ): Promise<Hash> {
    const node: TreeNode = {
      type: nodeType,
      links,
    };

    const { data, hash } = await encodeAndHash(node);
    await this.store.put(hash, data);
    return hash;
  }
}

/**
 * StreamBuilder - supports incremental appends
 */
export class StreamBuilder {
  private store: Store;
  private chunker: Chunker;

  // Current partial chunk being built
  private buffer: Uint8Array;
  private bufferOffset: number = 0;

  // Completed chunks
  private chunks: Link[] = [];
  private totalSize: number = 0;

  constructor(config: BuilderConfig) {
    this.store = config.store;
    this.chunker = config.chunker ?? fixedChunker(config.chunkSize ?? DEFAULT_CHUNK_SIZE);
    // Initialize buffer with first chunk size
    this.buffer = new Uint8Array(this.chunker(0));
  }

  /** Get current target chunk size */
  private currentChunkSize(): number {
    return this.chunker(this.chunks.length);
  }

  /**
   * Append data to the stream
   */
  async append(data: Uint8Array): Promise<void> {
    let offset = 0;

    while (offset < data.length) {
      const targetSize = this.currentChunkSize();

      // Resize buffer if needed for new chunk size
      if (this.buffer.length !== targetSize) {
        const newBuffer = new Uint8Array(targetSize);
        if (this.bufferOffset > 0) {
          newBuffer.set(this.buffer.subarray(0, this.bufferOffset));
        }
        this.buffer = newBuffer;
      }

      const space = targetSize - this.bufferOffset;
      const toWrite = Math.min(space, data.length - offset);

      this.buffer.set(data.subarray(offset, offset + toWrite), this.bufferOffset);
      this.bufferOffset += toWrite;
      offset += toWrite;

      // Flush full chunk
      if (this.bufferOffset === targetSize) {
        await this.flushChunk();
      }
    }

    this.totalSize += data.length;
  }

  /**
   * Flush current buffer as a chunk
   */
  private async flushChunk(): Promise<void> {
    if (this.bufferOffset === 0) return;

    const chunk = this.buffer.slice(0, this.bufferOffset);
    const hash = await sha256(chunk);
    await this.store.put(hash, new Uint8Array(chunk));

    this.chunks.push({ hash, size: chunk.length, type: LinkType.Blob });
    this.bufferOffset = 0;
  }

  /**
   * Get current root hash without finalizing
   * Useful for checkpoints
   */
  async currentRoot(): Promise<Hash | null> {
    if (this.chunks.length === 0 && this.bufferOffset === 0) {
      return null;
    }

    // Temporarily flush buffer
    const tempChunks = [...this.chunks];
    if (this.bufferOffset > 0) {
      const chunk = this.buffer.slice(0, this.bufferOffset);
      const hash = await sha256(chunk);
      await this.store.put(hash, new Uint8Array(chunk));
      tempChunks.push({ hash, size: chunk.length, type: LinkType.Blob });
    }

    return this.buildTreeFromChunks(tempChunks, this.totalSize);
  }

  /**
   * Finalize the stream and return root hash
   */
  async finalize(): Promise<{ hash: Hash; size: number }> {
    // Flush remaining buffer
    await this.flushChunk();

    if (this.chunks.length === 0) {
      // Empty stream - return hash of empty data
      const emptyHash = await sha256(new Uint8Array(0));
      await this.store.put(emptyHash, new Uint8Array(0));
      return { hash: emptyHash, size: 0 };
    }

    const hash = await this.buildTreeFromChunks(this.chunks, this.totalSize);
    return { hash, size: this.totalSize };
  }

  /**
   * Build flat tree from chunks (for streaming files)
   */
  private async buildTreeFromChunks(chunks: Link[], _totalSize: number): Promise<Hash> {
    if (chunks.length === 1) {
      return chunks[0].hash;
    }

    // Create single flat node with all links
    const node: TreeNode = {
      type: LinkType.File,
      links: chunks,
    };
    const { data, hash } = await encodeAndHash(node);
    await this.store.put(hash, data);
    return hash;
  }

  /**
   * Get stats
   */
  get stats(): { chunks: number; buffered: number; totalSize: number } {
    return {
      chunks: this.chunks.length,
      buffered: this.bufferOffset,
      totalSize: this.totalSize,
    };
  }
}

