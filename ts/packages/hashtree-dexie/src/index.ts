/**
 * Dexie-based IndexedDB store for hashtree blobs
 * More robust than raw IndexedDB - handles errors, upgrades, and stuck connections better
 */
import Dexie, { type Table, type Transaction } from 'dexie';
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
  size: number;
  lastAccess: number;
}

const LAST_ACCESS_FLUSH_DELAY_MS = 50;
const BLOB_BATCH_SIZE = 64;

/** One bounded pass over legacy payloads. Preserve access times recorded since insertion. */
async function indexLegacySizes(tx: Transaction): Promise<void> {
  const blobs = tx.table<BlobEntry, string>('blobs');
  const accesses = tx.table<BlobAccessEntry, string>('accesses');
  let lastHash: string | undefined;
  let total = 0;
  while (true) {
    const page = await (lastHash === undefined ? blobs.orderBy(':id') : blobs.where(':id').above(lastHash))
      .limit(BLOB_BATCH_SIZE).toArray();
    if (!page.length) break;
    const previous = await accesses.bulkGet(page.map(blob => blob.hashHex));
    await accesses.bulkPut(page.map((blob, i) => {
      const size = blob.data.byteLength;
      total += size;
      return { hashHex: blob.hashHex, size, lastAccess: previous[i]?.lastAccess ?? blob.lastAccess };
    }));
    if (page.length < BLOB_BATCH_SIZE) break;
    lastHash = page[page.length - 1].hashHex;
  }
  await tx.table('stats').put({ id: 'bytes', value: total });
}

class HashTreeDB extends Dexie {
  blobs!: Table<BlobEntry, string>;
  accesses!: Table<BlobAccessEntry, string>;
  stats!: Table<{ id: string; value: number }, string>;

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
    });

    // Upgrades from v1/v2 also create the access rows during the size backfill.
    this.version(4).stores({
      accesses: '&hashHex, lastAccess, [lastAccess+hashHex]',
      stats: '&id',
    }).upgrade(indexLegacySizes);
  }

  async byteCount(): Promise<number> {
    return (await this.stats.get('bytes'))?.value ?? 0;
  }

  async adjustBytes(delta: number): Promise<void> {
    if (delta) await this.stats.put({ id: 'bytes', value: await this.byteCount() + delta });
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
  private pendingEviction: Promise<unknown> = Promise.resolve();
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
      ([hashHex, lastAccess]) => ({ key: hashHex, changes: { lastAccess } }),
    );
    this.pendingLastAccessUpdates.clear();

    const pending = this.db.accesses
      .bulkUpdate(updates)
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
    return this.putMany([{ hash, data }]);
  }

  /**
   * Store several blobs in one transaction. Existing entries are replaced.
   * @returns true when every entry was stored, false when the write failed
   */
  async putMany(entries: ReadonlyArray<{ hash: Hash; data: Uint8Array }>): Promise<boolean> {
    if (entries.length === 0) {
      return true;
    }
    const lastAccess = Date.now();
    const blobs = new Map<string, BlobEntry>();
    for (const { hash, data } of entries) {
      const hashHex = toHex(hash);
      blobs.set(hashHex, { hashHex, data, lastAccess });
    }
    const rows = Array.from(blobs.values());
    const accesses = rows.map(({ hashHex, data }) => ({ hashHex, size: data.byteLength, lastAccess }));
    try {
      await this.flushPendingLastAccessUpdates();
      await this.db.transaction('rw', this.db.blobs, this.db.accesses, this.db.stats, async () => {
        const old = await this.db.accesses.bulkGet(rows.map(row => row.hashHex));
        const delta = accesses.reduce((sum, row, i) => sum + row.size - (old[i]?.size ?? 0), 0);
        await this.db.blobs.bulkPut(rows);
        await this.db.accesses.bulkPut(accesses);
        await this.db.adjustBytes(delta);
      });
      return true;
    } catch (e) {
      console.error('[DexieStore] putMany error:', e);
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
      return await this.db.transaction('rw', this.db.blobs, this.db.accesses, this.db.stats, async () => {
        const entry = await this.db.accesses.get(hashHex);
        if (entry?.size === undefined) return false;
        await this.db.blobs.delete(hashHex);
        await this.db.accesses.delete(hashHex);
        await this.db.adjustBytes(-entry.size);
        return true;
      });
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
      await this.db.transaction('rw', this.db.blobs, this.db.accesses, this.db.stats, async () => {
        await this.db.stats.clear();
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

  /** Total payload bytes, maintained in the same transaction as every blob mutation. */
  async totalBytes(): Promise<number> {
    try {
      return await this.db.byteCount();
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
    const run = this.pendingEviction.then(() => this.evictBatches(maxBytes, protectedHashes));
    this.pendingEviction = run.catch(() => undefined);
    return run;
  }

  private async evictBatches(maxBytes: number, protectedHashes: ReadonlySet<string>): Promise<number> {
    try {
      await this.flushPendingLastAccessUpdates();
      let after: [number, string] | undefined;
      let bytesRemoved = 0;
      let entriesRemoved = 0;
      for (;;) {
        // Recheck the shared count in each transaction. Other tabs can evict or
        // replace blocks between batches without driving us below the limit.
        const finished = await this.db.transaction('rw', this.db.blobs, this.db.accesses, this.db.stats, async () => {
          const current = await this.db.byteCount();
          if (current <= maxBytes) return true;
          const index = '[lastAccess+hashHex]';
          const page = await (after ? this.db.accesses.where(index).above(after) : this.db.accesses.orderBy(index))
            .limit(BLOB_BATCH_SIZE).toArray();
          const keys: string[] = [];
          let bytes = 0;
          for (const entry of page) {
            after = [entry.lastAccess, entry.hashHex];
            if (entry.size === undefined || protectedHashes.has(entry.hashHex)) continue;
            keys.push(entry.hashHex);
            bytes += entry.size;
            if (current - bytes <= maxBytes) break;
          }
          if (keys.length) {
            await this.db.blobs.bulkDelete(keys);
            await this.db.accesses.bulkDelete(keys);
            await this.db.stats.put({ id: 'bytes', value: current - bytes });
            bytesRemoved += bytes;
            entriesRemoved += keys.length;
          }
          return current - bytes <= maxBytes || page.length < BLOB_BATCH_SIZE;
        });
        if (finished) break;
      }
      if (!entriesRemoved) return 0;
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
