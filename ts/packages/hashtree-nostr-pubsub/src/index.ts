export { HashtreeNostrEventReader } from './reader.js';

export type {
  NostrEventReaderContract,
  NostrEvent,
  NostrEventSource,
  NostrEventSourceKind,
  NostrFilter,
  NostrReaderQueryEvent,
  NostrReaderQueryOptions,
  NostrReaderQueryReport,
  NostrVerifiedEvent,
} from './nostrTypes.js';

export type {
  HashtreeNostrEventBatchVerifier,
  HashtreeNostrEventReaderOptions,
  HashtreeNostrEventVerificationContext,
  HashtreeNostrPartitionReport,
  HashtreeNostrQueryOptions,
  HashtreeNostrQueryReport,
  HashtreeNostrReplicaAttempt,
  HashtreeNostrReplicaStatus,
  HashtreeNostrRootEntry,
  HashtreeNostrRootProvider,
  HashtreeNostrRootSnapshotContext,
  HashtreeNostrRoots,
} from './types.js';

export {
  HashtreeNostrFilterError,
  HashtreeNostrReplicaCorruptError,
  HashtreeNostrReplicaUnavailableError,
  HashtreeNostrUnsupportedSearchError,
} from './types.js';
