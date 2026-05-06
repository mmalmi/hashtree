/**
 * @hashtree/nostr - Nostr integration for hashtree
 *
 * Provides WebRTC P2P storage and Nostr ref resolver
 */

// WebRTC P2P store
export {
  WebRTCStore,
  DEFAULT_RELAYS,
  Peer,
  PeerSelector,
  PeerId,
  generateUuid,
  MAX_HTL,
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
  MSG_TYPE_PUBSUB_INTEREST,
  MSG_TYPE_PUBSUB_FRAME,
  MSG_TYPE_PUBSUB_INVENTORY,
  MSG_TYPE_PUBSUB_WANT,
  FRAGMENT_SIZE,
  // Protocol functions
  encodeRequest,
  encodeResponse,
  encodePubsubInterest,
  encodePubsubFrame,
  encodePubsubInventory,
  encodePubsubWant,
  parseMessage,
  createRequest,
  createResponse,
  createFragmentResponse,
  createPubsubInterest,
  createPubsubFrame,
  createPubsubInventory,
  createPubsubWant,
  hashToKey,
  verifyHash,
  generatePeerHTLConfig,
  decrementHTLWithPolicy,
  decrementHTL,
  shouldForwardHTL,
  peerPrincipal,
  normalizeDispatchConfig,
  buildHedgedWavePlan,
  syncSelectorPeers,
  shouldForward,
  type SignalingMessage,
  type WebRTCStoreConfig,
  type PeerStatus,
  type WebRTCStoreEvent,
  type WebRTCStoreEventHandler,
  type EventSigner,
  type EventEncrypter,
  type EventDecrypter,
  type GiftWrapper,
  type GiftUnwrapper,
  type SignedEvent,
  type PeerPool,
  type PeerClassifier,
  type PoolConfig,
  type SelectionStrategy,
  type RequestDispatchConfig,
  type PersistedPeerMetadata,
  type PeerMetadataSnapshot,
  type MeshStats,
  type WebRTCStats,
  type BandwidthSample,
  type DataRequest,
  type DataResponse,
  type PubsubInterest,
  type PubsubFrame,
  type PubsubInventory,
  type PubsubWant,
  type PeerHTLConfig,
  type PendingRequest,
} from './webrtc/index.js';

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
