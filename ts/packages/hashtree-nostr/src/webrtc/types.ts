/**
 * @hashtree/nostr-specific WebRTC store types layered on the shared mesh core.
 */

import type { Store } from '@hashtree/core';
import type {
  PeerPool,
  PoolConfig,
  SelectionStrategy,
  RequestDispatchConfig,
  MeshStats,
} from '@hashtree/mesh';

export {
  PeerId,
  WEBRTC_KIND,
  MAX_HTL,
  HtlMode,
  BLOB_REQUEST_POLICY,
  MESH_EVENT_POLICY,
  MESH_PROTOCOL,
  MESH_PROTOCOL_VERSION,
  MESH_DEFAULT_HTL,
  MESH_MAX_HTL,
  PEER_METADATA_SNAPSHOT_VERSION,
  DECREMENT_AT_MAX_PROB,
  DECREMENT_AT_MIN_PROB,
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
  MSG_TYPE_PUBSUB_INTEREST,
  MSG_TYPE_PUBSUB_FRAME,
  MSG_TYPE_PUBSUB_INVENTORY,
  MSG_TYPE_PUBSUB_WANT,
  FRAGMENT_SIZE,
  FRAGMENT_STALL_TIMEOUT,
  FRAGMENT_TOTAL_TIMEOUT,
  MAX_PENDING_REASSEMBLIES,
  MAX_PENDING_BYTES,
  createMeshNostrEventFrame,
  validateMeshNostrFrame,
  parseMeshNostrFrameText,
  generateUuid,
} from '@hashtree/mesh';

export type {
  IceCandidate,
  HelloMessage,
  OfferMessage,
  AnswerMessage,
  CandidateMessage,
  CandidatesMessage,
  SignalingMessage,
  DirectedMessage,
  HtlPolicy,
  DataRequest,
  DataResponse,
  PubsubInterest,
  PubsubFrame,
  PubsubInventory,
  PubsubWant,
  DataMessage,
  SignedEvent,
  MeshNostrPayload,
  MeshNostrEventPayload,
  MeshNostrFrame,
  PeerPool,
  PoolConfig,
  SelectionStrategy,
  RequestDispatchConfig,
  PersistedPeerMetadata,
  PeerMetadataSnapshot,
  MeshStats,
  WebRTCStats,
  BandwidthSample,
  PendingReassembly,
} from '@hashtree/mesh';

// Signer function type (compatible with window.nostr.signEvent)
export type EventSigner = (event: {
  kind: number;
  created_at: number;
  tags: string[][];
  content: string;
}) => Promise<{
  id: string;
  pubkey: string;
  sig: string;
  kind: number;
  created_at: number;
  tags: string[][];
  content: string;
}>;

// Encrypter function type (compatible with window.nostr.nip04.encrypt)
export type EventEncrypter = (pubkey: string, plaintext: string) => Promise<string>;

// Decrypter function type (compatible with window.nostr.nip04.decrypt)
export type EventDecrypter = (pubkey: string, ciphertext: string) => Promise<string>;

// Gift wrap function - wraps an inner event for a recipient
export type GiftWrapper = (
  innerEvent: { kind: number; content: string; tags: string[][] },
  recipientPubkey: string,
) => Promise<{
  id: string;
  pubkey: string;
  sig: string;
  kind: number;
  created_at: number;
  tags: string[][];
  content: string;
}>;

// Gift unwrap function - unwraps a received gift-wrapped event
export type GiftUnwrapper = (
  event: {
    id: string;
    pubkey: string;
    sig: string;
    kind: number;
    created_at: number;
    tags: string[][];
    content: string;
  },
) => Promise<{ pubkey: string; kind: number; content: string; tags: string[][] } | null>;

export type PeerClassifier = (pubkey: string) => PeerPool;

export interface WebRTCStoreConfig {
  signer: EventSigner;
  pubkey: string;
  encrypt: EventEncrypter;
  decrypt: EventDecrypter;
  giftWrap: GiftWrapper;
  giftUnwrap: GiftUnwrapper;
  satisfiedConnections?: number;
  maxConnections?: number;
  helloInterval?: number;
  messageTimeout?: number;
  requestTimeout?: number;
  peerQueryDelay?: number;
  relays?: string[];
  localStore?: Store;
  debug?: boolean;
  peerClassifier?: (pubkey: string) => PeerPool;
  pools?: {
    follows: PoolConfig;
    other: PoolConfig;
  };
  getFollowedPubkeys?: () => string[];
  fallbackStores?: Store[];
  isPeerBlocked?: (pubkey: string) => boolean;
  requestSelectionStrategy?: SelectionStrategy;
  requestFairnessEnabled?: boolean;
  requestDispatch?: RequestDispatchConfig;
}

export interface PeerStatus {
  peerId: string;
  pubkey: string;
  state: RTCPeerConnectionState | 'connected';
  direction: 'inbound' | 'outbound';
  connectedAt?: number;
  isSelf?: boolean;
  pool?: PeerPool;
  isConnected?: boolean;
  hashGet?: boolean;
}

export type WebRTCStoreEvent =
  | { type: 'peer-connected'; peerId: string }
  | { type: 'peer-disconnected'; peerId: string }
  | { type: 'update' };

export type WebRTCStoreEventHandler = (event: WebRTCStoreEvent) => void;
