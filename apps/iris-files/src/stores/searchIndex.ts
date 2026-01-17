/**
 * Search Index Store - Uses SearchIndex from hashtree-index
 *
 * Indexes:
 * - Videos by keywords: "v:" prefix
 * - Users by name/npub/nip05: "u:" prefix
 */
import { writable, get } from 'svelte/store';
import { SearchIndex } from 'hashtree-index';
import { cid, toHex, fromHex, type CID } from 'hashtree';
import { localStore } from '../store';
import { DEFAULT_BOOTSTRAP_PUBKEY } from '../utils/constants';

const STORAGE_KEY = 'searchIndexRoot';
const INDEX_VERSION_KEY = 'searchIndexVersion';
const CURRENT_INDEX_VERSION = 2; // Increment when format changes

export interface VideoIndexEntry {
  title: string;
  nhash: string;
  pubkey: string;
  timestamp: number;
  treeName?: string;
  videoId?: string;
  duration?: number;
}

export interface UserIndexEntry {
  pubkey: string;
  npub: string;
  name?: string;
  displayName?: string;
  nip05?: string;
}

// SearchIndex instance (uses shared localStore)
const searchIndex = new SearchIndex(localStore, { order: 64 });

// Current index root CID
const indexRoot = writable<CID | null>(null);

// Load persisted index root on startup
function loadIndexRoot(): CID | null {
  try {
    // Check index version - clear if outdated
    const storedVersion = localStorage.getItem(INDEX_VERSION_KEY);
    const version = storedVersion ? parseInt(storedVersion, 10) : 0;
    if (version < CURRENT_INDEX_VERSION) {
      localStorage.removeItem(STORAGE_KEY);
      localStorage.setItem(INDEX_VERSION_KEY, CURRENT_INDEX_VERSION.toString());
      return null;
    }

    const stored = localStorage.getItem(STORAGE_KEY);
    if (!stored) return null;
    const parsed = JSON.parse(stored);
    if (parsed && parsed.hash && parsed.key) {
      return cid(fromHex(parsed.hash), fromHex(parsed.key));
    }
  } catch {
    // Ignore parse errors
  }
  return null;
}

// Tree name for search index on Nostr
const SEARCH_INDEX_TREE = '_search';

// Save index root to localStorage and publish to Nostr (debounced)
let saveTimeout: ReturnType<typeof setTimeout> | null = null;
let publishTimeout: ReturnType<typeof setTimeout> | null = null;

function saveIndexRoot(root: CID | null): void {
  if (saveTimeout) clearTimeout(saveTimeout);
  saveTimeout = setTimeout(() => {
    try {
      if (root) {
        localStorage.setItem(STORAGE_KEY, JSON.stringify({ hash: toHex(root.hash), key: toHex(root.key) }));
      } else {
        localStorage.removeItem(STORAGE_KEY);
      }
    } catch {
      // Ignore storage errors
    }
  }, 1000);

  // Also schedule publish to Nostr
  schedulePublish(root);
}

function schedulePublish(root: CID | null): void {
  if (publishTimeout) clearTimeout(publishTimeout);
  if (!root) return;

  publishTimeout = setTimeout(async () => {
    try {
      const { nostrStore } = await import('../nostr/store');
      const { getRefResolver } = await import('../refResolver');

      const state = nostrStore.getState();
      if (!state.npub || !state.isLoggedIn) return;

      const resolver = getRefResolver();
      await resolver.publish?.(
        `${state.npub}/${SEARCH_INDEX_TREE}`,
        root,
        { visibility: 'public' }
      );
    } catch (e) {
      console.error('Failed to publish search index:', e);
    }
  }, 3000);
}

// Initialize from localStorage
const initialRoot = loadIndexRoot();
if (initialRoot) {
  indexRoot.set(initialRoot);
}

// Persist on changes
indexRoot.subscribe(saveIndexRoot);

// Track if we're waiting for network index (after existing user login)
let waitingForNetwork = false;
let networkWaitTimeout: ReturnType<typeof setTimeout> | null = null;
const queuedOps: Array<() => void> = [];

/**
 * Call after existing user login (not new generated user).
 * Waits 2s for network index before processing queued operations.
 */
export function waitForNetworkIndex(): void {
  if (get(indexRoot)) return; // Already have local index

  waitingForNetwork = true;
  networkWaitTimeout = setTimeout(() => {
    waitingForNetwork = false;
    networkWaitTimeout = null;
    for (const op of queuedOps) op();
    queuedOps.length = 0;
  }, 2000);
}

/**
 * Call when receiving search index from network.
 * Merges with local and cancels network wait.
 */
export async function receiveNetworkIndex(remoteCid: CID, isNewer: boolean): Promise<void> {
  if (networkWaitTimeout) {
    clearTimeout(networkWaitTimeout);
    networkWaitTimeout = null;
  }

  const local = get(indexRoot);
  const merged = await searchIndex.merge(local, remoteCid, isNewer);
  indexRoot.set(merged);

  waitingForNetwork = false;
  for (const op of queuedOps) op();
  queuedOps.length = 0;
}

/**
 * Fetch a user's search index from Nostr
 */
async function fetchSearchIndex(npub: string): Promise<CID | null> {
  try {
    const { getRefResolver } = await import('../refResolver');
    const resolver = getRefResolver();
    const result = await resolver.resolve(`${npub}/${SEARCH_INDEX_TREE}`);
    return result ?? null;
  } catch {
    return null;
  }
}

/**
 * Merge bootstrap search index on app start.
 * Called after NDK is ready.
 */
export async function mergeBootstrapIndex(): Promise<void> {
  try {
    const { nip19 } = await import('nostr-tools');
    const bootstrapNpub = nip19.npubEncode(DEFAULT_BOOTSTRAP_PUBKEY);
    const remoteCid = await fetchSearchIndex(bootstrapNpub);
    if (remoteCid) {
      const local = get(indexRoot);
      // Bootstrap index is never "newer" - always prefer local values
      const merged = await searchIndex.merge(local, remoteCid, false);
      indexRoot.set(merged);
    }
  } catch (e) {
    console.error('Failed to merge bootstrap index:', e);
  }
}

// Re-export parseKeywords from SearchIndex for components that need it
export function parseKeywords(text: string): string[] {
  return searchIndex.parseKeywords(text);
}

// ============ Video Index ============

const pendingVideoOps: VideoIndexEntry[] = [];
let videoFlushTimeout: ReturnType<typeof setTimeout> | null = null;

/**
 * Index a video for search
 */
export async function indexVideo(entry: VideoIndexEntry): Promise<void> {
  const keywords = searchIndex.parseKeywords(entry.title);
  if (keywords.length === 0) return;

  pendingVideoOps.push(entry);

  if (videoFlushTimeout) clearTimeout(videoFlushTimeout);
  videoFlushTimeout = setTimeout(flushPendingVideoOps, 500);
}

async function flushPendingVideoOps(): Promise<void> {
  if (pendingVideoOps.length === 0) return;

  // Queue if waiting for network index
  if (waitingForNetwork) {
    queuedOps.push(() => flushPendingVideoOps());
    return;
  }

  const ops = [...pendingVideoOps];
  pendingVideoOps.length = 0;
  videoFlushTimeout = null;

  let root = get(indexRoot);

  for (const entry of ops) {
    const keywords = searchIndex.parseKeywords(entry.title);
    const value = JSON.stringify(entry);
    // Use pubkey:treeName as dedup key so re-indexing overwrites old entries
    // This ensures old nhashes are replaced when videos are re-indexed
    const id = entry.treeName
      ? `${entry.pubkey}:${entry.treeName}`
      : entry.nhash || `${entry.pubkey}:${entry.videoId || ''}`;

    try {
      root = await searchIndex.index(root, 'v:', keywords, id, value);
    } catch (e) {
      console.error('Failed to index video:', entry.title, e);
    }
  }

  indexRoot.set(root);
}

/**
 * Search videos by keyword
 */
export async function searchVideos(query: string, limit = 20): Promise<VideoIndexEntry[]> {
  const root = get(indexRoot);
  if (!root) return [];

  const results = await searchIndex.search(root, 'v:', query, { limit: limit * 2 });

  // Parse entries, deduplicate by pubkey:treeName, and sort by score then timestamp
  const seen = new Set<string>();
  const entries: Array<{ entry: VideoIndexEntry; score: number }> = [];

  for (const r of results) {
    const entry = JSON.parse(r.value) as VideoIndexEntry;
    // Dedup key matches indexing key format
    const key = entry.treeName
      ? `${entry.pubkey}:${entry.treeName}`
      : entry.nhash || `${entry.pubkey}:${entry.videoId || ''}`;
    if (!seen.has(key)) {
      seen.add(key);
      entries.push({ entry, score: r.score });
    }
  }

  entries.sort((a, b) => {
    if (b.score !== a.score) return b.score - a.score;
    return b.entry.timestamp - a.entry.timestamp;
  });

  return entries.slice(0, limit).map(e => e.entry);
}

// ============ User Index ============

const pendingUserOps: UserIndexEntry[] = [];
let userFlushTimeout: ReturnType<typeof setTimeout> | null = null;

/**
 * Index a user for search
 */
export async function indexUser(entry: UserIndexEntry): Promise<void> {
  pendingUserOps.push(entry);

  if (userFlushTimeout) clearTimeout(userFlushTimeout);
  userFlushTimeout = setTimeout(flushPendingUserOps, 500);
}

/**
 * Index multiple users at once (more efficient)
 */
export async function indexUsers(entries: UserIndexEntry[]): Promise<void> {
  pendingUserOps.push(...entries);

  if (userFlushTimeout) clearTimeout(userFlushTimeout);
  userFlushTimeout = setTimeout(flushPendingUserOps, 100);
}

/**
 * Extract searchable terms from a user entry
 */
function getUserSearchTerms(entry: UserIndexEntry): string[] {
  const terms: string[] = [];

  // Always index by npub
  terms.push(entry.npub.toLowerCase());

  // Index by name parts
  if (entry.name) {
    const nameParts = entry.name.toLowerCase().split(/\s+/).filter(p => p.length >= 2);
    terms.push(...nameParts);
  }

  // Index by display name parts
  if (entry.displayName && entry.displayName !== entry.name) {
    const displayParts = entry.displayName.toLowerCase().split(/\s+/).filter(p => p.length >= 2);
    terms.push(...displayParts);
  }

  // Index by nip05 username (part before @)
  if (entry.nip05) {
    const atIndex = entry.nip05.indexOf('@');
    const username = atIndex > 0 ? entry.nip05.slice(0, atIndex) : entry.nip05;
    if (username.length >= 2) {
      terms.push(username.toLowerCase());
    }
  }

  return [...new Set(terms)];
}

async function flushPendingUserOps(): Promise<void> {
  if (pendingUserOps.length === 0) return;

  // Queue if waiting for network index
  if (waitingForNetwork) {
    queuedOps.push(() => flushPendingUserOps());
    return;
  }

  const ops = [...pendingUserOps];
  pendingUserOps.length = 0;
  userFlushTimeout = null;

  let root = get(indexRoot);

  for (const entry of ops) {
    const terms = getUserSearchTerms(entry);
    const value = JSON.stringify(entry);

    try {
      root = await searchIndex.index(root, 'u:', terms, entry.npub, value);
    } catch (e) {
      console.error('Failed to index user:', entry.npub, e);
    }
  }

  indexRoot.set(root);
}

/**
 * Search users by name, nip05, or npub prefix
 */
export async function searchUsers(query: string, limit = 10): Promise<UserIndexEntry[]> {
  const root = get(indexRoot);
  if (!root) return [];

  const trimmed = query.trim().toLowerCase();
  if (trimmed.length < 1) return [];

  // For user search, we do a simple prefix search on the raw query
  // (not parsed into keywords) to support npub prefix matching
  const results = await searchIndex.search(root, 'u:', trimmed, { limit });

  return results.map(r => JSON.parse(r.value) as UserIndexEntry);
}

// ============ Index Management ============

/**
 * Get current index root (for persistence)
 */
export function getIndexRoot(): CID | null {
  return get(indexRoot);
}

/**
 * Set index root (for loading persisted index)
 */
export function setIndexRoot(root: CID | null): void {
  indexRoot.set(root);
}

/**
 * Subscribe to index root changes
 */
export const subscribeToIndexRoot = indexRoot.subscribe;

/**
 * Clear the search index
 */
export function clearIndex(): void {
  indexRoot.set(null);
  pendingVideoOps.length = 0;
  pendingUserOps.length = 0;
  if (videoFlushTimeout) {
    clearTimeout(videoFlushTimeout);
    videoFlushTimeout = null;
  }
  if (userFlushTimeout) {
    clearTimeout(userFlushTimeout);
    userFlushTimeout = null;
  }
}
