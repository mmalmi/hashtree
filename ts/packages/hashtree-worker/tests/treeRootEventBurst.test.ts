import { MemoryStore, nhashDecode, toHex, type Hash, type Store } from '@hashtree/core';
import { HASHTREE_ROOT_KIND, readTreeEventSnapshot } from '@hashtree/nostr';
import { nip19 } from 'nostr-tools';
import { beforeEach, describe, expect, it } from 'vitest';
import type { SignedEvent } from '../src/relay/protocol';
import {
  clearMemoryCache,
  getCachedRootInfo,
  initTreeRootCache,
  removeCachedRoot,
  setCachedRoot,
} from '../src/relay/treeRootCache';
import { handleTreeRootEvent, setNotifyCallback, type TreeRootRecord } from '../src/relay/treeRootSubscription';

/** Upserts like the worker's IndexedDB store, counting writes and optionally holding them. */
class CountingStore implements Store {
  readonly inner = new MemoryStore();
  writeCalls = 0;
  entriesWritten = 0;
  private gate: Promise<void> = Promise.resolve();
  private openGate: () => void = () => {};

  blockWrites(): void {
    this.gate = new Promise((resolve) => { this.openGate = resolve; });
  }

  releaseWrites(): void {
    this.openGate();
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    this.writeCalls++;
    this.entriesWritten++;
    await this.gate;
    return this.upsert(hash, data);
  }

  private async upsert(hash: Hash, data: Uint8Array): Promise<boolean> {
    await this.inner.delete(hash);
    return this.inner.put(hash, data);
  }

  async putMany(entries: ReadonlyArray<{ hash: Hash; data: Uint8Array }>): Promise<boolean> {
    this.writeCalls++;
    this.entriesWritten += entries.length;
    await this.gate;
    for (const entry of entries) await this.upsert(entry.hash, entry.data);
    return true;
  }

  get(hash: Hash): Promise<Uint8Array | null> {
    return this.inner.get(hash);
  }

  has(hash: Hash): Promise<boolean> {
    return this.inner.has(hash);
  }

  delete(hash: Hash): Promise<boolean> {
    return this.inner.delete(hash);
  }
}

const pubkey = 'f'.repeat(64);
const npub = nip19.npubEncode(pubkey);
let eventSequence = 0;

function rootEvent(treeName: string, hashByte: number, createdAt: number, id?: string): SignedEvent {
  eventSequence++;
  return {
    id: id ?? eventSequence.toString(16).padStart(64, '0'),
    pubkey,
    kind: HASHTREE_ROOT_KIND,
    content: '',
    tags: [['d', treeName], ['l', 'hashtree'], ['hash', hashByte.toString(16).padStart(2, '0').repeat(32)]],
    created_at: createdAt,
    sig: 'a'.repeat(128),
  };
}

async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function waitForNotifications(count: number): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (notifications.length < count) {
    if (Date.now() > deadline) throw new Error(`Expected ${count} notifications, got ${notifications.length}`);
    await settle();
  }
}

let store: CountingStore;
let notifications: Array<{ treeName: string; record: TreeRootRecord }>;

beforeEach(() => {
  clearMemoryCache();
  store = new CountingStore();
  initTreeRootCache(store);
  notifications = [];
  setNotifyCallback((_npub, treeName, record) => { notifications.push({ treeName, record }); });
});

describe('tree root event bursts', () => {
  it('applies a burst with one grouped write per round instead of writes per event', async () => {
    const trees = 150;
    const burst: SignedEvent[] = [];
    for (let tree = 0; tree < trees; tree++) {
      const older = rootEvent(`tree-${tree}`, 1, 1_000);
      const newest = rootEvent(`tree-${tree}`, 2, 2_000);
      // The same events arrive from several relays and subscriptions, in mixed order.
      burst.push(newest, older, newest, older, newest);
    }

    await Promise.all(burst.map((event) => handleTreeRootEvent(event)));

    expect(notifications).toHaveLength(trees);
    for (const { record } of notifications) {
      expect(record.updatedAt).toBe(2_000);
      expect(record.snapshotNhash).toMatch(/^nhash1/);
    }
    // One snapshot and one root record per tree, never one per delivered event.
    expect(store.entriesWritten).toBe(2 * trees);
    expect(store.writeCalls).toBeLessThanOrEqual(4);
    for (let tree = 0; tree < trees; tree++) {
      expect((await getCachedRootInfo(npub, `tree-${tree}`))?.updatedAt).toBe(2_000);
    }
  });

  it('does no store work for an event older than the cached root', async () => {
    await handleTreeRootEvent(rootEvent('docs', 2, 2_000));
    const writesBefore = store.entriesWritten;
    notifications.length = 0;

    await handleTreeRootEvent(rootEvent('docs', 1, 1_000));

    expect(store.entriesWritten).toBe(writesBefore);
    expect(notifications).toEqual([]);
    expect(toHex((await getCachedRootInfo(npub, 'docs'))!.hash)).toBe('02'.repeat(32));
  });

  it('does no store work for a repeated delivery of the cached event', async () => {
    const event = rootEvent('docs', 2, 2_000);
    await handleTreeRootEvent(event);
    const writesBefore = store.entriesWritten;
    notifications.length = 0;

    await Promise.all([handleTreeRootEvent({ ...event }), handleTreeRootEvent({ ...event })]);

    expect(store.entriesWritten).toBe(writesBefore);
    expect(notifications).toEqual([]);
  });

  it('skips older events against a root that is only in the persistent store', async () => {
    await handleTreeRootEvent(rootEvent('docs', 2, 2_000));
    clearMemoryCache();
    const writesBefore = store.entriesWritten;
    notifications.length = 0;

    await handleTreeRootEvent(rootEvent('docs', 1, 1_000));

    expect(store.entriesWritten).toBe(writesBefore);
    expect(notifications).toEqual([]);
    expect((await getCachedRootInfo(npub, 'docs'))?.updatedAt).toBe(2_000);
  });

  it('updates the cache and notifies before persistence completes', async () => {
    store.blockWrites();
    const event = rootEvent('docs', 7, 3_000);

    const applied = handleTreeRootEvent(event);
    await waitForNotifications(1);

    expect(notifications).toHaveLength(1);
    expect(notifications[0].record.updatedAt).toBe(3_000);
    expect(notifications[0].record.snapshotNhash).toMatch(/^nhash1/);
    expect(toHex((await getCachedRootInfo(npub, 'docs'))!.hash)).toBe('07'.repeat(32));
    expect(store.inner.size).toBe(0);

    store.releaseWrites();
    await applied;

    clearMemoryCache();
    const persisted = await getCachedRootInfo(npub, 'docs');
    expect(persisted?.updatedAt).toBe(3_000);
    expect(persisted?.snapshotNhash).toBe(notifications[0].record.snapshotNhash);
    const snapshot = await readTreeEventSnapshot(store.inner, nip19, nhashDecode(persisted!.snapshotNhash!));
    expect(snapshot?.event.id).toBe(event.id);
  });

  it('keeps the newest root when events arrive out of order while a write is pending', async () => {
    store.blockWrites();
    const first = handleTreeRootEvent(rootEvent('docs', 2, 2_000));
    await waitForNotifications(1);
    const older = handleTreeRootEvent(rootEvent('docs', 1, 1_000));
    const newest = handleTreeRootEvent(rootEvent('docs', 3, 3_000));
    const between = handleTreeRootEvent(rootEvent('docs', 4, 2_500));
    await settle();

    store.releaseWrites();
    await Promise.all([first, older, newest, between]);

    expect(notifications.map(({ record }) => record.updatedAt)).toEqual([2_000, 3_000]);
    clearMemoryCache();
    const persisted = await getCachedRootInfo(npub, 'docs');
    expect(persisted?.updatedAt).toBe(3_000);
    expect(toHex(persisted!.hash)).toBe('03'.repeat(32));
  });

  it('keeps the lower event id for same-second roots within one burst', async () => {
    const high = rootEvent('docs', 1, 2_000, 'b'.repeat(64));
    const low = rootEvent('docs', 2, 2_000, 'a'.repeat(64));

    await Promise.all([handleTreeRootEvent(high), handleTreeRootEvent(low), handleTreeRootEvent(high)]);

    expect(toHex((await getCachedRootInfo(npub, 'docs'))!.hash)).toBe('02'.repeat(32));
    expect(notifications.at(-1)?.record.updatedAt).toBe(2_000);
    expect(notifications.length).toBeLessThanOrEqual(1);
  });

  it('persists the newest of concurrent direct cache writes', async () => {
    const hash = (byte: number) => new Uint8Array(32).fill(byte);
    await Promise.all([
      setCachedRoot(npub, 'docs', { hash: hash(1) }, 'public', { updatedAt: 100, eventId: 'a' }),
      setCachedRoot(npub, 'docs', { hash: hash(3) }, 'public', { updatedAt: 300, eventId: 'c' }),
      setCachedRoot(npub, 'docs', { hash: hash(2) }, 'public', { updatedAt: 200, eventId: 'b' }),
    ]);

    clearMemoryCache();
    expect((await getCachedRootInfo(npub, 'docs'))?.updatedAt).toBe(300);
  });

  it('does not resurrect a removed root from a pending write', async () => {
    store.blockWrites();
    const applied = handleTreeRootEvent(rootEvent('docs', 2, 2_000));
    await waitForNotifications(1);
    const removed = removeCachedRoot(npub, 'docs');
    store.releaseWrites();
    await Promise.all([applied, removed]);

    clearMemoryCache();
    expect(await getCachedRootInfo(npub, 'docs')).toBeNull();
  });
});
