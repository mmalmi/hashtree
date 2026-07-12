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
  RefResolverSubscriptionMetadata,
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
export {
  FallbackStore,
  type FallbackStoreConfig,
  type ReadableStore,
  type WritableStore,
} from './store/fallback.js';

// HashTree - unified tree operations (create, read, edit, stream)
export {
  HashTree,
  StreamWriter,
  verifyTree,
  DEFAULT_CHUNK_SIZE,
  DEFAULT_MAX_LINKS,
  type HashTreeConfig,
  type TreeEntry,
  type DirEntry,
} from './hashtree.js';

// Stream options for readFileStream
export type { StreamOptions } from './tree/read.js';

// Block loader: waits for a block to become available rather than returning
// null when the data isn't local yet. Used internally by directory reads.
export { loadBlock } from './tree/loadBlock.js';

// Chunker utilities
export {
  type Chunker,
  fixedChunker,
  videoChunker,
} from './builder.js';

// Shared streaming upload helper
export {
  streamUploadWithProgress,
  type StreamUploadPhase,
  type StreamUploadProgress,
  type StreamUploadFile,
  type StreamUploadWriter,
  type StreamUploadOptions,
} from './upload.js';


// BEP52 (BitTorrent v2) compatible merkle tree
// Main API: Bep52TreeBuilder, Bep52StreamBuilder
// Low-level functions available via: import { bep52 } from '@hashtree/core'
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
  BlossomBandwidthStats as WorkerBlossomBandwidthStats,
  BlossomBandwidthServerStats as WorkerBlossomBandwidthServerStats,
  RelayStats as WorkerRelayStats,
  DirEntry as WorkerDirEntry,
  SocialGraphEvent as WorkerSocialGraphEvent,
  BlossomUploadProgress as WorkerBlossomUploadProgress,
  BlossomServerStatus as WorkerBlossomServerStatus,
  BlossomServerConfig as WorkerBlossomServerConfig,
  MediaRequest,
  MediaResponse,
} from './worker/protocol.js';

export { generateRequestId } from './worker/protocol.js';
