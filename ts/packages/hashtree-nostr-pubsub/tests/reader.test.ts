import { describe, expect, it } from 'vitest';
import {
  MemoryStore,
  type CID,
  type Hash,
  type Store,
} from '@hashtree/core';
import { NostrEventStore, type StoredNostrEvent } from '@hashtree/nostr';
import { finalizeEvent, getPublicKey, verifyEvent } from 'nostr-tools/pure';
import {
  HashtreeNostrEventReader,
  HashtreeNostrFilterError,
  HashtreeNostrUnsupportedSearchError,
  type HashtreeNostrRootProvider,
} from '../src/index.js';

const aliceKey = new Uint8Array(32).fill(1);
const bobKey = new Uint8Array(32).fill(2);
const alice = getPublicKey(aliceKey);
const bob = getPublicKey(bobKey);

function event(
  key: Uint8Array,
  createdAt: number,
  options: { kind?: number; tags?: string[][]; content?: string } = {},
): StoredNostrEvent {
  const signed = finalizeEvent({
    created_at: createdAt,
    kind: options.kind ?? 1,
    tags: options.tags ?? [],
    content: options.content ?? `event-${createdAt}`,
  }, key);
  return {
    id: signed.id,
    pubkey: signed.pubkey,
    created_at: signed.created_at,
    kind: signed.kind,
    tags: signed.tags,
    content: signed.content,
    sig: signed.sig,
  };
}

async function buildIndex(
  events: StoredNostrEvent[],
  store = new MemoryStore(),
): Promise<{ store: MemoryStore; root: CID }> {
  const root = await new NostrEventStore(store).build(null, events);
  if (!root) throw new Error('test index unexpectedly has no root');
  return { store, root };
}

describe('HashtreeNostrEventReader', () => {
  it('applies Nostr OR filters, authors, tags, kinds, and inclusive time bounds', async () => {
    const byAuthor = event(aliceKey, 10, { tags: [['t', 'alpha']] });
    const target = 'f'.repeat(64);
    const byTag = event(bobKey, 20, { kind: 7, tags: [['e', target]] });
    const excluded = event(bobKey, 30, { tags: [['t', 'other']] });
    const { store, root } = await buildIndex([byAuthor, byTag, excluded]);
    const reader = new HashtreeNostrEventReader({ store, roots: root });

    const report = await reader.query([
      { authors: [alice], kinds: [1], since: 10, until: 10 },
      { '#e': [target], kinds: [7] },
    ]);

    expect(report.events.map(({ event: candidate }) => candidate.id)).toEqual([
      byTag.id,
      byAuthor.id,
    ]);
    expect(report.complete).toBe(true);
  });

  it('treats filter limits as per-filter and QueryOptions.limit as global', async () => {
    const aliceNewest = event(aliceKey, 40);
    const aliceOlder = event(aliceKey, 30);
    const bobNewest = event(bobKey, 35, { kind: 7 });
    const bobOlder = event(bobKey, 20, { kind: 7 });
    const { store, root } = await buildIndex([aliceNewest, aliceOlder, bobNewest, bobOlder]);
    const reader = new HashtreeNostrEventReader({ store, roots: root });

    const perFilter = await reader.query([
      { authors: [alice], limit: 1 },
      { authors: [bob], limit: 1 },
    ]);
    const globallyLimited = await reader.query([
      { authors: [alice], limit: 2 },
      { authors: [bob], limit: 2 },
    ], { limit: 2 });

    expect(perFilter.events.map(({ event: candidate }) => candidate.id)).toEqual([
      aliceNewest.id,
      bobNewest.id,
    ]);
    expect(globallyLimited.events.map(({ event: candidate }) => candidate.id)).toEqual([
      aliceNewest.id,
      bobNewest.id,
    ]);
  });

  it('merges additive partitions concurrently, deduplicates roots, orders, and globally limits', async () => {
    const first = await buildIndex([event(aliceKey, 30), event(aliceKey, 10)]);
    const second = await buildIndex([event(bobKey, 40), event(bobKey, 20)]);
    const gate = new ParallelReadGate(2);
    const reader = new HashtreeNostrEventReader({
      store: first.store,
      roots: [
        { partitionId: 'first', root: first.root, store: new GatedStore(first.store, gate) },
        { partitionId: 'duplicate', root: first.root, store: first.store },
        { partitionId: 'second', root: second.root, store: new GatedStore(second.store, gate) },
      ],
    });

    const report = await reader.query([{}], { limit: 3, deadline: Date.now() + 1_000 });

    expect(gate.started).toBe(2);
    expect(report.events.map(({ event: candidate }) => candidate.created_at)).toEqual([40, 30, 20]);
    expect(new Set(report.events.map(({ event: candidate }) => candidate.id)).size).toBe(3);
    expect(report.partitions).toHaveLength(3);
  });

  it('bounds partition concurrency while incrementally retaining the global top-k', async () => {
    const tracker = new ConcurrentReadTracker();
    const roots = await Promise.all(Array.from({ length: 7 }, async (_, index) => {
      const built = await buildIndex([event(index % 2 === 0 ? aliceKey : bobKey, 10 + index)]);
      return {
        partitionId: `partition-${index}`,
        root: built.root,
        store: new TrackedFirstReadStore(built.store, tracker),
      };
    }));
    const reader = new HashtreeNostrEventReader({
      store: new MemoryStore(),
      roots,
      maxConcurrentPartitions: 2,
    });

    const report = await reader.query([{}], { limit: 3 });

    expect(tracker.peak).toBe(2);
    expect(tracker.started).toBe(7);
    expect(report.events.map(({ event: candidate }) => candidate.created_at)).toEqual([16, 15, 14]);
    expect(report.partitions.map(({ partitionId }) => partitionId)).toEqual(
      roots.map(({ partitionId }) => partitionId),
    );
  });

  it('fails over an unavailable replica and a corrupt replica in order', async () => {
    const valid = event(aliceKey, 10);
    const good = await buildIndex([valid]);
    const invalidSignature = { ...event(aliceKey, 11), sig: '0'.repeat(128) };
    const corrupt = await buildIndex([invalidSignature]);
    const missingStore = new MemoryStore();
    const reader = new HashtreeNostrEventReader({
      store: good.store,
      roots: [
        { partitionId: 'archive', replicaId: 'missing', root: good.root, store: missingStore },
        { partitionId: 'archive', replicaId: 'corrupt', root: corrupt.root, store: corrupt.store },
        { partitionId: 'archive', replicaId: 'good', root: good.root, store: good.store },
      ],
    });

    const report = await reader.query([{}]);

    expect(report.events.map(({ event: candidate }) => candidate.id)).toEqual([valid.id]);
    expect(report.partitions[0]).toMatchObject({
      status: 'complete',
      selectedReplicaId: 'good',
      attempts: [
        { replicaId: 'missing', status: 'unavailable' },
        { replicaId: 'corrupt', status: 'corrupt' },
        { replicaId: 'good', status: 'complete' },
      ],
    });
  });

  it('distinguishes a proven empty null root from an unavailable partition', async () => {
    const valid = await buildIndex([event(aliceKey, 10)]);
    const empty = new HashtreeNostrEventReader({ store: valid.store, roots: null });
    const unavailable = new HashtreeNostrEventReader({
      store: new MemoryStore(),
      roots: valid.root,
    });

    const emptyReport = await empty.query([{}]);
    const unavailableReport = await unavailable.query([{}], { deadline: Date.now() + 500 });

    expect(emptyReport).toMatchObject({
      events: [],
      complete: true,
      partitions: [{ status: 'empty', attempts: [{ status: 'empty' }] }],
    });
    expect(unavailableReport).toMatchObject({
      events: [],
      complete: false,
      partitions: [{ status: 'unavailable', attempts: [{ status: 'unavailable' }] }],
    });
  });

  it('fails over when a selected event block is missing instead of reporting empty', async () => {
    const stored = event(aliceKey, 10);
    const damaged = await buildIndex([stored]);
    const source = await new NostrEventStore(damaged.store).getCollectionSource(damaged.root);
    const eventCid = await source.get(stored.id);
    if (!eventCid) throw new Error('test event CID missing');
    await damaged.store.delete(eventCid.hash);
    const good = await buildIndex([stored]);
    const reader = new HashtreeNostrEventReader({
      store: damaged.store,
      roots: [
        { partitionId: 'archive', replicaId: 'damaged', root: damaged.root, store: damaged.store },
        { partitionId: 'archive', replicaId: 'good', root: good.root, store: good.store },
      ],
    });

    const report = await reader.query([{ ids: [stored.id] }]);

    expect(report.events.map(({ event: candidate }) => candidate.id)).toEqual([stored.id]);
    expect(report.partitions[0]?.attempts.map(({ status }) => status)).toEqual([
      'unavailable',
      'complete',
    ]);
  });

  it('reports an invalid signature as corrupt when no replica can replace it', async () => {
    const invalid = { ...event(aliceKey, 10), sig: '0'.repeat(128) };
    const index = await buildIndex([invalid]);
    const reader = new HashtreeNostrEventReader({ store: index.store, roots: index.root });

    const report = await reader.query([{}]);

    expect(report).toMatchObject({
      events: [],
      complete: false,
      partitions: [{ status: 'unavailable', attempts: [{ status: 'corrupt' }] }],
    });
  });

  it('verifies each replica in one defensive batch and freezes accepted events', async () => {
    const first = event(aliceKey, 10, { tags: [['t', 'first']] });
    const second = event(bobKey, 20, { tags: [['t', 'second']] });
    const index = await buildIndex([first, second]);
    const controller = new AbortController();
    let calls = 0;
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: index.root,
      verifyEvents: async (events, context) => {
        calls += 1;
        expect(context.signal).toBe(controller.signal);
        expect(Object.isFrozen(events)).toBe(true);
        expect(events).toHaveLength(2);
        events[0]!.content = 'tampered';
        events[0]!.tags[0]![1] = 'tampered';
        return events.map(() => true);
      },
    });

    const report = await reader.query([
      { authors: [alice] },
      { authors: [bob] },
    ], { signal: controller.signal });

    expect(calls).toBe(1);
    expect(report.events.map(({ event: candidate }) => candidate.id)).toEqual([
      second.id,
      first.id,
    ]);
    expect(report.events.every(({ event: candidate }) => (
      Object.isFrozen(candidate)
      && Object.isFrozen(candidate.tags)
      && candidate.tags.every(Object.isFrozen)
    ))).toBe(true);
    expect(report.events.some(({ event: candidate }) => (
      candidate.content === 'tampered'
      || candidate.tags.some((tag) => tag.includes('tampered'))
    ))).toBe(false);
    expect(verifyEvent(report.events[0]!.event)).toBe(true);
  });

  it('treats malformed verifier result arrays as corrupt replica responses', async () => {
    const stored = [event(aliceKey, 10), event(bobKey, 20)];
    const index = await buildIndex(stored);
    let calls = 0;
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: [
        { partitionId: 'archive', replicaId: 'non-array', root: index.root },
        { partitionId: 'archive', replicaId: 'wrong-length', root: index.root },
        { partitionId: 'archive', replicaId: 'sparse', root: index.root },
        { partitionId: 'archive', replicaId: 'non-boolean', root: index.root },
        { partitionId: 'archive', replicaId: 'good', root: index.root },
      ],
      verifyEvents: async (events) => {
        calls += 1;
        if (calls === 1) return new Uint8Array([1, 1]) as unknown as boolean[];
        if (calls === 2) return [true];
        if (calls === 3) {
          const sparse = new Array<boolean>(events.length);
          sparse[0] = true;
          return sparse;
        }
        if (calls === 4) return [true, 1] as unknown as boolean[];
        return events.map(() => true);
      },
    });

    const report = await reader.query([{}]);

    expect(calls).toBe(5);
    expect(report.events).toHaveLength(2);
    expect(report.partitions[0]).toMatchObject({
      status: 'complete',
      selectedReplicaId: 'good',
      attempts: [
        { replicaId: 'non-array', status: 'corrupt' },
        { replicaId: 'wrong-length', status: 'corrupt' },
        { replicaId: 'sparse', status: 'corrupt' },
        { replicaId: 'non-boolean', status: 'corrupt' },
        { replicaId: 'good', status: 'complete' },
      ],
    });
  });

  it('rejects an entire replica batch when any event fails verification', async () => {
    const stored = [event(aliceKey, 10), event(bobKey, 20)];
    const index = await buildIndex(stored);
    let calls = 0;
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: [
        { partitionId: 'archive', replicaId: 'invalid', root: index.root },
        { partitionId: 'archive', replicaId: 'good', root: index.root },
      ],
      verifyEvents: async (events) => {
        calls += 1;
        return calls === 1
          ? events.map((_, index) => index !== 0)
          : events.map(() => true);
      },
    });

    const report = await reader.query([{}]);

    expect(report.events).toHaveLength(2);
    expect(report.partitions[0]?.attempts.map(({ status }) => status)).toEqual([
      'corrupt',
      'complete',
    ]);
  });

  it('rejects malformed stored events before invoking an injected verifier', async () => {
    const malformed = event(aliceKey, 10, { kind: 65_536 });
    const index = await buildIndex([malformed]);
    let calls = 0;
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: index.root,
      verifyEvents: async (events) => {
        calls += 1;
        return events.map(() => true);
      },
    });

    const report = await reader.query([{}]);

    expect(calls).toBe(0);
    expect(report).toMatchObject({
      events: [],
      complete: false,
      partitions: [{ status: 'unavailable', attempts: [{ status: 'corrupt' }] }],
    });
  });

  it('cancels an in-flight injected batch verifier through AbortSignal', async () => {
    const index = await buildIndex([event(aliceKey, 10)]);
    const controller = new AbortController();
    let markStarted!: () => void;
    let markStopped!: () => void;
    const started = new Promise<void>((resolve) => {
      markStarted = resolve;
    });
    const stopped = new Promise<void>((resolve) => {
      markStopped = resolve;
    });
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: index.root,
      verifyEvents: async (_events, context) => {
        expect(context.signal).toBe(controller.signal);
        markStarted();
        return await new Promise<boolean[]>((_resolve, reject) => {
          context.signal!.addEventListener('abort', () => {
            markStopped();
            reject(context.signal!.reason);
          }, { once: true });
        });
      },
    });
    const query = reader.query([{}], { signal: controller.signal });
    await started;

    controller.abort();

    await expect(query).rejects.toMatchObject({ name: 'AbortError' });
    await stopped;
  });

  it('aborts an in-flight injected batch verifier at the query deadline', async () => {
    const index = await buildIndex([event(aliceKey, 10)]);
    const deadline = Date.now() + 100;
    let markStarted!: () => void;
    let markStopped!: () => void;
    const started = new Promise<void>((resolve) => {
      markStarted = resolve;
    });
    const stopped = new Promise<void>((resolve) => {
      markStopped = resolve;
    });
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: index.root,
      verifyEvents: async (_events, context) => {
        expect(context.deadline).toBe(deadline);
        expect(context.signal).toBeInstanceOf(AbortSignal);
        markStarted();
        return await new Promise<boolean[]>((_resolve, reject) => {
          context.signal!.addEventListener('abort', () => {
            markStopped();
            reject(context.signal!.reason);
          }, { once: true });
        });
      },
    });
    const query = reader.query([{}], { deadline });
    await started;

    await expect(query).rejects.toMatchObject({ name: 'TimeoutError' });
    await stopped;
  });

  it('does not invoke the verifier after the query deadline has elapsed', async () => {
    const index = await buildIndex([event(aliceKey, 10)]);
    let calls = 0;
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: index.root,
      verifyEvents: async (events) => {
        calls += 1;
        return events.map(() => true);
      },
    });

    await expect(reader.query([{}], { deadline: Date.now() - 1 })).rejects.toMatchObject({
      name: 'TimeoutError',
    });
    expect(calls).toBe(0);
  });

  it('does not misreport verifier infrastructure failures as corrupt replicas', async () => {
    const index = await buildIndex([event(aliceKey, 10)]);
    let calls = 0;
    const reader = new HashtreeNostrEventReader({
      store: index.store,
      roots: [
        { partitionId: 'archive', replicaId: 'first', root: index.root },
        { partitionId: 'archive', replicaId: 'second', root: index.root },
      ],
      verifyEvents: async () => {
        calls += 1;
        throw new Error('worker unavailable');
      },
    });

    await expect(reader.query([{}])).rejects.toMatchObject({
      name: 'HashtreeNostrVerifierUnavailableError',
      message: 'Hashtree Nostr event verifier is unavailable',
      cause: { message: 'worker unavailable' },
    });
    expect(calls).toBe(1);
  });

  it('rejects NIP-50 search before consulting the root provider', async () => {
    let snapshots = 0;
    const provider: HashtreeNostrRootProvider = {
      snapshot: () => {
        snapshots += 1;
        return [];
      },
    };
    const reader = new HashtreeNostrEventReader({ store: new MemoryStore(), roots: provider });

    await expect(reader.query([{ search: 'hashtree' }])).rejects.toBeInstanceOf(
      HashtreeNostrUnsupportedSearchError,
    );
    expect(snapshots).toBe(0);
  });

  it('rejects unsupported partial hexadecimal and empty list filters explicitly', async () => {
    const reader = new HashtreeNostrEventReader({ store: new MemoryStore(), roots: null });

    await expect(reader.query([{ authors: [alice.slice(0, 16)] }])).rejects.toBeInstanceOf(
      HashtreeNostrFilterError,
    );
    await expect(reader.query([{ ids: [] }])).rejects.toBeInstanceOf(
      HashtreeNostrFilterError,
    );
  });

  it('snapshots a provider exactly once per query with filters and cancellation context', async () => {
    const indexed = await buildIndex([event(aliceKey, 10)]);
    const controller = new AbortController();
    const deadline = Date.now() + 1_000;
    let snapshots = 0;
    const provider: HashtreeNostrRootProvider = {
      snapshot: (context) => {
        snapshots += 1;
        expect(context.filters).toEqual([{ authors: [alice] }]);
        expect(context.signal).toBe(controller.signal);
        expect(context.deadline).toBe(deadline);
        return [{ partitionId: 'selected', root: indexed.root }];
      },
    };
    const reader = new HashtreeNostrEventReader({ store: indexed.store, roots: provider });

    const report = await reader.query([{ authors: [alice] }], {
      signal: controller.signal,
      deadline,
    });

    expect(snapshots).toBe(1);
    expect(report.events).toHaveLength(1);
  });

  it('cancels an in-flight block read through AbortSignal', async () => {
    const indexed = await buildIndex([event(aliceKey, 10)]);
    const controller = new AbortController();
    const reader = new HashtreeNostrEventReader({
      store: new DelayedStore(indexed.store, 1_000),
      roots: indexed.root,
    });
    const query = reader.query([{}], { signal: controller.signal });
    setTimeout(() => controller.abort(), 10);

    await expect(query).rejects.toMatchObject({ name: 'AbortError' });
  });

  it('bounds an in-flight block read with an absolute deadline', async () => {
    const indexed = await buildIndex([event(aliceKey, 10)]);
    const reader = new HashtreeNostrEventReader({
      store: new DelayedStore(indexed.store, 1_000),
      roots: indexed.root,
    });

    await expect(reader.query([{}], { deadline: Date.now() + 10 })).rejects.toMatchObject({
      name: 'TimeoutError',
    });
  });
});

class DelayedStore implements Store {
  constructor(private readonly store: Store, private readonly delayMs: number) {}

  async get(hash: Hash): Promise<Uint8Array | null> {
    await new Promise((resolve) => setTimeout(resolve, this.delayMs));
    return await this.store.get(hash);
  }

  async has(hash: Hash): Promise<boolean> {
    return await this.store.has(hash);
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    return await this.store.put(hash, data);
  }

  async delete(hash: Hash): Promise<boolean> {
    return await this.store.delete(hash);
  }
}

class ParallelReadGate {
  started = 0;
  private release!: () => void;
  private readonly ready = new Promise<void>((resolve) => {
    this.release = resolve;
  });

  constructor(private readonly expected: number) {}

  async arrive(): Promise<void> {
    this.started += 1;
    if (this.started >= this.expected) this.release();
    await this.ready;
  }
}

class GatedStore implements Store {
  private firstRead = true;

  constructor(private readonly store: Store, private readonly gate: ParallelReadGate) {}

  async get(hash: Hash): Promise<Uint8Array | null> {
    if (this.firstRead) {
      this.firstRead = false;
      await this.gate.arrive();
    }
    return await this.store.get(hash);
  }

  async has(hash: Hash): Promise<boolean> {
    return await this.store.has(hash);
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    return await this.store.put(hash, data);
  }

  async delete(hash: Hash): Promise<boolean> {
    return await this.store.delete(hash);
  }
}

class ConcurrentReadTracker {
  started = 0;
  active = 0;
  peak = 0;

  async track(): Promise<void> {
    this.started += 1;
    this.active += 1;
    this.peak = Math.max(this.peak, this.active);
    await new Promise((resolve) => setTimeout(resolve, 5));
    this.active -= 1;
  }
}

class TrackedFirstReadStore implements Store {
  private firstRead = true;

  constructor(private readonly store: Store, private readonly tracker: ConcurrentReadTracker) {}

  async get(hash: Hash): Promise<Uint8Array | null> {
    if (this.firstRead) {
      this.firstRead = false;
      await this.tracker.track();
    }
    return await this.store.get(hash);
  }

  async has(hash: Hash): Promise<boolean> {
    return await this.store.has(hash);
  }

  async put(hash: Hash, data: Uint8Array): Promise<boolean> {
    return await this.store.put(hash, data);
  }

  async delete(hash: Hash): Promise<boolean> {
    return await this.store.delete(hash);
  }
}
