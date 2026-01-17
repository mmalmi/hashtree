/**
 * Worker Protocol Types
 *
 * Message types for communication between main thread and hashtree worker.
 * Worker owns: HashTree, OpfsStore, WebRTC, Nostr (via nostr-tools)
 * Main thread owns: UI, NIP-07 extension access (signing/encryption)
 */

import type { CID } from '../types';

// Nostr types (simplified - don't want full nostr-tools dependency in protocol)
export interface NostrFilter {
  ids?: string[];
  authors?: string[];
  kinds?: number[];
  '#e'?: string[];
  '#p'?: string[];
  '#d'?: string[];
  since?: number;
  until?: number;
  limit?: number;
  [key: string]: string[] | number[] | number | undefined;
}

// SocialGraph event type (kind 3 contact list events)
export interface SocialGraphEvent {
  id: string;
  pubkey: string;
  kind: number;
  content: string;
  tags: string[][];
  created_at: number;
  sig: string;
}

export interface UnsignedEvent {
  kind: number;
  created_at: number;
  tags: string[][];
  content: string;
  pubkey?: string;
}

export interface SignedEvent extends UnsignedEvent {
  id: string;
  pubkey: string;
  sig: string;
}

export interface PeerStats {
  peerId: string;
  pubkey: string;
  connected: boolean;
  requestsSent: number;
  requestsReceived: number;
  responsesSent: number;
  responsesReceived: number;
  bytesSent: number;
  bytesReceived: number;
}

// ============================================================================
// Main Thread → Worker Messages
// ============================================================================

export type WorkerRequest =
  // Lifecycle
  | { type: 'init'; id: string; config: WorkerConfig }
  | { type: 'close'; id: string }
  | { type: 'setIdentity'; id: string; pubkey: string; nsec?: string }

  // Store operations (low-level hash-based)
  | { type: 'get'; id: string; hash: Uint8Array }
  | { type: 'put'; id: string; hash: Uint8Array; data: Uint8Array }
  | { type: 'has'; id: string; hash: Uint8Array }
  | { type: 'delete'; id: string; hash: Uint8Array }

  // Tree operations (high-level CID-based)
  | { type: 'readFile'; id: string; cid: CID }
  | { type: 'readFileRange'; id: string; cid: CID; start: number; end?: number }
  | { type: 'readFileStream'; id: string; cid: CID }
  | { type: 'writeFile'; id: string; parentCid: CID | null; path: string; data: Uint8Array }
  | { type: 'deleteFile'; id: string; parentCid: CID; path: string }
  | { type: 'listDir'; id: string; cid: CID }
  | { type: 'resolveRoot'; id: string; npub: string; path?: string }

  // Nostr subscriptions
  | { type: 'subscribe'; id: string; filters: NostrFilter[] }
  | { type: 'unsubscribe'; id: string; subId: string }
  | { type: 'publish'; id: string; event: SignedEvent }

  // Media streaming (service worker registers a MessagePort)
  | { type: 'registerMediaPort'; port: MessagePort }

  // Stats
  | { type: 'getPeerStats'; id: string }
  | { type: 'getRelayStats'; id: string }
  | { type: 'getStorageStats'; id: string }

  // WebRTC pool configuration
  | { type: 'setWebRTCPools'; id: string; pools: { follows: { max: number; satisfied: number }; other: { max: number; satisfied: number } } }
  | { type: 'sendWebRTCHello'; id: string }
  | { type: 'setFollows'; id: string; follows: string[] }
  | { type: 'blockPeer'; id: string; pubkey: string }

  // Blossom upload
  | { type: 'pushToBlossom'; id: string; cidHash: Uint8Array; cidKey?: Uint8Array }
  | { type: 'startBlossomSession'; id: string; sessionId: string; totalChunks: number }
  | { type: 'endBlossomSession'; id: string }

  // SocialGraph operations
  | { type: 'initSocialGraph'; id: string; rootPubkey?: string }
  | { type: 'setSocialGraphRoot'; id: string; pubkey: string }
  | { type: 'handleSocialGraphEvents'; id: string; events: SocialGraphEvent[] }
  | { type: 'getFollowDistance'; id: string; pubkey: string }
  | { type: 'isFollowing'; id: string; follower: string; followed: string }
  | { type: 'getFollows'; id: string; pubkey: string }
  | { type: 'getFollowers'; id: string; pubkey: string }
  | { type: 'getFollowedByFriends'; id: string; pubkey: string }
  | { type: 'getSocialGraphSize'; id: string }
  | { type: 'getUsersByDistance'; id: string; distance: number }

  // NIP-07 responses (main thread → worker, after signing/encryption)
  | { type: 'signed'; id: string; event?: SignedEvent; error?: string }
  | { type: 'encrypted'; id: string; ciphertext?: string; error?: string }
  | { type: 'decrypted'; id: string; plaintext?: string; error?: string }

  // WebRTC proxy events (main thread reports to worker)
  | WebRTCEvent;

/** Blossom server configuration */
export interface BlossomServerConfig {
  url: string;
  read?: boolean;  // Whether to read from this server (default true)
  write?: boolean; // Whether to write to this server
}

export interface WorkerConfig {
  relays: string[];
  blossomServers?: BlossomServerConfig[];  // Blossom servers with read/write config
  pubkey: string;  // User's pubkey (required - user always logged in)
  nsec?: string;  // Hex-encoded secret key (only for nsec login, not extension)
  storeName?: string;  // IndexedDB database name, defaults to 'hashtree-worker'
}

// ============================================================================
// Worker → Main Thread Messages
// ============================================================================

export type WorkerResponse =
  // Lifecycle
  | { type: 'ready' }
  | { type: 'error'; id?: string; error: string }

  // Generic responses
  | { type: 'result'; id: string; data?: Uint8Array; error?: string }
  | { type: 'bool'; id: string; value: boolean; error?: string }
  | { type: 'cid'; id: string; cid?: CID; error?: string }
  | { type: 'void'; id: string; error?: string }

  // Tree operations
  | { type: 'dirListing'; id: string; entries?: DirEntry[]; error?: string }
  | { type: 'streamChunk'; id: string; chunk: Uint8Array; done: boolean }

  // Nostr events
  | { type: 'event'; subId: string; event: SignedEvent }
  | { type: 'eose'; subId: string }

  // Stats
  | { type: 'peerStats'; id: string; stats: PeerStats[] }
  | { type: 'relayStats'; id: string; stats: RelayStats[] }
  | { type: 'storageStats'; id: string; items: number; bytes: number }

  // Blossom notifications
  | { type: 'blossomUploadError'; hash: string; error: string }
  | { type: 'blossomUploadProgress'; progress: BlossomUploadProgress }
  | { type: 'blossomPushResult'; id: string; pushed: number; skipped: number; failed: number; error?: string }

  // SocialGraph responses
  | { type: 'socialGraphReady'; id: string; version: number; size: number }
  | { type: 'socialGraphVersion'; version: number }
  | { type: 'followDistance'; id: string; distance: number }
  | { type: 'isFollowingResult'; id: string; result: boolean }
  | { type: 'pubkeyList'; id: string; pubkeys: string[] }
  | { type: 'socialGraphSize'; id: string; size: number }

  // NIP-07 requests (worker → main thread, needs extension)
  | { type: 'signEvent'; id: string; event: UnsignedEvent }
  | { type: 'nip44Encrypt'; id: string; pubkey: string; plaintext: string }
  | { type: 'nip44Decrypt'; id: string; pubkey: string; ciphertext: string }

  // WebRTC proxy commands (worker tells main thread what to do)
  | WebRTCCommand;

export interface DirEntry {
  name: string;
  isDir: boolean;
  size?: number;
  cid?: CID;
}

export interface RelayStats {
  url: string;
  connected: boolean;
  eventsReceived: number;
  eventsSent: number;
}

/** Per-server blossom upload status */
export interface BlossomServerStatus {
  url: string;
  uploaded: number;  // Number of chunks uploaded to this server
  failed: number;    // Number of chunks failed on this server
  skipped: number;   // Number of chunks already existed on this server
}

/** Overall blossom upload progress */
export interface BlossomUploadProgress {
  sessionId: string;           // Unique session identifier
  totalChunks: number;         // Total chunks to upload
  processedChunks: number;     // Chunks processed (uploaded + skipped + failed)
  servers: BlossomServerStatus[];  // Per-server status
}

// ============================================================================
// Service Worker ↔ Worker Messages (via MessagePort)
// ============================================================================

// Request by direct CID (for cached/known content)
export interface MediaRequestByCid {
  type: 'media';
  requestId: string;
  cid: string;  // hex encoded CID hash
  start: number;
  end?: number;
  mimeType?: string;
}

// Request by npub/path (supports live streaming via tree root updates)
export interface MediaRequestByPath {
  type: 'mediaByPath';
  requestId: string;
  npub: string;
  path: string;  // e.g., "public/video.webm"
  start: number;
  end?: number;
  mimeType?: string;
}

export type MediaRequest = MediaRequestByCid | MediaRequestByPath;

export type MediaResponse =
  | { type: 'headers'; requestId: string; totalSize: number; mimeType: string; isLive?: boolean }
  | { type: 'chunk'; requestId: string; data: Uint8Array }
  | { type: 'done'; requestId: string }
  | { type: 'error'; requestId: string; message: string };

// ============================================================================
// WebRTC Proxy Protocol (Worker ↔ Main Thread)
// Worker controls logic, main thread owns RTCPeerConnection
// ============================================================================

/** Worker → Main: Commands to control WebRTC connections */
export type WebRTCCommand =
  // Connection lifecycle
  | { type: 'rtc:createPeer'; peerId: string; pubkey: string }
  | { type: 'rtc:closePeer'; peerId: string }

  // SDP handling
  | { type: 'rtc:createOffer'; peerId: string }
  | { type: 'rtc:createAnswer'; peerId: string }
  | { type: 'rtc:setLocalDescription'; peerId: string; sdp: RTCSessionDescriptionInit }
  | { type: 'rtc:setRemoteDescription'; peerId: string; sdp: RTCSessionDescriptionInit }

  // ICE handling
  | { type: 'rtc:addIceCandidate'; peerId: string; candidate: RTCIceCandidateInit }

  // Data channel
  | { type: 'rtc:sendData'; peerId: string; data: Uint8Array };

/** Main → Worker: Events from WebRTC connections */
export type WebRTCEvent =
  // Connection state
  | { type: 'rtc:peerCreated'; peerId: string }
  | { type: 'rtc:peerStateChange'; peerId: string; state: RTCPeerConnectionState }
  | { type: 'rtc:peerClosed'; peerId: string }

  // SDP results
  | { type: 'rtc:offerCreated'; peerId: string; sdp: RTCSessionDescriptionInit }
  | { type: 'rtc:answerCreated'; peerId: string; sdp: RTCSessionDescriptionInit }
  | { type: 'rtc:descriptionSet'; peerId: string; error?: string }

  // ICE events
  | { type: 'rtc:iceCandidate'; peerId: string; candidate: RTCIceCandidateInit | null }
  | { type: 'rtc:iceGatheringComplete'; peerId: string }

  // Data channel
  | { type: 'rtc:dataChannelOpen'; peerId: string }
  | { type: 'rtc:dataChannelMessage'; peerId: string; data: Uint8Array }
  | { type: 'rtc:dataChannelClose'; peerId: string }
  | { type: 'rtc:dataChannelError'; peerId: string; error: string };

// ============================================================================
// Helper functions
// ============================================================================

let requestIdCounter = 0;

export function generateRequestId(): string {
  return `req_${Date.now()}_${++requestIdCounter}`;
}
