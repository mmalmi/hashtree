/**
 * WebRTC signaling types for hashtree P2P connections
 *
 * These types match the Rust hashtree-network mesh transport types.
 */

import type { Store } from '@hashtree/core';

// ICE candidate format (matches Rust IceCandidate)
export interface IceCandidate {
  candidate: string;
  sdpMLineIndex?: number;
  sdpMid?: string;
}

// Signaling message types (match Rust SignalingMessage enum)
export interface HelloMessage {
  type: 'hello';
  peerId: string;
  roots?: string[];
  hashGet?: boolean;
}

export interface OfferMessage {
  type: 'offer';
  peerId: string;
  targetPeerId: string;
  sdp: string;
}

export interface AnswerMessage {
  type: 'answer';
  peerId: string;
  targetPeerId: string;
  sdp: string;
}

export interface CandidateMessage {
  type: 'candidate';
  peerId: string;
  targetPeerId: string;
  candidate: string;
  sdpMLineIndex?: number;
  sdpMid?: string;
}

export interface CandidatesMessage {
  type: 'candidates';
  peerId: string;
  targetPeerId: string;
  candidates: IceCandidate[];
}

export type SignalingMessage = HelloMessage | OfferMessage | AnswerMessage | CandidateMessage | CandidatesMessage;

// Directed messages (have targetPeerId) - excludes HelloMessage
export type DirectedMessage = OfferMessage | AnswerMessage | CandidateMessage | CandidatesMessage;

// HTL (Hops To Live) constants - Freenet-style probabilistic decrement
export const MAX_HTL = 10;
export const DECREMENT_AT_MAX_PROB = 0.5;  // 50% chance to decrement at max
export const DECREMENT_AT_MIN_PROB = 0.25; // 25% chance to decrement at 1

// Signaling kind for WebRTC events
export const WEBRTC_KIND = 25050;

export enum HtlMode {
  Probabilistic = 'probabilistic',
}

export interface HtlPolicy {
  mode: HtlMode;
  maxHtl: number;
  pAtMax: number;
  pAtMin: number;
}

export const BLOB_REQUEST_POLICY: HtlPolicy = {
  mode: HtlMode.Probabilistic,
  maxHtl: MAX_HTL,
  pAtMax: DECREMENT_AT_MAX_PROB,
  pAtMin: DECREMENT_AT_MIN_PROB,
};

export const MESH_EVENT_POLICY: HtlPolicy = {
  mode: HtlMode.Probabilistic,
  maxHtl: 4,
  pAtMax: 0.75,
  pAtMin: 0.5,
};

export const MESH_PROTOCOL = 'htree.nostr.mesh.v1';
export const MESH_PROTOCOL_VERSION = 1;
export const MESH_DEFAULT_HTL = MESH_EVENT_POLICY.maxHtl;
export const MESH_MAX_HTL = 6;

// Fragment constants for WebRTC transport
export const FRAGMENT_SIZE = 32 * 1024;           // 32KB per WebRTC message (safe limit)
export const FRAGMENT_STALL_TIMEOUT = 5_000;      // 5s without fragment = stall
export const FRAGMENT_TOTAL_TIMEOUT = 120_000;    // 2min max for full chunk reassembly
export const MAX_PENDING_REASSEMBLIES = 20;       // Memory cap: max concurrent reassemblies
export const MAX_PENDING_BYTES = 64 * 1024 * 1024; // 64MB memory cap for reassembly buffers

// Message type bytes (prefix before MessagePack body)
export const MSG_TYPE_REQUEST = 0x00;
export const MSG_TYPE_RESPONSE = 0x01;

// Data channel protocol messages
// Wire format: [type byte][msgpack body]
// Request:  [0x00][msgpack: {h: bytes32, htl?: u8}]
// Response: [0x01][msgpack: {h: bytes32, d: bytes, i?: u32, n?: u32}]
// Fragmented responses include i (index) and n (total), unfragmented omit them

export interface DataRequest {
  h: Uint8Array;   // 32-byte hash
  htl?: number;    // Hops To Live (default MAX_HTL if not set)
}

export interface DataResponse {
  h: Uint8Array;   // 32-byte hash
  d: Uint8Array;   // Data (fragment or full)
  i?: number;      // Fragment index (0-based), absent = unfragmented
  n?: number;      // Total fragments, absent = unfragmented
}

export type DataMessage =
  | { type: typeof MSG_TYPE_REQUEST; body: DataRequest }
  | { type: typeof MSG_TYPE_RESPONSE; body: DataResponse };

// Signer function type (compatible with window.nostr.signEvent)
export type EventSigner = (event: {
  kind: number;
  created_at: number;
  tags: string[][];
  content: string;
}) => Promise<{ id: string; pubkey: string; sig: string; kind: number; created_at: number; tags: string[][]; content: string }>;

// Encrypter function type (compatible with window.nostr.nip04.encrypt)
export type EventEncrypter = (pubkey: string, plaintext: string) => Promise<string>;

// Decrypter function type (compatible with window.nostr.nip04.decrypt)
export type EventDecrypter = (pubkey: string, ciphertext: string) => Promise<string>;

// Signed event type (Nostr event with signature)
export interface SignedEvent {
  id: string;
  pubkey: string;
  sig: string;
  kind: number;
  created_at: number;
  tags: string[][];
  content: string;
}

export interface MeshNostrEventPayload {
  type: 'EVENT';
  event: SignedEvent;
}

export type MeshNostrPayload = MeshNostrEventPayload;

export interface MeshNostrFrame {
  protocol: string;
  version: number;
  frame_id: string;
  htl: number;
  sender_peer_id: string;
  payload: MeshNostrPayload;
}

export function createMeshNostrEventFrame(
  event: SignedEvent,
  senderPeerId: string,
  htl: number = MESH_DEFAULT_HTL,
): MeshNostrFrame {
  return {
    protocol: MESH_PROTOCOL,
    version: MESH_PROTOCOL_VERSION,
    frame_id: generateUuid(),
    htl,
    sender_peer_id: senderPeerId,
    payload: {
      type: 'EVENT',
      event,
    },
  };
}

export function validateMeshNostrFrame(frame: MeshNostrFrame): string | null {
  if (frame.protocol !== MESH_PROTOCOL) return 'invalid protocol';
  if (frame.version !== MESH_PROTOCOL_VERSION) return 'invalid version';
  if (!frame.frame_id) return 'missing frame id';
  if (!frame.sender_peer_id) return 'missing sender peer id';
  if (frame.sender_peer_id.includes(':')) return 'invalid sender peer id';
  if (frame.htl <= 0 || frame.htl > MESH_MAX_HTL) return 'invalid htl';
  if (frame.payload?.type !== 'EVENT') return 'invalid payload type';
  if (frame.payload.event.kind !== WEBRTC_KIND) return 'unsupported event kind';
  return null;
}

export function parseMeshNostrFrameText(text: string): MeshNostrFrame | null {
  try {
    const value = JSON.parse(text) as MeshNostrFrame;
    return validateMeshNostrFrame(value) === null ? value : null;
  } catch {
    return null;
  }
}

// Gift wrap function - wraps an inner event for a recipient
// Returns a kind 25050 ephemeral gift-wrapped event
export type GiftWrapper = (
  innerEvent: { kind: number; content: string; tags: string[][] },
  recipientPubkey: string,
) => Promise<SignedEvent>;

// Gift unwrap function - unwraps a received gift-wrapped event
// Returns the inner rumor event or null if can't decrypt
export type GiftUnwrapper = (
  event: SignedEvent,
) => Promise<{ pubkey: string; kind: number; content: string; tags: string[][] } | null>;

// Peer pool types for prioritized connections
export type PeerPool = 'follows' | 'other';

// Function to classify a peer into a pool based on pubkey
export type PeerClassifier = (pubkey: string) => PeerPool;

// Pool configuration
export interface PoolConfig {
  maxConnections: number;
  satisfiedConnections: number;
}

// Peer selection strategy for retrieval routing
export type SelectionStrategy =
  | 'weighted'
  | 'roundRobin'
  | 'random'
  | 'lowestLatency'
  | 'highestSuccessRate'
  | 'utilityUcb';

// Hedged request dispatch policy
export interface RequestDispatchConfig {
  initialFanout: number;
  hedgeFanout: number;
  maxFanout: number;
  hedgeIntervalMs: number;
}

export const PEER_METADATA_SNAPSHOT_VERSION = 1;

export interface PersistedPeerMetadata {
  principal: string;
  requestsSent: number;
  successes: number;
  timeouts: number;
  failures: number;
  srttMs: number;
  rttvarMs: number;
  rtoMs: number;
  bytesReceived: number;
  bytesSent: number;
}

export interface PeerMetadataSnapshot {
  version: number;
  peers: PersistedPeerMetadata[];
}

// Configuration
export interface WebRTCStoreConfig {
  signer: EventSigner;            // NIP-07 compatible signer
  pubkey: string;                 // signer's pubkey
  encrypt: EventEncrypter;        // NIP-44 compatible encrypter
  decrypt: EventDecrypter;        // NIP-44 compatible decrypter
  giftWrap: GiftWrapper;          // NIP-17 style gift wrap (kind 25050)
  giftUnwrap: GiftUnwrapper;      // NIP-17 style gift unwrap
  satisfiedConnections?: number;  // default 3 (legacy, used if no pools)
  maxConnections?: number;        // default 6 (legacy, used if no pools)
  helloInterval?: number;         // default 3000ms
  messageTimeout?: number;        // default 15000ms
  requestTimeout?: number;        // default 500ms - fast fallback to Blossom
  peerQueryDelay?: number;        // default 500ms - delay between sequential peer queries
  relays?: string[];
  localStore?: Store;
  debug?: boolean;
  // Pool-based peer management
  peerClassifier?: PeerClassifier;
  pools?: {
    follows: PoolConfig;
    other: PoolConfig;
  };
  // Function to get list of followed pubkeys for subscription filtering
  // When other pool is disabled, only subscribe to hellos from these pubkeys
  getFollowedPubkeys?: () => string[];
  // Fallback stores to try when WebRTC peers don't have the data
  // Tried in order after all WebRTC peers fail
  // Example: [new BlossomStore({ servers: ['https://hashtree.iris.to'] })]
  fallbackStores?: Store[];
  // Function to check if a peer is blocked (by pubkey)
  // Blocked peers won't be connected to
  isPeerBlocked?: (pubkey: string) => boolean;
  // Retrieval peer-selection strategy
  requestSelectionStrategy?: SelectionStrategy;
  // Whether fairness constraints are enabled in peer selector
  requestFairnessEnabled?: boolean;
  // Hedged dispatch fanout policy for peer retrieval
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
  isConnected?: boolean; // True when peer connection AND data channel are ready
  hashGet?: boolean;
}

export type WebRTCStoreEvent =
  | { type: 'peer-connected'; peerId: string }
  | { type: 'peer-disconnected'; peerId: string }
  | { type: 'update' };

export type WebRTCStoreEventHandler = (event: WebRTCStoreEvent) => void;

// Stats tracking
export interface MeshStats {
  requestsSent: number;           // Requests we sent to peers
  requestsReceived: number;       // Requests we received from peers
  responsesSent: number;          // Responses we sent to peers
  responsesReceived: number;      // Responses we received from peers
  receiveErrors: number;          // Errors handling incoming messages (parse, hash mismatch, etc)
  blossomFetches: number;         // Successful fetches from blossom fallback stores
  fragmentsSent: number;          // Fragment messages sent
  fragmentsReceived: number;      // Fragment messages received
  fragmentTimeouts: number;       // Reassemblies that timed out (stall or total)
  reassembliesCompleted: number;  // Successful reassemblies
  bytesSent: number;              // Total bytes sent (responses)
  bytesReceived: number;          // Total bytes received (responses)
  bytesForwarded: number;         // Bytes sent on behalf of forwarded requests (included in bytesSent)
  meshReceived: number;           // Relayless mesh frames accepted
  meshForwarded: number;          // Relayless mesh frames forwarded
  meshDroppedDuplicate: number;   // Relayless mesh frames/events dropped by dedupe
}

export type WebRTCStats = MeshStats;

// Bandwidth sample for rolling average calculation
export interface BandwidthSample {
  timestamp: number;
  bytesSent: number;
  bytesReceived: number;
}

// Fragment reassembly tracking
export interface PendingReassembly {
  hash: Uint8Array;
  fragments: Map<number, Uint8Array>;  // index → data
  totalExpected: number;
  receivedBytes: number;
  firstFragmentAt: number;
  lastFragmentAt: number;
}

export function generateUuid(): string {
  return Math.random().toString(36).substring(2, 15) +
    Math.random().toString(36).substring(2, 15);
}

export class PeerId {
  readonly pubkey: string;
  private readonly str: string;

  constructor(pubkey: string) {
    this.pubkey = pubkey;
    this.str = pubkey;
  }

  toString(): string {
    return this.str;
  }

  short(): string {
    return this.pubkey.slice(0, 8);
  }

  static fromString(str: string): PeerId {
    const pubkey = str.trim();
    if (!pubkey || pubkey.includes(':')) {
      throw new Error(`Invalid peer string: ${str}`);
    }
    return new PeerId(pubkey);
  }
}
