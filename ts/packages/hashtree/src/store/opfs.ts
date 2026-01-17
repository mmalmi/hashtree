/**
 * OPFS (Origin Private File System) content-addressed store
 * Persistent browser storage using the File System Access API
 *
 * Files are stored as: <dirName>/<first2chars>/<hash>.bin
 * This provides sharding to avoid too many files in one directory.
 */

import { Store, Hash, toHex, fromHex } from '../types.js';

const DEFAULT_DIR_NAME = 'hashtree';

export interface OpfsStoreOptions {
  /** Directory name in OPFS root (default: 'hashtree') */
  dirName?: string;
}

export class OpfsStore implements Store {
  private dirName: string;
  private rootDir: FileSystemDirectoryHandle | null = null;
  private storeDir: FileSystemDirectoryHandle | null = null;
  private initPromise: Promise<FileSystemDirectoryHandle> | null = null;

  constructor(options: OpfsStoreOptions | string = DEFAULT_DIR_NAME) {
    if (typeof options === 'string') {
      this.dirName = options;
    } else {
      this.dirName = options.dirName ?? DEFAULT_DIR_NAME;
    }
  }

  /**
   * Get or create the store directory (with deduplication)
   */
  private async getStoreDir(): Promise<FileSystemDirectoryHandle> {
    if (this.storeDir) return this.storeDir;

    // Deduplicate initialization
    if (this.initPromise) return this.initPromise;

    this.initPromise = (async () => {
      this.rootDir = await navigator.storage.getDirectory();
      this.storeDir = await this.rootDir.getDirectoryHandle(this.dirName, { create: true });
      return this.storeDir;
    })();

    return this.initPromise;
  }

  /**
   * Get shard directory for a hash (first 2 hex chars)
   */
  private async getShardDir(hashHex: string, create: boolean): Promise<FileSystemDirectoryHandle | null> {
    const storeDir = await this.getStoreDir();
    const shard = hashHex.slice(0, 2);

    try {
      return await storeDir.getDirectoryHandle(shard, { create });
    } catch {
      return null;
    }
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const hashHex = toHex(hash);

    try {
      const shardDir = await this.getShardDir(hashHex, true);
      if (!shardDir) return false;

      // Check if file already exists
      try {
        await shardDir.getFileHandle(`${hashHex}.bin`, { create: false });
        return false; // Already exists
      } catch {
        // File doesn't exist, continue to create
      }

      const handle = await shardDir.getFileHandle(`${hashHex}.bin`, { create: true });
      const writable = await handle.createWritable();
      await writable.write(data as unknown as ArrayBuffer);
      await writable.close();

      return true;
    } catch (err) {
      console.error(`OPFS write failed for ${hashHex}:`, err);
      return false;
    }
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    if (!hash) return null;

    const hashHex = toHex(hash);
    const shardDir = await this.getShardDir(hashHex, false);
    if (!shardDir) return null;

    try {
      const handle = await shardDir.getFileHandle(`${hashHex}.bin`, { create: false });
      const file = await handle.getFile();
      const buffer = await file.arrayBuffer();
      return new Uint8Array(buffer);
    } catch {
      return null;
    }
  }

  async has(hash: Hash): Promise<boolean> {
    const hashHex = toHex(hash);
    const shardDir = await this.getShardDir(hashHex, false);
    if (!shardDir) return false;

    try {
      await shardDir.getFileHandle(`${hashHex}.bin`, { create: false });
      return true;
    } catch {
      return false;
    }
  }

  async delete(hash: Hash): Promise<boolean> {
    const hashHex = toHex(hash);
    const shardDir = await this.getShardDir(hashHex, false);
    if (!shardDir) return false;

    try {
      await shardDir.removeEntry(`${hashHex}.bin`);
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Get all stored hashes
   */
  async keys(): Promise<Hash[]> {
    const storeDir = await this.getStoreDir();
    const hashes: Hash[] = [];

    // @ts-ignore - entries() exists on FileSystemDirectoryHandle
    for await (const [, shardHandle] of storeDir.entries()) {
      if (shardHandle.kind !== 'directory') continue;

      // @ts-ignore
      for await (const [fileName] of shardHandle.entries()) {
        if (fileName.endsWith('.bin')) {
          const hashHex = fileName.slice(0, -4);
          hashes.push(fromHex(hashHex));
        }
      }
    }

    return hashes;
  }

  /**
   * Clear all data
   */
  async clear(): Promise<void> {
    const storeDir = await this.getStoreDir();

    // @ts-ignore
    for await (const [name, handle] of storeDir.entries()) {
      if (handle.kind === 'directory') {
        await storeDir.removeEntry(name, { recursive: true });
      }
    }
  }

  /**
   * Get count of stored items
   */
  async count(): Promise<number> {
    const keys = await this.keys();
    return keys.length;
  }

  /**
   * Get total bytes stored
   */
  async totalBytes(): Promise<number> {
    const storeDir = await this.getStoreDir();
    let total = 0;

    // @ts-ignore
    for await (const [, shardHandle] of storeDir.entries()) {
      if (shardHandle.kind !== 'directory') continue;

      // @ts-ignore
      for await (const [, fileHandle] of shardHandle.entries()) {
        if (fileHandle.kind === 'file') {
          const file = await fileHandle.getFile();
          total += file.size;
        }
      }
    }

    return total;
  }

  /**
   * Close - cleanup any resources
   */
  async close(): Promise<void> {
    this.rootDir = null;
    this.storeDir = null;
    this.initPromise = null;
  }

  /**
   * Delete the entire store directory
   */
  static async deleteStore(dirName: string = DEFAULT_DIR_NAME): Promise<void> {
    const root = await navigator.storage.getDirectory();
    try {
      await root.removeEntry(dirName, { recursive: true });
    } catch {
      // Directory might not exist
    }
  }
}
