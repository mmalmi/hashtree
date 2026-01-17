/**
 * StreamWriter - supports incremental file appends
 *
 * Created via HashTree.createStream()
 *
 * All chunks are CHK encrypted by default (same as putFile).
 * Use createStream({ unencrypted: true }) for unencrypted streaming.
 */

import { Store, Hash, CID, TreeNode, LinkType, Link, cid } from './types.js';
import { encodeAndHash } from './codec.js';
import { sha256 } from './hash.js';
import { encryptChk, type EncryptionKey } from './crypto.js';
import { type Chunker, fixedChunker } from './builder.js';
export type { Chunker } from './builder.js';

export interface StreamWriterConfig {
  store: Store;
  chunkSize?: number;
  chunker?: Chunker;
  unencrypted?: boolean;
}

export class StreamWriter {
  private store: Store;
  private chunker: Chunker;
  private unencrypted: boolean;

  // Current partial chunk being built
  private buffer: Uint8Array;
  private bufferOffset: number = 0;

  // Completed chunks (with encryption keys for tree building when encrypted)
  private chunks: Link[] = [];
  private totalSize: number = 0;

  constructor(config: StreamWriterConfig);
  /** @deprecated Use config object instead */
  constructor(store: Store, chunkSize: number, maxLinks: number, unencrypted?: boolean);
  constructor(
    storeOrConfig: Store | StreamWriterConfig,
    chunkSize?: number,
    _maxLinks?: number,
    unencrypted?: boolean
  ) {
    if ('store' in storeOrConfig && typeof storeOrConfig === 'object' && !('get' in storeOrConfig)) {
      // New config-based constructor
      const config = storeOrConfig as StreamWriterConfig;
      this.store = config.store;
      this.chunker = config.chunker ?? fixedChunker(config.chunkSize ?? 2 * 1024 * 1024);
      this.unencrypted = config.unencrypted ?? false;
    } else {
      // Legacy positional constructor (maxLinks ignored)
      this.store = storeOrConfig as Store;
      this.chunker = fixedChunker(chunkSize!);
      this.unencrypted = unencrypted ?? false;
    }
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
   * Flush current buffer as a chunk (encrypted or plaintext based on mode)
   */
  private async flushChunk(): Promise<void> {
    if (this.bufferOffset === 0) return;

    const chunkSize = this.bufferOffset;
    // slice() to create independent copy - allows buffer reuse after await
    const chunk = this.buffer.slice(0, chunkSize);

    if (this.unencrypted) {
      // Public mode: store plaintext
      const hash = await sha256(chunk);
      await this.store.put(hash, chunk);
      this.chunks.push({ hash, size: chunkSize, type: LinkType.Blob });
    } else {
      // Encrypted mode: CHK encrypt the chunk
      const { ciphertext, key } = await encryptChk(chunk);
      const hash = await sha256(ciphertext);
      await this.store.put(hash, ciphertext);
      this.chunks.push({ hash, size: chunkSize, key, type: LinkType.Blob });
    }

    this.bufferOffset = 0;
  }

  /**
   * Get current root CID without finalizing
   * Useful for checkpoints (e.g., live streaming)
   * Returns CID with key for encrypted streams, CID without key for public streams
   */
  async currentRoot(): Promise<CID | null> {
    if (this.chunks.length === 0 && this.bufferOffset === 0) {
      return null;
    }

    // Temporarily store buffer without modifying state
    const tempChunks = [...this.chunks];
    if (this.bufferOffset > 0) {
      const chunk = this.buffer.slice(0, this.bufferOffset);

      if (this.unencrypted) {
        const hash = await sha256(chunk);
        await this.store.put(hash, chunk);
        tempChunks.push({ hash, size: chunk.length, type: LinkType.Blob });
      } else {
        // Store PLAINTEXT size in link.size for correct range seeking
        const plaintextSize = chunk.length;
        const { ciphertext, key } = await encryptChk(chunk);
        const hash = await sha256(ciphertext);
        await this.store.put(hash, ciphertext);
        tempChunks.push({ hash, size: plaintextSize, key, type: LinkType.Blob });
      }
    }

    return this.buildTreeFromChunks(tempChunks, this.totalSize);
  }

  /**
   * Finalize the stream and return root CID
   * For encrypted streams: returns { hash, size, key }
   * For public streams: returns { hash, size } (key is undefined)
   */
  async finalize(): Promise<{ hash: Hash; size: number; key?: EncryptionKey }> {
    // Flush remaining buffer
    await this.flushChunk();

    if (this.chunks.length === 0) {
      // Empty stream
      if (this.unencrypted) {
        const emptyData = new Uint8Array(0);
        const hash = await sha256(emptyData);
        await this.store.put(hash, emptyData);
        return { hash, size: 0 };
      } else {
        const { ciphertext, key } = await encryptChk(new Uint8Array(0));
        const hash = await sha256(ciphertext);
        await this.store.put(hash, ciphertext);
        return { hash, size: 0, key };
      }
    }

    const result = await this.buildTreeFromChunks(this.chunks, this.totalSize);
    return { hash: result.hash, size: this.totalSize, key: result.key };
  }

  /**
   * Build flat tree from chunks
   */
  private async buildTreeFromChunks(chunks: Link[], _totalSize: number): Promise<CID> {
    // Single chunk - return its hash (and key if encrypted)
    if (chunks.length === 1) {
      return cid(chunks[0].hash, chunks[0].key);
    }

    // Create single flat node with all links
    const node: TreeNode = {
      type: LinkType.File,
      links: chunks,
    };
    const { data, hash: nodeHash } = await encodeAndHash(node);

    if (this.unencrypted) {
      // Public mode: store plaintext tree node
      await this.store.put(nodeHash, data);
      return { hash: nodeHash };
    } else {
      // Encrypted mode: CHK encrypt the tree node
      const { ciphertext, key } = await encryptChk(data);
      const hash = await sha256(ciphertext);
      await this.store.put(hash, ciphertext);
      return cid(hash, key);
    }
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

  /**
   * Clear internal state to release memory
   * Call after finalize() if you want to free memory immediately
   */
  clear(): void {
    this.chunks = [];
    this.buffer = new Uint8Array(0);
    this.bufferOffset = 0;
    this.totalSize = 0;
  }
}
