/**
 * WebRTC signaling types for hashtree P2P connections
 *
 * These types match the Rust hashtree-webrtc crate for interoperability.
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
  helloInterval?: number;         // default 10000ms
  messageTimeout?: number;        // default 15000ms
  requestTimeout?: number;        // default 500ms - fast fallback to Blossom
  peerQueryDelay?: number;        // default 500ms - delay between sequential peer queries
  relays?: string[];
  localStore?: import('../types.js').Store;
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
  fallbackStores?: import('../types.js').Store[];
  // Function to check if a peer is blocked (by pubkey)
  // Blocked peers won't be connected to
  isPeerBlocked?: (pubkey: string) => boolean;
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
}

export type WebRTCStoreEvent =
  | { type: 'peer-connected'; peerId: string }
  | { type: 'peer-disconnected'; peerId: string }
  | { type: 'update' };

export type WebRTCStoreEventHandler = (event: WebRTCStoreEvent) => void;

// Stats tracking
export interface WebRTCStats {
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
}

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
  readonly uuid: string;
  private readonly str: string;

  constructor(pubkey: string, uuid?: string) {
    this.pubkey = pubkey;
    this.uuid = uuid || generateUuid();
    this.str = `${pubkey}:${this.uuid}`;
  }

  toString(): string {
    return this.str;
  }

  short(): string {
    return `${this.pubkey.slice(0, 8)}:${this.uuid.slice(0, 6)}`;
  }

  static fromString(str: string): PeerId {
    const [pubkey, uuid] = str.split(':');
    if (!pubkey || !uuid) {
      throw new Error(`Invalid peer string: ${str}`);
    }
    return new PeerId(pubkey, uuid);
  }
}
