import type { CID, Store } from '@hashtree/core';
import type {
  NostrFilter,
  NostrEventSource,
  NostrReaderQueryOptions,
  NostrReaderQueryReport,
} from './nostrTypes.js';

export interface HashtreeNostrRootEntry {
  /** Distinct partition IDs are additive; equal IDs are ordered replicas. */
  partitionId: string;
  /** Stable diagnostic label. Defaults to the entry's position in the snapshot. */
  replicaId?: string;
  /** A null root is a valid, proven-empty replica. */
  root: CID | null;
  /** Optional replica-specific block source. */
  store?: Store;
}

export interface HashtreeNostrRootSnapshotContext {
  /** Defensive filter copies let catalog providers select relevant partitions. */
  filters: readonly NostrFilter[];
  signal?: AbortSignal;
  /** Absolute Unix epoch deadline in milliseconds. */
  deadline?: number;
}

export interface HashtreeNostrRootProvider {
  snapshot(
    context: HashtreeNostrRootSnapshotContext,
  ): Promise<readonly HashtreeNostrRootEntry[]> | readonly HashtreeNostrRootEntry[];
}

export type HashtreeNostrRoots =
  | CID
  | null
  | readonly HashtreeNostrRootEntry[]
  | HashtreeNostrRootProvider;

export interface HashtreeNostrEventReaderOptions {
  store: Store;
  roots: HashtreeNostrRoots;
  source?: NostrEventSource;
  sourceId?: string;
  priority?: number;
}

export type HashtreeNostrReplicaStatus =
  | 'complete'
  | 'empty'
  | 'unavailable'
  | 'corrupt';

export interface HashtreeNostrReplicaAttempt {
  partitionId: string;
  replicaId: string;
  status: HashtreeNostrReplicaStatus;
  eventCount: number;
  reason?: string;
}

export interface HashtreeNostrPartitionReport {
  partitionId: string;
  status: 'complete' | 'empty' | 'unavailable';
  selectedReplicaId?: string;
  eventCount: number;
  attempts: HashtreeNostrReplicaAttempt[];
}

export interface HashtreeNostrQueryReport extends NostrReaderQueryReport {
  /** False means at least one additive partition had no usable replica. */
  complete: boolean;
  partitions: HashtreeNostrPartitionReport[];
}

export interface HashtreeNostrQueryOptions extends NostrReaderQueryOptions {}

export class HashtreeNostrFilterError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'HashtreeNostrFilterError';
  }
}

export class HashtreeNostrUnsupportedSearchError extends HashtreeNostrFilterError {
  constructor() {
    super('NIP-50 search is not supported by the Hashtree Nostr event adapter');
    this.name = 'HashtreeNostrUnsupportedSearchError';
  }
}

export class HashtreeNostrReplicaUnavailableError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = 'HashtreeNostrReplicaUnavailableError';
  }
}

export class HashtreeNostrReplicaCorruptError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = 'HashtreeNostrReplicaCorruptError';
  }
}
