/**
 * Shared mesh transport types for hashtree.
 *
 * These primitives are transport-neutral inside the hashtree mesh stack even
 * when the default production composition uses Nostr signaling plus WebRTC.
 */

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

export type SignalingMessage =
  | HelloMessage
  | OfferMessage
  | AnswerMessage
  | CandidateMessage
  | CandidatesMessage;

// Directed messages (have targetPeerId) - excludes HelloMessage
export type DirectedMessage = OfferMessage | AnswerMessage | CandidateMessage | CandidatesMessage;

// HTL (Hops To Live) constants - Freenet-style probabilistic decrement
export const MAX_HTL = 10;
export const DECREMENT_AT_MAX_PROB = 0.5;
export const DECREMENT_AT_MIN_PROB = 0.25;

// Signaling kind for WebRTC / mesh events
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

// Fragment constants for transport message safety and bounded reassembly.
export const FRAGMENT_SIZE = 32 * 1024;
export const FRAGMENT_STALL_TIMEOUT = 5_000;
export const FRAGMENT_TOTAL_TIMEOUT = 120_000;
export const MAX_PENDING_REASSEMBLIES = 20;
export const MAX_PENDING_BYTES = 64 * 1024 * 1024;

// Message type bytes (prefix before MessagePack body)
export const MSG_TYPE_REQUEST = 0x00;
export const MSG_TYPE_RESPONSE = 0x01;

export interface DataRequest {
  h: Uint8Array;
  htl?: number;
}

export interface DataResponse {
  h: Uint8Array;
  d: Uint8Array;
  i?: number;
  n?: number;
}

export type DataMessage =
  | { type: typeof MSG_TYPE_REQUEST; body: DataRequest }
  | { type: typeof MSG_TYPE_RESPONSE; body: DataResponse };

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

// Peer pool types for prioritized connections
export type PeerPool = 'follows' | 'other';

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

export interface MeshStats {
  requestsSent: number;
  requestsReceived: number;
  responsesSent: number;
  responsesReceived: number;
  receiveErrors: number;
  blossomFetches: number;
  fragmentsSent: number;
  fragmentsReceived: number;
  fragmentTimeouts: number;
  reassembliesCompleted: number;
  bytesSent: number;
  bytesReceived: number;
  bytesForwarded: number;
  meshReceived: number;
  meshForwarded: number;
  meshDroppedDuplicate: number;
}

export type WebRTCStats = MeshStats;

export interface BandwidthSample {
  timestamp: number;
  bytesSent: number;
  bytesReceived: number;
}

export interface PendingReassembly {
  hash: Uint8Array;
  fragments: Map<number, Uint8Array>;
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
