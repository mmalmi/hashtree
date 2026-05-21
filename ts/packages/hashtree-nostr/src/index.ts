/**
 * @hashtree/nostr - Nostr integration for hashtree
 *
 * Provides Nostr ref resolving and event storage. Transport-neutral mesh
 * primitives are re-exported for compatibility; blob transport now lives in
 * @hashtree/fips-transport so FIPS owns peer discovery and WebRTC/UDP links.
 */

export const DEFAULT_RELAYS: string[] = [
  'wss://relay.damus.io',
  'wss://relay.primal.net',
  'wss://relay.nostr.band',
  'wss://temp.iris.to',
  'wss://relay.snort.social',
];

export * from '@hashtree/mesh';

// Ref resolvers
export {
  createNostrRefResolver,
  // Legacy alias
  createNostrRefResolver as createNostrRootResolver,
  type NostrRefResolverConfig,
  // Legacy alias
  type NostrRefResolverConfig as NostrRootResolverConfig,
  type NostrEvent,
  type NostrFilter,
  type Nip19Like,
  type VisibilityCallbacks,
  type ParsedTreeVisibility,
} from './resolver/index.js';

// Event storage and indexing
export {
  NOSTR_EVENT_ENVELOPE_VERSION,
  NostrEventStore,
  decodeStoredNostrEventMsgpack,
  encodeStoredNostrEventMsgpack,
  type StoredNostrEvent,
  type NostrEventManifest,
  type ListEventsOptions,
  type NostrEventQuery,
  type NostrEventQueryValue,
} from './events.js';

export {
  COLLECTION_MANIFEST_METADATA_FILE,
  DEFAULT_NOSTR_EVENT_COLLECTION_SOURCE_ID,
  NOSTR_EVENT_ITEM_FORMAT,
  NOSTR_EVENT_PROJECTION_FORMAT,
  collectionManifestToNostrEventManifest,
  createNostrEventCollectionDefinition,
  createNostrEventCollectionSource,
  createNostrEventCollectionWriter,
  nostrEventCollectionManifestMetadata,
  nostrEventCollectionRootMetadata,
  nostrEventManifestToCollectionManifest,
} from './eventCollection.js';

export {
  MANIFEST_BY_AUTHOR_KIND_TIME,
  MANIFEST_BY_AUTHOR_TIME,
  MANIFEST_BY_ID,
  MANIFEST_BY_KIND_TIME,
  MANIFEST_BY_KIND_TIME_AUTHOR,
  MANIFEST_BY_TAG,
  MANIFEST_BY_TIME,
  MANIFEST_PARAMETERIZED_REPLACEABLE,
  MANIFEST_REPLACEABLE,
  parameterizedReplaceableKey,
  replaceableKey,
  kindTimeAuthorKey,
  tagPrefix,
} from './eventKeys.js';

export {
  encodeSignedNostrEventJson,
  decodeSignedNostrEventJson,
  storeSignedNostrEventSnapshot,
  readSignedNostrEventSnapshot,
  parseHashtreeRootEvent,
  type ParsedHashtreeRootEvent,
  type SnapshotTreeLike,
  type SnapshotTarget,
} from './snapshot.js';

export {
  storeTreeEventSnapshot,
  readTreeEventSnapshot,
  fetchLatestTreeEventSnapshot,
  watchLatestTreeEventSnapshot,
  compareTreeEventSnapshots,
  isNewerTreeEventSnapshot,
  snapshotMatchesRootCid,
  resolveSnapshotRootCid,
  type TreeEventSnapshotInfo,
  type TreeEventSnapshotQuery,
  type FetchLatestTreeEventSnapshotConfig,
  type WatchTreeEventSnapshotsConfig,
} from './treeEventSnapshots.js';

export {
  buildTreeEventSnapshotPermalink,
  parseTreeEventSnapshotPermalink,
  normalizeTreeEventSnapshotLinkKey,
  type TreeEventSnapshotPermalink,
  type BuildTreeEventSnapshotPermalinkOptions,
} from './treeEventSnapshotPermalinks.js';

export {
  createReplaceablePublishQueue,
  replaceableEventCoordinateFromTemplate,
  replaceableEventCoordinateKey,
  type ReplaceableEventCoordinate,
  type ReplaceableEventTemplateLike,
  type ReplaceablePublishQueueConfig,
  type ReplaceablePublishRequest,
  type ReplaceablePublishOutcome,
} from './replaceablePublish.js';
