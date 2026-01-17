/**
 * Dexie-based IndexedDB store for hashtree blobs
 * More robust than raw IndexedDB - handles errors, upgrades, and stuck connections better
 */
import Dexie, { type Table } from 'dexie';
import type { Store, Hash } from '../types.js';
import { toHex, fromHex } from '../types.js';

interface BlobEntry {
  hashHex: string;
  data: Uint8Array;
  /** Timestamp when blob was last accessed (for LRU eviction) */
  lastAccess: number;
}

class HashTreeDB extends Dexie {
  blobs!: Table<BlobEntry, string>;

  constructor(dbName: string) {
    super(dbName);

    // Version 1: Original schema without lastAccess
    this.version(1).stores({
      blobs: '&hashHex',
    });

    // Version 2: Add lastAccess field for LRU eviction
    this.version(2).stores({
      blobs: '&hashHex, lastAccess',
    }).upgrade(tx => {
      // Add lastAccess to existing entries
      const now = Date.now();
      return tx.table('blobs').toCollection().modify(blob => {
        blob.lastAccess = now;
      });
    });
  }
}

/**
 * Dexie-based Store implementation
 * Drop-in replacement for IndexedDBStore with better error handling
 */
export class DexieStore implements Store {
  private db: HashTreeDB;

  constructor(dbName: string = 'hashtree') {
    this.db = new HashTreeDB(dbName);
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const hashHex = toHex(hash);
    try {
      // Store directly - IDB will clone the data internally
      await this.db.blobs.put({ hashHex, data, lastAccess: Date.now() });
      return true;
    } catch (e) {
      console.error('[DexieStore] put error:', e);
      return false;
    }
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    if (!hash) return null;
    const hashHex = toHex(hash);
    try {
      const entry = await this.db.blobs.get(hashHex);
      if (!entry) return null;

      // Update lastAccess timestamp for LRU tracking (fire-and-forget)
      // We need to re-put the entry to avoid fake-indexeddb corruption issues with partial updates
      this.db.blobs.put({ ...entry, lastAccess: Date.now() }).catch(() => {});

      // Return directly - IDB returns a fresh copy already
      // Only slice if the view doesn't match the buffer (rare edge case)
      const data = entry.data;
      if (data.byteOffset === 0 && data.byteLength === data.buffer.byteLength) {
        return data;
      }
      // Rare: view is a subset of a larger buffer, need to copy
      return new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
    } catch (e) {
      console.error('[DexieStore] get error:', e);
      return null;
    }
  }

  async has(hash: Hash): Promise<boolean> {
    const hashHex = toHex(hash);
    try {
      // Use count with where clause - doesn't load the blob data
      const count = await this.db.blobs.where('hashHex').equals(hashHex).count();
      return count > 0;
    } catch (e) {
      console.error('[DexieStore] has error:', e);
      return false;
    }
  }

  async delete(hash: Hash): Promise<boolean> {
    const hashHex = toHex(hash);
    try {
      const existed = await this.has(hash);
      if (existed) {
        await this.db.blobs.delete(hashHex);
        return true;
      }
      return false;
    } catch (e) {
      console.error('[DexieStore] delete error:', e);
      return false;
    }
  }

  /**
   * Get all stored hashes
   */
  async keys(): Promise<Hash[]> {
    try {
      // Only fetch the primary keys, not the blob data
      const hashHexes = await this.db.blobs.toCollection().primaryKeys();
      return hashHexes.map(hex => fromHex(hex));
    } catch (e) {
      console.error('[DexieStore] keys error:', e);
      return [];
    }
  }

  /**
   * Clear all data
   */
  async clear(): Promise<void> {
    try {
      await this.db.blobs.clear();
    } catch (e) {
      console.error('[DexieStore] clear error:', e);
    }
  }

  /**
   * Get count of stored items
   */
  async count(): Promise<number> {
    try {
      return await this.db.blobs.count();
    } catch (e) {
      console.error('[DexieStore] count error:', e);
      return 0;
    }
  }

  /**
   * Get total bytes stored
   * Uses cursor to avoid loading all blobs into memory at once
   */
  async totalBytes(): Promise<number> {
    try {
      let total = 0;
      await this.db.blobs.each(entry => {
        total += entry.data.byteLength;
      });
      return total;
    } catch (e) {
      console.error('[DexieStore] totalBytes error:', e);
      return 0;
    }
  }

  /**
   * Evict least-recently-used entries until totalBytes is below maxBytes.
   * Returns the number of entries deleted.
   */
  async evict(maxBytes: number): Promise<number> {
    try {
      const currentBytes = await this.totalBytes();
      if (currentBytes <= maxBytes) return 0;

      // Get entries sorted by lastAccess (oldest first)
      const entries = await this.db.blobs.orderBy('lastAccess').toArray();

      let bytesRemoved = 0;
      let entriesRemoved = 0;
      const targetRemoval = currentBytes - maxBytes;

      for (const entry of entries) {
        if (bytesRemoved >= targetRemoval) break;

        await this.db.blobs.delete(entry.hashHex);
        bytesRemoved += entry.data.byteLength;
        entriesRemoved++;
      }

      console.log(`[DexieStore] Evicted ${entriesRemoved} entries (${bytesRemoved} bytes)`);
      return entriesRemoved;
    } catch (e) {
      console.error('[DexieStore] evict error:', e);
      return 0;
    }
  }

  /**
   * Close the database connection
   */
  close(): void {
    this.db.close();
  }

  /**
   * Delete the entire database
   */
  static async deleteDatabase(dbName: string = 'hashtree'): Promise<void> {
    await Dexie.delete(dbName);
  }
}
