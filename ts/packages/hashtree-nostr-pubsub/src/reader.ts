import { NostrEventStore, type StoredNostrEvent } from '@hashtree/nostr';
import {
  filtersMatchNostrEvents,
  verifyAndFreezeNostrEvent,
  type NostrEventReaderContract,
  type NostrFilter,
  type NostrReaderQueryEvent,
  type NostrVerifiedEvent,
} from './nostrTypes.js';
import { QueryCancellation, isCancellationError } from './cancellation.js';
import {
  normalizeGlobalLimit,
  planFilterQuery,
  validateHashtreeNostrFilters,
} from './filterQuery.js';
import { GuardedReplicaStore } from './guardedStore.js';
import {
  normalizeRootProvider,
  snapshotRoots,
  type RootReplicaSnapshot,
} from './roots.js';
import {
  HashtreeNostrReplicaCorruptError,
  HashtreeNostrReplicaUnavailableError,
  type HashtreeNostrEventReaderOptions,
  type HashtreeNostrPartitionReport,
  type HashtreeNostrQueryOptions,
  type HashtreeNostrQueryReport,
  type HashtreeNostrReplicaAttempt,
} from './types.js';

interface PartitionResult {
  events: NostrVerifiedEvent[];
  report: HashtreeNostrPartitionReport;
}

const DEFAULT_LOCAL_INDEX_PRIORITY = 300;

export class HashtreeNostrEventReader implements NostrEventReaderContract {
  private readonly store;
  private readonly rootProvider;
  private readonly source;
  private readonly priority;

  constructor(options: HashtreeNostrEventReaderOptions) {
    this.store = options.store;
    this.rootProvider = normalizeRootProvider(options.roots);
    this.source = options.source ?? {
      id: options.sourceId ?? 'hashtree-nostr',
      kind: 'local-index' as const,
    };
    this.priority = options.priority ?? DEFAULT_LOCAL_INDEX_PRIORITY;
  }

  async query(
    filters: NostrFilter[],
    options: HashtreeNostrQueryOptions = {},
  ): Promise<HashtreeNostrQueryReport> {
    validateHashtreeNostrFilters(filters);
    const limit = normalizeGlobalLimit(options.limit);
    const cancellation = new QueryCancellation(options);
    const roots = await snapshotRoots(
      this.rootProvider,
      this.store,
      filters,
      cancellation,
    );
    const partitions = groupPartitions(roots);
    const results = await Promise.all(
      partitions.map((replicas) => this.queryPartition(replicas, filters, limit, cancellation)),
    );
    cancellation.throwIfCancelled();

    const merged = mergeVerifiedEvents(results.flatMap((result) => result.events), limit);
    const events: NostrReaderQueryEvent[] = merged.map((event) => ({
      event,
      source: this.source,
      priority: this.priority,
    }));
    const reports = results.map((result) => result.report);

    return {
      events,
      complete: reports.every((report) => report.status !== 'unavailable'),
      partitions: reports,
    };
  }

  private async queryPartition(
    replicas: RootReplicaSnapshot[],
    filters: NostrFilter[],
    limit: number | undefined,
    cancellation: QueryCancellation,
  ): Promise<PartitionResult> {
    const partitionId = replicas[0]!.partitionId;
    const attempts: HashtreeNostrReplicaAttempt[] = [];

    for (const replica of replicas) {
      cancellation.throwIfCancelled();
      try {
        const events = await this.queryReplica(replica, filters, limit, cancellation);
        const status = events.length === 0 ? 'empty' : 'complete';
        attempts.push({
          partitionId,
          replicaId: replica.replicaId,
          status,
          eventCount: events.length,
        });
        return {
          events,
          report: {
            partitionId,
            status,
            selectedReplicaId: replica.replicaId,
            eventCount: events.length,
            attempts,
          },
        };
      } catch (error) {
        if (isCancellationError(error) || cancellation.signal?.aborted) throw error;
        const status = error instanceof HashtreeNostrReplicaCorruptError
          ? 'corrupt'
          : 'unavailable';
        attempts.push({
          partitionId,
          replicaId: replica.replicaId,
          status,
          eventCount: 0,
          reason: errorMessage(error),
        });
      }
    }

    return {
      events: [],
      report: {
        partitionId,
        status: 'unavailable',
        eventCount: 0,
        attempts,
      },
    };
  }

  private async queryReplica(
    replica: RootReplicaSnapshot,
    filters: NostrFilter[],
    limit: number | undefined,
    cancellation: QueryCancellation,
  ): Promise<NostrVerifiedEvent[]> {
    if (replica.root === null || limit === 0) return [];

    const guardedStore = new GuardedReplicaStore(replica.store, cancellation);
    const eventStore = new NostrEventStore(guardedStore);
    const effectiveFilters = filters.length === 0 ? [{}] : filters;

    let storedEvents: StoredNostrEvent[];
    try {
      const groups = await Promise.all(effectiveFilters.map(async (filter) => {
        const plan = planFilterQuery(filter, limit);
        if (plan.impossible) return [];
        return await eventStore.query(replica.root, plan.query, plan.options);
      }));
      storedEvents = dedupeStoredEvents(groups.flat());
    } catch (error) {
      if (isCancellationError(error) || cancellation.signal?.aborted) throw error;
      if (error instanceof HashtreeNostrReplicaUnavailableError) throw error;
      throw new HashtreeNostrReplicaCorruptError(
        'Hashtree Nostr replica contains unreadable index data',
        { cause: error },
      );
    }

    try {
      return storedEvents
        .map((event) => verifyAndFreezeNostrEvent(event))
        .filter((event) => filtersMatchNostrEvents(filters, event));
    } catch (error) {
      throw new HashtreeNostrReplicaCorruptError(
        'Hashtree Nostr replica contains an event with an invalid id or signature',
        { cause: error },
      );
    }
  }
}

function groupPartitions(roots: RootReplicaSnapshot[]): RootReplicaSnapshot[][] {
  const partitions = new Map<string, RootReplicaSnapshot[]>();
  for (const root of roots) {
    const replicas = partitions.get(root.partitionId) ?? [];
    replicas.push(root);
    partitions.set(root.partitionId, replicas);
  }
  return [...partitions.values()];
}

function dedupeStoredEvents(events: StoredNostrEvent[]): StoredNostrEvent[] {
  const byId = new Map<string, StoredNostrEvent>();
  for (const event of events) {
    const current = byId.get(event.id);
    if (!current || compareEventPayload(event, current) < 0) byId.set(event.id, event);
  }
  return [...byId.values()];
}

function mergeVerifiedEvents(
  events: NostrVerifiedEvent[],
  limit: number | undefined,
): NostrVerifiedEvent[] {
  const byId = new Map<string, NostrVerifiedEvent>();
  for (const event of events) {
    const current = byId.get(event.id);
    if (!current || compareEventPayload(event, current) < 0) byId.set(event.id, event);
  }
  const ordered = [...byId.values()].sort(compareNewestFirst);
  return limit === undefined ? ordered : ordered.slice(0, limit);
}

function compareNewestFirst(left: NostrVerifiedEvent, right: NostrVerifiedEvent): number {
  if (left.created_at !== right.created_at) return left.created_at > right.created_at ? -1 : 1;
  return left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
}

function compareEventPayload(left: StoredNostrEvent, right: StoredNostrEvent): number {
  const leftPayload = JSON.stringify(left);
  const rightPayload = JSON.stringify(right);
  return leftPayload < rightPayload ? -1 : leftPayload > rightPayload ? 1 : 0;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : 'Unknown replica failure';
}
