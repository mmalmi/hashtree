/**
 * HashTree - Simple content-addressed merkle tree storage
 *
 * Browser-first, ESM-only library for building merkle trees
 * with content-hash addressing: SHA256(content) -> content
 */

// Core types
export type {
  Hash,
  CID,
  TreeNode,
  Blob,
  Link,
  Store,
  StoreWithMeta,
  RefResolver,
  RefResolverListEntry,
  SubscribeVisibilityInfo,
  PublishOptions,
  PublishResult,
  // Legacy alias
  RefResolver as RootResolver,
} from './types.js';

export {
  LinkType,
  toHex,
  fromHex,
  cid,
} from './types.js';

// Hash utilities
export { sha256 } from './hash.js';

// Encryption utilities
export {
  // CHK (Content Hash Key) encryption - deterministic, enables deduplication
  encryptChk,
  decryptChk,
  contentHash,
  encryptedSizeChk,
  // Legacy encryption with random IV (deprecated, use CHK)
  encrypt,
  decrypt,
  generateKey,
  keyToHex,
  keyFromHex,
  encryptedSize,
  plaintextSize,
  type EncryptionKey,
} from './crypto.js';

// MessagePack codec
export {
  encodeTreeNode,
  decodeTreeNode,
  tryDecodeTreeNode,
  encodeAndHash,
  getNodeType,
} from './codec.js';

// Storage adapters
export { MemoryStore } from './store/memory.js';
export { OpfsStore, type OpfsStoreOptions } from './store/opfs.js';
export {
  BlossomStore,
  type BlossomStoreConfig,
  type BlossomServer,
  type BlossomSigner,
  type BlossomAuthEvent,
  type BlossomLogEntry,
  type BlossomLogger,
  type BlossomUploadCallback,
} from './store/blossom.js';
export { DexieStore } from './store/dexie.js';

// HashTree - unified tree operations (create, read, edit, stream)
export {
  HashTree,
  StreamWriter,
  verifyTree,
  DEFAULT_CHUNK_SIZE,
  type HashTreeConfig,
  type TreeEntry,
  type DirEntry,
} from './hashtree.js';

// Stream options for readFileStream
export type { StreamOptions } from './tree/read.js';

// Chunker utilities
export {
  type Chunker,
  fixedChunker,
  videoChunker,
} from './builder.js';

// WebRTC P2P store
export {
  WebRTCStore,
  DEFAULT_RELAYS,
  Peer,
  PeerId,
  generateUuid,
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
  type WebRTCStats,
} from './webrtc/index.js';

// BEP52 (BitTorrent v2) compatible merkle tree
// Main API: Bep52TreeBuilder, Bep52StreamBuilder
// Low-level functions available via: import { bep52 } from 'hashtree'
export {
  BEP52_BLOCK_SIZE,
  ZERO_HASH,
  Bep52TreeBuilder,
  Bep52StreamBuilder,
  type Bep52Result,
  type Bep52Config,
} from './bep52.js';

// Re-export low-level BEP52 merkle functions as namespace
export * as bep52 from './bep52.js';

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
} from './resolver/index.js';

// Bech32 identifiers (nhash, npath)
export {
  nhashEncode,
  nhashDecode,
  npathEncode,
  npathDecode,
  decode,
  isNHash,
  isNPath,
  NHashTypeGuard,
  BECH32_REGEX,
  type NHashData,
  type NPathData,
  type DecodeResult,
} from './nhash.js';

// Tree visibility utilities (public/link-visible/private)
export {
  generateLinkKey,
  computeKeyId,
  encryptKeyForLink,
  decryptKeyFromLink,
  hex as visibilityHex,
  type TreeVisibility,
} from './visibility.js';

// Worker protocol types (for main thread ↔ worker communication)
export type {
  WorkerRequest,
  WorkerResponse,
  WorkerConfig,
  NostrFilter as WorkerNostrFilter,
  UnsignedEvent as WorkerUnsignedEvent,
  SignedEvent as WorkerSignedEvent,
  PeerStats as WorkerPeerStats,
  RelayStats as WorkerRelayStats,
  DirEntry as WorkerDirEntry,
  SocialGraphEvent as WorkerSocialGraphEvent,
  BlossomUploadProgress as WorkerBlossomUploadProgress,
  BlossomServerStatus as WorkerBlossomServerStatus,
  BlossomServerConfig as WorkerBlossomServerConfig,
  MediaRequest,
  MediaResponse,
  // WebRTC proxy protocol (worker controls, main executes)
  WebRTCCommand,
  WebRTCEvent,
} from './worker/protocol.js';

export { generateRequestId } from './worker/protocol.js';
