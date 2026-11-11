import type { Hash, Store } from '@hashtree/core';

export interface StoreEntry {
  hash: Hash;
  data: Uint8Array;
}

type BatchStore = Store & {
  putMany?(entries: readonly StoreEntry[]): Promise<boolean>;
};

/**
 * Write-behind persistence for tree root state.
 *
 * Callers queue the newest record per key and never wait on the store to
 * update in-memory state. Queued work is written one round at a time: work
 * queued while a round is being written forms the next round, so a burst
 * costs a few grouped writes instead of one write per record, and a key's
 * values always reach the store in the order they were queued.
 */
export class RootStoreWriter<R> {
  private blobs: StoreEntry[] = [];
  private queued = new Map<string, R | null>();
  private writing = new Map<string, R | null>();
  private draining: Promise<void> | null = null;

  constructor(
    private readonly store: BatchStore | null,
    private readonly storageKey: (key: string) => Promise<Hash>,
    private readonly encode: (record: R) => Uint8Array,
  ) {}

  /** Queue the newest record for a key; `null` removes the stored record. */
  queueRecord(key: string, record: R | null, blobs: readonly StoreEntry[] = []): void {
    if (!this.store) return;
    this.queued.set(key, record);
    this.blobs.push(...blobs);
  }

  /**
   * The newest value for a key that may not have reached the store yet:
   * the record, `null` for a pending removal, or `undefined` when the store
   * is current.
   */
  pending(key: string): R | null | undefined {
    if (this.queued.has(key)) return this.queued.get(key);
    if (this.writing.has(key)) return this.writing.get(key);
    return undefined;
  }

  /** Resolves once everything queued before this call has been written. */
  flush(): Promise<void> {
    if (!this.draining) {
      if (this.queued.size === 0) return Promise.resolve();
      this.draining = this.drain().finally(() => {
        this.draining = null;
      });
    }
    return this.draining;
  }

  private async drain(): Promise<void> {
    // Let callers in the same tick join the first round.
    await Promise.resolve();
    let failure: unknown;
    while (this.queued.size > 0) {
      const blobs = this.blobs;
      this.blobs = [];
      this.writing = this.queued;
      this.queued = new Map();
      try {
        await this.writeRound(blobs, this.writing);
      } catch (error) {
        failure ??= error;
      } finally {
        this.writing = new Map();
      }
    }
    if (failure) throw failure;
  }

  private async writeRound(blobs: StoreEntry[], records: Map<string, R | null>): Promise<void> {
    const store = this.store;
    if (!store) return;
    const puts = [...blobs];
    const removals: Hash[] = [];
    for (const [key, record] of records) {
      const hash = await this.storageKey(key);
      if (record === null) removals.push(hash);
      else puts.push({ hash, data: this.encode(record) });
    }
    if (puts.length > 0) {
      if (typeof store.putMany === 'function') {
        await store.putMany(puts);
      } else {
        for (const entry of puts) await store.put(entry.hash, entry.data);
      }
    }
    for (const hash of removals) await store.delete(hash);
  }
}
