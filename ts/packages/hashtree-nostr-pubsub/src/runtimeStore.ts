import type { CID, Store } from '@hashtree/core';
import { NostrEventStore } from '@hashtree/nostr';
import type {
  NostrEvent, NostrFilter, QueryOptions, RuntimeEventStore, RuntimeOutboxEntry,
} from 'nostr-pubsub';
import { HashtreeNostrEventReader } from './reader.js';

export interface HashtreeRuntimeState {
  root: CID | null;
  outboxRoot: CID | null;
  pending: Array<Omit<RuntimeOutboxEntry, 'event'> & { id: string }>;
}

/** Persist the small root pointer atomically; events remain in Hashtree's indexes. */
export interface HashtreeRuntimeStateStorage {
  load(): Promise<HashtreeRuntimeState | null>;
  save(state: HashtreeRuntimeState): Promise<void>;
  /** Coordinate writers sharing a database, for example with the browser Web Locks API. */
  withLock?<T>(operation: () => Promise<T>): Promise<T>;
  close?(): void | Promise<void>;
}

export class HashtreeRuntimeEventStore implements RuntimeEventStore {
  private readonly events: NostrEventStore;
  private pendingWrite: Promise<unknown> = Promise.resolve();

  constructor(private readonly store: Store, private readonly state: HashtreeRuntimeStateStorage) {
    this.events = new NostrEventStore(store);
  }

  async query(filters: NostrFilter[], options: QueryOptions = {}): Promise<NostrEvent[]> {
    return this.read(async (state) => {
      const report = await new HashtreeNostrEventReader({ store: this.store, roots: state.root })
        .query(filters, options);
      if (!report.complete) {
        const reasons = report.partitions.flatMap((partition) => partition.attempts.map((attempt) => attempt.reason).filter(Boolean));
        throw new Error(`Hashtree event index is unavailable: ${reasons.join('; ')}`);
      }
      return report.events.map(({ event }) => event);
    });
  }

  put(event: NostrEvent): Promise<void> {
    return this.update(async (state) => { state.root = await this.events.add(state.root, event); });
  }

  delete(ids: string[]): Promise<void> {
    return this.update(async (state) => {
      for (const id of ids) state.root = await this.events.delete(state.root, id);
    });
  }

  async listPending(): Promise<RuntimeOutboxEntry[]> {
    return this.read(async (state) => {
      const entries: RuntimeOutboxEntry[] = [];
      for (const { id, ...scope } of state.pending) {
        const event = await this.events.getById(state.outboxRoot, id);
        // Replaceable events supersede older pending versions in the same index.
        if (event) entries.push({ event, ...scope });
      }
      return entries;
    });
  }

  putPending(entry: RuntimeOutboxEntry): Promise<void> {
    return this.update(state => this.writePending(state, entry));
  }

  updatePending(id: string, update: (row: RuntimeOutboxEntry | undefined) => RuntimeOutboxEntry | undefined): Promise<void> {
    return this.update(async state => {
      const metadata = state.pending.find(entry => entry.id === id);
      let previous: RuntimeOutboxEntry | undefined;
      if (metadata) {
        const { id: eventId, ...scope } = metadata;
        const event = await this.events.getById(state.outboxRoot, eventId);
        if (event) previous = { event, ...scope };
      }
      const next = update(previous);
      if (next) {
        if (next.event.id !== id) throw new Error('Pending update changed the event ID');
        await this.writePending(state, next);
      } else if (metadata) {
        await this.removePending(state, id);
      }
    });
  }

  deletePending(id: string): Promise<void> {
    return this.update(state => this.removePending(state, id));
  }

  private async removePending(state: HashtreeRuntimeState, id: string): Promise<void> {
    state.outboxRoot = await this.events.delete(state.outboxRoot, id);
    state.pending = state.pending.filter(entry => entry.id !== id);
  }

  private async writePending(state: HashtreeRuntimeState, { event, ...scope }: RuntimeOutboxEntry): Promise<void> {
    if (state.pending.length >= 1_000 && !state.pending.some(({ id }) => id === event.id)) {
      throw new Error('Outbox capacity exhausted');
    }
    state.outboxRoot = await this.events.add(state.outboxRoot, event);
    state.pending = state.pending.filter(({ id }) => id !== event.id);
    state.pending.push({ id: event.id, ...scope });
    const present = await Promise.all(state.pending.map(async (item) => ({
      item, exists: !!await this.events.getById(state.outboxRoot, item.id),
    })));
    state.pending = present.filter(({ exists }) => exists).map(({ item }) => item);
  }

  async close(): Promise<void> { await this.pendingWrite; await this.state.close?.(); }

  private async read<T>(read: (state: HashtreeRuntimeState) => Promise<T>): Promise<T> {
    await this.pendingWrite;
    const operation = async () => read(await this.load());
    return this.state.withLock ? this.state.withLock(operation) : operation();
  }

  private async load(): Promise<HashtreeRuntimeState> {
    return await this.state.load() ?? { root: null, outboxRoot: null, pending: [] };
  }

  private update(change: (state: HashtreeRuntimeState) => Promise<void>): Promise<void> {
    const operation = async () => {
      const state = await this.load();
      await change(state);
      await this.state.save(state);
    };
    const result = this.pendingWrite.then(() => this.state.withLock
      ? this.state.withLock(operation) : operation());
    this.pendingWrite = result.catch(() => undefined);
    return result;
  }
}
