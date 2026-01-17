/**
 * HashTree - Unified merkle tree operations
 *
 * Single class for creating, reading, and editing content-addressed merkle trees.
 *
 * All files are encrypted by default using CHK (Content Hash Key) encryption.
 * Use the "Public" variants (putFilePublic, readFilePublic) for unencrypted storage.
 */

import { Store, Hash, CID, TreeNode, LinkType, toHex, cid } from './types.js';
import { tryDecodeTreeNode } from './codec.js';
import { StreamWriter } from './streaming.js';
import * as streaming from './streaming.js';
export { StreamWriter } from './streaming.js';
export { verifyTree } from './verify.js';
import * as create from './tree/create.js';
import * as read from './tree/read.js';
import * as edit from './tree/edit.js';
import {
  putFileEncrypted,
  readFileEncrypted,
  readFileEncryptedStream,
  readFileEncryptedRange,
  putDirectoryEncrypted,
  listDirectoryEncrypted,
  getTreeNodeEncrypted,
  type EncryptedDirEntry,
} from './encrypted.js';
import * as editEncrypted from './tree/editEncrypted.js';
import * as writeAtOps from './tree/writeAt.js';

/** Default chunk size: 2MB (optimized for blossom uploads) */
export const DEFAULT_CHUNK_SIZE = 2 * 1024 * 1024;

export interface HashTreeConfig {
  store: Store;
  chunkSize?: number;
}

export interface TreeEntry {
  name: string;
  cid: CID;
  size: number;
  type: LinkType;
  meta?: Record<string, unknown>;
}

export interface DirEntry {
  name: string;
  cid: CID;
  size: number;
  type: LinkType;
  meta?: Record<string, unknown>;
}

/**
 * HashTree - create, read, and edit merkle trees
 */
export class HashTree {
  private store: Store;
  private chunkSize: number;

  constructor(config: HashTreeConfig) {
    this.store = config.store;
    this.chunkSize = config.chunkSize ?? DEFAULT_CHUNK_SIZE;
  }

  private get config(): create.CreateConfig {
    return { store: this.store, chunkSize: this.chunkSize };
  }

  // Create (encrypted by default)

  async putBlob(data: Uint8Array): Promise<Hash> {
    return create.putBlob(this.store, data);
  }

  /**
   * Store a file
   * @param data - File data to store
   * @param options - { unencrypted?: boolean } - if true, store without encryption
   * @returns { cid, size }
   */
  async putFile(
    data: Uint8Array,
    options?: { unencrypted?: boolean }
  ): Promise<{ cid: CID; size: number }> {
    if (options?.unencrypted) {
      const result = await create.putFile(this.config, data);
      return { cid: { hash: result.hash }, size: result.size };
    }
    const result = await putFileEncrypted(this.config, data);
    return { cid: cid(result.hash, result.key), size: result.size };
  }

  /**
   * Store a directory
   * @param entries - Directory entries
   * @param options - { unencrypted?: boolean } - if true, store without encryption
   * @returns { cid, size }
   */
  async putDirectory(
    entries: DirEntry[],
    options?: { unencrypted?: boolean }
  ): Promise<{ cid: CID; size: number }> {
    const size = entries.reduce((sum, e) => sum + e.size, 0);
    if (options?.unencrypted) {
      const dirEntries: create.DirEntry[] = entries.map(e => ({
        name: e.name,
        cid: e.cid,
        size: e.size,
        type: e.type,
        meta: e.meta,
      }));
      const hash = await create.putDirectory(this.config, dirEntries);
      return { cid: { hash }, size };
    }
    // Encrypted by default
    const encryptedEntries: EncryptedDirEntry[] = entries.map(e => ({
      name: e.name,
      hash: e.cid.hash,
      size: e.size,
      key: e.cid.key,
      type: e.type,
      meta: e.meta,
    }));
    const result = await putDirectoryEncrypted(this.config, encryptedEntries);
    return { cid: cid(result.hash, result.key), size };
  }

  // Read

  async getBlob(hash: Hash): Promise<Uint8Array | null> {
    return read.getBlob(this.store, hash);
  }

  /**
   * Get a tree node
   */
  async getTreeNode(id: CID): Promise<TreeNode | null> {
    if (id.key) {
      return getTreeNodeEncrypted(this.store, id.hash, id.key);
    }
    return read.getTreeNode(this.store, id.hash);
  }

  async getType(id: CID): Promise<LinkType> {
    if (!id?.hash) return LinkType.Blob;
    return read.getType(this.store, id.hash);
  }

  async isDirectory(id: CID): Promise<boolean> {
    if (!id?.hash) return false;
    // For encrypted directories, we need to decrypt to check the node type
    if (id.key) {
      try {
        // Try to get the tree node (will decrypt and validate)
        const node = await getTreeNodeEncrypted(this.store, id.hash, id.key);
        if (!node) return false;
        // Empty directory is still a directory
        if (node.links.length === 0) return true;
        // Check if it's a directory (has named entries) vs chunked file (no names)
        return node.links.some(l => l.name !== undefined && !l.name.startsWith('_'));
      } catch {
        return false;
      }
    }
    return read.isDirectory(this.store, id.hash);
  }

  /**
   * Read a file
   */
  async readFile(id: CID): Promise<Uint8Array | null> {
    if (id.key) {
      return readFileEncrypted(this.store, id.hash, id.key);
    }
    return read.readFile(this.store, id.hash);
  }

  /**
   * Stream a file
   * @param id - CID of the file
   * @param options - { offset?: number, prefetch?: number }
   */
  async *readFileStream(
    id: CID,
    options: { offset?: number; prefetch?: number } = {}
  ): AsyncGenerator<Uint8Array> {
    if (id.key) {
      yield* readFileEncryptedStream(this.store, id.hash, id.key, options);
    } else {
      yield* read.readFileStream(this.store, id.hash, options);
    }
  }

  /**
   * Read a range of bytes from a file
   */
  async readFileRange(id: CID, start: number, end?: number): Promise<Uint8Array | null> {
    if (id.key) {
      return readFileEncryptedRange(this.store, id.hash, id.key, start, end);
    }
    return read.readFileRange(this.store, id.hash, start, end);
  }

  /**
   * List directory entries
   */
  async listDirectory(id: CID): Promise<TreeEntry[]> {
    if (id.key) {
      const entries = await listDirectoryEncrypted(this.store, id.hash, id.key);
      return entries.map(e => ({
        name: e.name,
        cid: cid(e.hash, e.key),
        size: e.size,
        type: e.type ?? LinkType.Blob,
        meta: e.meta,
      }));
    }
    const entries = await read.listDirectory(this.store, id.hash);
    return entries.map(e => ({
      name: e.name,
      cid: e.cid,
      size: e.size,
      type: e.type,
      meta: e.meta,
    }));
  }

  /**
   * Resolve a path to get the entry's CID
   *
   * @param root - Root CID of the tree
   * @param path - Path to resolve (string like 'a/b/file.txt' or array like ['a', 'b', 'file.txt'])
   * @returns { cid, type } or null if not found
   */
  async resolvePath(
    root: CID,
    path: string | string[]
  ): Promise<{ cid: CID; type: LinkType } | null> {
    const parts = Array.isArray(path)
      ? path
      : path.split('/').filter(p => p.length > 0);

    let current = root;
    let entryType: LinkType = LinkType.Dir;

    for (const segment of parts) {
      const entries = await this.listDirectory(current);
      const entry = entries.find(e => e.name === segment);
      if (!entry) {
        return null;
      }

      current = entry.cid;
      entryType = entry.type;
    }

    return { cid: current, type: entryType };
  }

  async getSize(hash: Hash): Promise<number> {
    return read.getSize(this.store, hash);
  }

  async *walk(
    hash: Hash,
    path: string = ''
  ): AsyncGenerator<{ path: string; hash: Hash; type: LinkType; size?: number }> {
    yield* read.walk(this.store, hash, path);
  }

  /**
   * Walk entire tree and return all entries as an array
   * More convenient than the async generator for most use cases
   * @param id - Root CID of the tree
   * @param options.includeData - If true, include file data in results
   * @param options.filesOnly - If true, only return files (not directories)
   */
  async walkTree(
    id: CID,
    options?: { includeData?: boolean; filesOnly?: boolean }
  ): Promise<Array<{ path: string; cid: CID; type: LinkType; size: number; data?: Uint8Array }>> {
    const results: Array<{ path: string; cid: CID; type: LinkType; size: number; data?: Uint8Array }> = [];

    const walkRecursive = async (currentCid: CID, currentPath: string): Promise<void> => {
      const isDir = await this.isDirectory(currentCid);

      if (isDir) {
        if (!options?.filesOnly) {
          const entries = await this.listDirectory(currentCid);
          const dirSize = entries.reduce((sum, e) => sum + e.size, 0);
          results.push({ path: currentPath, cid: currentCid, type: LinkType.Dir, size: dirSize });
        }

        const entries = await this.listDirectory(currentCid);
        for (const entry of entries) {
          const childPath = currentPath ? `${currentPath}/${entry.name}` : entry.name;
          await walkRecursive(entry.cid, childPath);
        }
      } else {
        // File or blob
        const type = await this.getType(currentCid);
        const size = await this.getSize(currentCid.hash);
        const result: { path: string; cid: CID; type: LinkType; size: number; data?: Uint8Array } = {
          path: currentPath,
          cid: currentCid,
          type,
          size,
        };

        if (options?.includeData) {
          const data = await this.readFile(currentCid);
          if (data) result.data = data;
        }

        results.push(result);
      }
    };

    await walkRecursive(id, '');
    return results;
  }

  /**
   * Iterate over all raw blocks in a merkle tree
   * Yields each block's hash and data, traversing encrypted nodes correctly
   * Useful for syncing to remote stores (e.g., Blossom push)
   */
  async *walkBlocks(id: CID): AsyncGenerator<{ hash: Hash; data: Uint8Array }> {
    const visited = new Set<string>();

    const traverse = async function* (
      store: Store,
      hash: Hash,
      key?: Uint8Array
    ): AsyncGenerator<{ hash: Hash; data: Uint8Array }> {
      const hex = toHex(hash);
      if (visited.has(hex)) return;
      visited.add(hex);

      const data = await store.get(hash);
      if (!data) return;

      yield { hash, data };

      // Handle encrypted vs unencrypted tree nodes
      if (key) {
        const decrypted = await getTreeNodeEncrypted(store, hash, key);
        if (decrypted) {
          for (const link of decrypted.links) {
            yield* traverse(store, link.hash, link.key);
          }
        }
      } else {
        const node = tryDecodeTreeNode(data);
        if (node) {
          for (const link of node.links) {
            yield* traverse(store, link.hash, link.key);
          }
        }
      }
    };

    yield* traverse(this.store, id.hash, id.key);
  }

  /**
   * Pull (fetch) all chunks for a tree recursively
   * Triggers WebRTC fetches for any missing chunks
   * Uses parallel fetching within each tree level for better performance
   * @returns { cid, chunks, bytes } - The CID and stats about what was pulled
   */
  async pull(id: CID): Promise<{ cid: CID; chunks: number; bytes: number }> {
    const visited = new Set<string>();
    let chunks = 0;
    let bytes = 0;

    const fetch = async (hash: Hash, key?: Uint8Array): Promise<void> => {
      const hex = toHex(hash);
      if (visited.has(hex)) return;
      visited.add(hex);

      // Fetch the chunk (will go to WebRTC peers if not local)
      const data = await this.store.get(hash);
      if (!data) {
        return;
      }

      chunks++;
      bytes += data.length;

      // If there's an encryption key, try to decrypt and check if it's a tree node
      // For encrypted data, we can't use isTreeNode on the raw bytes
      if (key) {
        const decrypted = await getTreeNodeEncrypted(this.store, hash, key);
        if (decrypted) {
          // It's an encrypted tree node - fetch all children in parallel
          await Promise.all(decrypted.links.map(link => fetch(link.hash, link.key)));
        }
        // If decryption failed or not a tree node, it's a blob (already fetched)
      } else {
        // Unencrypted data - check directly
        const node = tryDecodeTreeNode(data);
        if (node) {
          // Fetch all children in parallel
          await Promise.all(node.links.map(link => fetch(link.hash, link.key)));
        }
        // If not a tree node, it's a blob (already fetched)
      }
    };

    await fetch(id.hash, id.key);
    return { cid: id, chunks, bytes };
  }

  /**
   * Push all chunks for a tree to a target store
   * Useful for syncing to remote stores (e.g., Blossom servers)
   * @param id - CID of the tree to push
   * @param targetStore - Store to push blocks to (must support put())
   * @param options - callbacks for progress and per-block status
   * @returns { cid, pushed, skipped, failed, bytes, errors } - The CID and detailed stats
   */
  async push(
    id: CID,
    targetStore: Store,
    options?: {
      onProgress?: (current: number, total: number) => void;
      onBlock?: (hash: Hash, status: 'success' | 'skipped' | 'error', error?: Error) => void;
      /** Number of parallel uploads (default: 4) */
      concurrency?: number;
      /** AbortSignal to cancel the push */
      signal?: AbortSignal;
    }
  ): Promise<{
    cid: CID;
    pushed: number;
    skipped: number;
    failed: number;
    bytes: number;
    errors: Array<{ hash: Hash; error: Error }>;
    cancelled: boolean;
  }> {
    // First pull to ensure all blocks are available locally
    await this.pull(id);

    // Check if already aborted
    if (options?.signal?.aborted) {
      return { cid: id, pushed: 0, skipped: 0, failed: 0, bytes: 0, errors: [], cancelled: true };
    }

    let pushed = 0;
    let skipped = 0;
    let failed = 0;
    let bytes = 0;
    let completed = 0;
    let discovered = 0;
    let walkDone = false;
    const errors: Array<{ hash: Hash; error: Error }> = [];

    const concurrency = options?.concurrency ?? 4;

    // Bounded queue for producer-consumer pattern
    const queue: Array<{ hash: Hash; data: Uint8Array }> = [];
    const maxQueueSize = concurrency * 2;
    let queueResolve: (() => void) | null = null;
    let consumerResolve: (() => void) | null = null;

    // Process a single block
    const processBlock = async (block: { hash: Hash; data: Uint8Array }) => {
      if (options?.signal?.aborted) return;

      const { hash, data } = block;
      try {
        const isNew = await targetStore.put(hash, data);
        if (isNew === false) {
          skipped++;
          options?.onBlock?.(hash, 'skipped');
        } else {
          pushed++;
          bytes += data.length;
          options?.onBlock?.(hash, 'success');
        }
      } catch (e) {
        failed++;
        const error = e instanceof Error ? e : new Error(String(e));
        errors.push({ hash, error });
        options?.onBlock?.(hash, 'error', error);
      }
      completed++;
      // Report progress: completed / discovered (discovered grows as we walk)
      options?.onProgress?.(completed, discovered);
    };

    // Consumer: pull from queue and process
    const runConsumer = async () => {
      const active: Promise<void>[] = [];

      while (!walkDone || queue.length > 0 || active.length > 0) {
        if (options?.signal?.aborted) break;

        // Start new tasks from queue
        while (active.length < concurrency && queue.length > 0) {
          const block = queue.shift()!;
          // Signal producer that queue has space
          if (queueResolve && queue.length < maxQueueSize) {
            queueResolve();
            queueResolve = null;
          }
          const promise = processBlock(block).then(() => {
            active.splice(active.indexOf(promise), 1);
          });
          active.push(promise);
        }

        // If queue empty but walk not done, wait for producer
        if (queue.length === 0 && !walkDone && active.length < concurrency) {
          await new Promise<void>(resolve => {
            consumerResolve = resolve;
          });
          continue;
        }

        // Wait for at least one task to complete
        if (active.length > 0) {
          await Promise.race(active);
        }
      }

      await Promise.all(active);
    };

    // Producer: walk tree and push to queue
    const runProducer = async () => {
      for await (const block of this.walkBlocks(id)) {
        if (options?.signal?.aborted) break;

        discovered++;

        // Wait if queue is full
        while (queue.length >= maxQueueSize) {
          await new Promise<void>(resolve => {
            queueResolve = resolve;
          });
          if (options?.signal?.aborted) break;
        }

        queue.push(block);

        // Wake up consumer
        if (consumerResolve) {
          consumerResolve();
          consumerResolve = null;
        }
      }
      walkDone = true;
      // Wake consumer one last time
      if (consumerResolve) {
        consumerResolve();
        consumerResolve = null;
      }
    };

    // Run producer and consumer in parallel
    await Promise.all([runProducer(), runConsumer()]);

    return { cid: id, pushed, skipped, failed, bytes, errors, cancelled: options?.signal?.aborted ?? false };
  }

  // Edit operations

  /**
   * Add or update an entry in a directory
   * @param root - Root CID of the tree
   * @param path - Path to the directory containing the entry
   * @param name - Name of the entry
   * @param entry - CID of the entry content
   * @param size - Size of the content
   * @param type - Type of the entry (LinkType.Blob, LinkType.File, or LinkType.Dir)
   * @returns New root CID
   */
  async setEntry(
    root: CID,
    path: string[],
    name: string,
    entry: CID,
    size: number,
    type: LinkType = LinkType.Blob,
    meta?: Record<string, unknown>
  ): Promise<CID> {
    if (root.key) {
      const result = await editEncrypted.setEntryEncrypted(
        this.config,
        root.hash,
        root.key,
        path,
        name,
        entry.hash,
        size,
        entry.key,
        type,
        meta
      );
      return cid(result.hash, result.key);
    }
    const hash = await edit.setEntry(this.config, root.hash, path, name, entry, size, type, meta);
    return { hash };
  }

  /**
   * Remove an entry from a directory
   * @param root - Root CID of the tree
   * @param path - Path to the directory containing the entry
   * @param name - Name of the entry to remove
   * @returns New root CID
   */
  async removeEntry(root: CID, path: string[], name: string): Promise<CID> {
    if (root.key) {
      const result = await editEncrypted.removeEntryEncrypted(
        this.config,
        root.hash,
        root.key,
        path,
        name
      );
      return cid(result.hash, result.key);
    }
    const hash = await edit.removeEntry(this.config, root.hash, path, name);
    return { hash };
  }

  /**
   * Rename an entry in a directory
   * @param root - Root CID of the tree
   * @param path - Path to the directory containing the entry
   * @param oldName - Current name
   * @param newName - New name
   * @returns New root CID
   */
  async renameEntry(
    root: CID,
    path: string[],
    oldName: string,
    newName: string
  ): Promise<CID> {
    if (root.key) {
      const result = await editEncrypted.renameEntryEncrypted(
        this.config,
        root.hash,
        root.key,
        path,
        oldName,
        newName
      );
      return cid(result.hash, result.key);
    }
    const hash = await edit.renameEntry(this.config, root.hash, path, oldName, newName);
    return { hash };
  }

  /**
   * Move an entry to a different directory
   * @param root - Root CID of the tree
   * @param sourcePath - Path to the source directory
   * @param name - Name of the entry to move
   * @param targetPath - Path to the target directory
   * @returns New root CID
   */
  async moveEntry(
    root: CID,
    sourcePath: string[],
    name: string,
    targetPath: string[]
  ): Promise<CID> {
    if (root.key) {
      const result = await editEncrypted.moveEntryEncrypted(
        this.config,
        root.hash,
        root.key,
        sourcePath,
        name,
        targetPath
      );
      return cid(result.hash, result.key);
    }
    const hash = await edit.moveEntry(this.config, root.hash, sourcePath, name, targetPath);
    return { hash };
  }

  /**
   * Write data at a specific offset in a file
   * Only affected chunks are rewritten - efficient for small patches to large files.
   * Useful for updating file headers (e.g., WebM duration) without rewriting entire file.
   *
   * @param fileCid - CID of the file to modify
   * @param offset - Byte offset to write at
   * @param data - Data to write
   * @returns New file CID
   */
  async writeAt(fileCid: CID, offset: number, data: Uint8Array): Promise<CID> {
    if (fileCid.key) {
      const result = await writeAtOps.writeAtEncrypted(
        this.config,
        fileCid.hash,
        fileCid.key,
        offset,
        data
      );
      return cid(result.hash, result.key);
    }
    const result = await writeAtOps.writeAt(this.config, fileCid.hash, offset, data);
    return { hash: result.hash };
  }

  // Utility

  getStore(): Store {
    return this.store;
  }

  /**
   * Create a streaming file writer for incremental appends
   * Useful for writing large files chunk by chunk (e.g., video recording)
   * @param options.unencrypted - if true, create without encryption
   * @param options.chunker - custom chunker for variable chunk sizes (e.g., videoChunker for faster start)
   */
  createStream(options?: { unencrypted?: boolean; chunker?: streaming.Chunker }): StreamWriter {
    return new StreamWriter({
      store: this.store,
      chunkSize: this.chunkSize,
      chunker: options?.chunker,
      unencrypted: options?.unencrypted ?? false,
    });
  }
}

