/**
 * HashTree - Simple content-addressed merkle tree
 *
 * Core principle: Every node is stored by SHA256(msgpack(node)) -> msgpack(node)
 * This enables pure KV content-addressed storage.
 */
import type { TreeVisibility } from './visibility.js';
export type { TreeVisibility };

/**
 * 32-byte SHA256 hash used as content address
 */
export type Hash = Uint8Array;

/**
 * Content Identifier - hash + optional decryption key
 *
 * For public content: just the hash
 * For encrypted content: hash + CHK decryption key
 */
export interface CID {
  /** SHA256 hash of the (encrypted) content */
  hash: Hash;
  /** CHK decryption key (for encrypted content) */
  key?: Uint8Array;
}

/**
 * Create a CID from hash and optional key
 */
export function cid(hash: Hash, key?: Uint8Array): CID {
  return key ? { hash, key } : { hash };
}

/**
 * Link/node types - what kind of content a link points to or a node contains
 */
export enum LinkType {
  /** Raw data blob (leaf chunk) */
  Blob = 0,
  /** Chunked file tree (TreeNode with unnamed links) */
  File = 1,
  /** Directory tree (TreeNode with named links) */
  Dir = 2,
}

/**
 * A link to a child node with optional metadata
 */
export interface Link {
  /** SHA256 hash of the child node's MessagePack encoding */
  hash: Hash;
  /** Optional name (for directory entries) */
  name?: string;
  /** Size of subtree in bytes (for efficient seeks). 0 for Dir links. */
  size: number;
  /** CHK decryption key (content hash) for encrypted nodes */
  key?: Uint8Array;
  /** Type of content this link points to: Blob, File, or Dir */
  type: LinkType;
  /** Optional metadata (for directory entries: createdAt, mimeType, thumbnail, etc.) */
  meta?: Record<string, unknown>;
}

/**
 * Tree node - contains links to children
 * Stored as: SHA256(msgpack(TreeNode)) -> msgpack(TreeNode)
 *
 * For directories: type=Dir, links have names
 * For chunked files: type=File, links are ordered chunks
 */
export interface TreeNode {
  /** Type of this node: File or Dir */
  type: LinkType.File | LinkType.Dir;
  /** Links to child nodes */
  links: Link[];
}

/**
 * Blob - raw data (leaf)
 * Stored as: SHA256(data) -> data
 *
 * Note: Blobs are stored directly as raw bytes, not as a structured type
 */
export type Blob = Uint8Array;

/**
 * Result of adding content to the tree
 */
export interface PutResult {
  /** Hash of the stored node */
  hash: Hash;
  /** Size of the stored data */
  size: number;
}

/**
 * Options for building trees
 */
export interface TreeOptions {
  /** Max links per tree node before splitting (default: 256) */
  fanout?: number;
  /** Max blob size before chunking (default: 2MB) */
  chunkSize?: number;
}

/**
 * Directory entry for building directory trees
 */
export interface DirEntry {
  name: string;
  hash: Hash;
  size: number;
  /** Type of content this entry points to: Blob, File, or Dir */
  type: LinkType;
}

/**
 * Content-addressed key-value store interface
 */
export interface Store {
  /**
   * Store data by its hash
   * @returns true if newly stored, false if already existed
   */
  put(hash: Hash, data: Uint8Array): Promise<boolean>;

  /**
   * Retrieve data by hash
   * @returns data or null if not found
   */
  get(hash: Hash): Promise<Uint8Array | null>;

  /**
   * Check if hash exists
   */
  has(hash: Hash): Promise<boolean>;

  /**
   * Delete by hash
   * @returns true if deleted, false if didn't exist
   */
  delete(hash: Hash): Promise<boolean>;
}

/**
 * Extended store with metadata support (e.g., Blossom)
 */
export interface StoreWithMeta extends Store {
  /**
   * Store with content type
   */
  put(hash: Hash, data: Uint8Array, contentType?: string): Promise<boolean>;
}

/**
 * Hex string representation of a hash
 */
export type HashHex = string;

/**
 * Convert hash to hex string
 */
export function toHex(hash: Hash): HashHex {
  return Array.from(hash).map(b => b.toString(16).padStart(2, '0')).join('');
}

/**
 * Convert hex string to hash
 */
export function fromHex(hex: HashHex): Hash {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.substr(i * 2, 2), 16);
  }
  return bytes;
}

/**
 * Compare two hashes for equality
 */
export function hashEquals(a: Hash, b: Hash): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

/**
 * Entry returned from RefResolver.list()
 */
export interface RefResolverListEntry {
  key: string;
  cid: CID;
  /** Tree visibility: public, link-visible, or private */
  visibility?: TreeVisibility;
  /** Encrypted key for link-visible trees - decrypt with link key from URL */
  encryptedKey?: string;
  /** Key ID for link-visible trees */
  keyId?: string;
  /** Self-encrypted key for private/link-visible trees - decrypt with NIP-44 */
  selfEncryptedKey?: string;
  /** Self-encrypted link key for link-visible trees - allows owner to recover link key */
  selfEncryptedLinkKey?: string;
  /** Unix timestamp when the tree was created/last updated */
  createdAt?: number;
}

/**
 * Visibility info passed to subscribe callbacks
 */
export interface SubscribeVisibilityInfo {
  /** Tree visibility: public, link-visible, or private */
  visibility: TreeVisibility;
  /** Encrypted key for link-visible trees - decrypt with link key from URL */
  encryptedKey?: string;
  /** Key ID for link-visible trees */
  keyId?: string;
  /** Self-encrypted key for private/link-visible trees - decrypt with NIP-44 */
  selfEncryptedKey?: string;
  /** Self-encrypted link key for link-visible trees - allows owner to recover link key for sharing */
  selfEncryptedLinkKey?: string;
}

/**
 * Options for publishing a tree
 */
export interface PublishOptions {
  /** Tree visibility: public (default), link-visible, or private */
  visibility?: TreeVisibility;
  /** Link key for link-visible trees - if not provided, one will be generated */
  linkKey?: Uint8Array;
  /** Additional l-tags to add (e.g., ['docs'] for document trees) */
  labels?: string[];
}

/**
 * Result from publishing a tree
 */
export interface PublishResult {
  /** Whether the publish succeeded */
  success: boolean;
  /** Link key for link-visible trees (include in URL as k= param) */
  linkKey?: Uint8Array;
}

/**
 * RefResolver - Maps human-readable keys to merkle root hashes (refs)
 *
 * This abstraction allows different backends (Nostr, DNS, HTTP, local storage)
 * to provide mutable pointers to immutable content-addressed data.
 *
 * Key format is implementation-specific, e.g.:
 * - Nostr: "npub1.../treename"
 * - DNS: "example.com/treename"
 * - Local: "local/mydata"
 *
 * All methods wait indefinitely until data is available - caller should apply timeout if needed.
 */
export interface RefResolver {
  /**
   * Resolve a key to its current CID.
   * Waits indefinitely until found - caller should apply timeout if needed.
   * @returns CID (never null - waits until found)
   */
  resolve(key: string): Promise<CID | null>;

  /**
   * Subscribe to CID changes for a key.
   * Callback fires immediately with current value (if available), then on each update.
   * Subscription stays open indefinitely until unsubscribed.
   *
   * @param key The key to watch
   * @param callback Called with new CID (or null if deleted/unavailable) and visibility info
   * @returns Unsubscribe function
   */
  subscribe(key: string, callback: (cid: CID | null, visibilityInfo?: SubscribeVisibilityInfo) => void): () => void;

  /**
   * Publish/update a CID (optional - only for writable backends)
   * Handles all visibility types: public, link-visible, private
   * @param key The key to publish to
   * @param cid The CID to publish (with encryption key if encrypted)
   * @param options Publish options (visibility, linkKey, labels)
   * @returns Result with success status and linkKey (for link-visible)
   */
  publish?(key: string, cid: CID, options?: PublishOptions): Promise<PublishResult>;

  /**
   * List all keys matching a prefix.
   * Streams results as they arrive - stays open indefinitely.
   * Callback fires on each new entry or update.
   *
   * @param prefix The prefix to watch (e.g., "npub1..." for all trees of a user)
   * @param callback Called with updated list as entries arrive (includes visibility info)
   * @returns Unsubscribe function
   */
  list?(prefix: string, callback: (entries: Array<RefResolverListEntry>) => void): () => void;

  /**
   * Stop the resolver and clean up resources
   */
  stop?(): void;

  /**
   * Inject a local list entry (for instant UI updates)
   * This makes trees appear immediately without waiting for network
   * @param entry The entry to inject with full visibility info
   */
  injectListEntry?(entry: RefResolverListEntry): void;

  /**
   * Delete a tree (publish event without hash to nullify)
   * @param key The key to delete (e.g., "npub1.../treename")
   * @returns true if deleted successfully
   */
  delete?(key: string): Promise<boolean>;
}
