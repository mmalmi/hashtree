/**
 * Dexie-based IndexedDB store for hashtree blobs
 * More robust than raw IndexedDB - handles errors, upgrades, and stuck connections better
 */
import Dexie, { type Table } from 'dexie';
import type { Store, Hash } from '@hashtree/core';
import { toHex, fromHex } from '@hashtree/core';

interface BlobEntry {
  hashHex: string;
  data: Uint8Array;
  /** Timestamp when blob was last accessed (for LRU eviction) */
  lastAccess: number;
}

interface BlobAccessEntry {
  hashHex: string;
  lastAccess: number;
}

const LAST_ACCESS_FLUSH_DELAY_MS = 50;

class HashTreeDB extends Dexie {
  blobs!: Table<BlobEntry, string>;
  accesses!: Table<BlobAccessEntry, string>;

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

    // Version 3: Track access timestamps in a separate table so read hits
    // don't need to rewrite blob payload rows.
    this.version(3).stores({
      blobs: '&hashHex, lastAccess',
      accesses: '&hashHex, lastAccess',
    }).upgrade(async (tx) => {
      const blobs = await tx.table('blobs').toArray() as BlobEntry[];
      const accessesTable = tx.table('accesses');
      await Promise.all(
        blobs.map((blob) =>
          accessesTable.put({
            hashHex: blob.hashHex,
            lastAccess: blob.lastAccess ?? Date.now(),
          }),
        ),
      );
    });
  }
}

/**
 * Dexie-based Store implementation
 * Drop-in replacement for IndexedDBStore with better error handling
 */
export class DexieStore implements Store {
  private db: HashTreeDB;
  private pendingLastAccessUpdates = new Map<string, number>();
  private lastAccessFlushPromise: Promise<void> | null = null;
  private lastAccessFlushTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(dbName: string = 'hashtree') {
    this.db = new HashTreeDB(dbName);
  }

  private scheduleLastAccessTouch(hashHex: string): void {
    this.pendingLastAccessUpdates.set(hashHex, Date.now());
    if (this.lastAccessFlushTimer !== null) {
      return;
    }

    this.lastAccessFlushTimer = setTimeout(() => {
      this.lastAccessFlushTimer = null;
      void this.flushPendingLastAccessUpdates();
    }, LAST_ACCESS_FLUSH_DELAY_MS);
  }

  private async flushPendingLastAccessUpdates(): Promise<void> {
    if (this.lastAccessFlushTimer !== null) {
      clearTimeout(this.lastAccessFlushTimer);
      this.lastAccessFlushTimer = null;
    }

    if (this.lastAccessFlushPromise) {
      await this.lastAccessFlushPromise;
      if (this.pendingLastAccessUpdates.size === 0) {
        return;
      }
    }

    if (this.pendingLastAccessUpdates.size === 0) {
      return;
    }

    const updates = Array.from(
      this.pendingLastAccessUpdates,
      ([hashHex, lastAccess]) => ({ hashHex, lastAccess }),
    );
    this.pendingLastAccessUpdates.clear();

    const pending = this.db.accesses
      .bulkPut(updates)
      .then(() => undefined)
      .catch((e) => {
        console.error('[DexieStore] lastAccess flush error:', e);
      })
      .finally(() => {
        this.lastAccessFlushPromise = null;
      });

    this.lastAccessFlushPromise = pending;
    await pending;

    if (this.pendingLastAccessUpdates.size > 0) {
      await this.flushPendingLastAccessUpdates();
    }
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    const hashHex = toHex(hash);
    const lastAccess = Date.now();
    try {
      await this.flushPendingLastAccessUpdates();
      await this.db.transaction('rw', this.db.blobs, this.db.accesses, async () => {
        // Store directly - IDB will clone the data internally
        await this.db.blobs.put({ hashHex, data, lastAccess });
        await this.db.accesses.put({ hashHex, lastAccess });
      });
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

      // Batch LRU touch updates so hot read paths stay read-heavy.
      this.scheduleLastAccessTouch(hashHex);

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
      await this.flushPendingLastAccessUpdates();
      const existed = await this.has(hash);
      if (existed) {
        await this.db.transaction('rw', this.db.blobs, this.db.accesses, async () => {
          await this.db.blobs.delete(hashHex);
          await this.db.accesses.delete(hashHex);
        });
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
      await this.flushPendingLastAccessUpdates();
      await this.db.transaction('rw', this.db.blobs, this.db.accesses, async () => {
        await this.db.blobs.clear();
        await this.db.accesses.clear();
      });
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
  async evict(maxBytes: number, protectedHashes: ReadonlySet<string> = new Set()): Promise<number> {
    try {
      await this.flushPendingLastAccessUpdates();
      const currentBytes = await this.totalBytes();
      if (currentBytes <= maxBytes) return 0;

      // Get entries sorted by lastAccess (oldest first) from the lightweight access table.
      const entries = await this.db.accesses.orderBy('lastAccess').toArray();

      let bytesRemoved = 0;
      let entriesRemoved = 0;
      const targetRemoval = currentBytes - maxBytes;

      for (const entry of entries) {
        if (bytesRemoved >= targetRemoval) break;
        if (protectedHashes.has(entry.hashHex)) continue;

        const blob = await this.db.blobs.get(entry.hashHex);
        await this.db.transaction('rw', this.db.blobs, this.db.accesses, async () => {
          await this.db.blobs.delete(entry.hashHex);
          await this.db.accesses.delete(entry.hashHex);
        });
        bytesRemoved += blob?.data.byteLength ?? 0;
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
    if (this.lastAccessFlushTimer !== null) {
      clearTimeout(this.lastAccessFlushTimer);
      this.lastAccessFlushTimer = null;
    }
    this.pendingLastAccessUpdates.clear();
    this.db.close();
  }

  /**
   * Delete the entire database
   */
  static async deleteDatabase(dbName: string = 'hashtree'): Promise<void> {
    await Dexie.delete(dbName);
  }
}
