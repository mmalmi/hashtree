// @ts-nocheck
/**
 * Tree Root Subscription Handler
 *
 * Worker subscribes directly to tree root events (kind 30064 with legacy 30078 support).
 * Updates local cache and notifies main thread of changes.
 */

import type { CID } from '@hashtree/core';
import {
  HASHTREE_LABEL,
  HASHTREE_ROOT_KINDS,
  isHashtreeRootKind,
  storeTreeEventSnapshot,
  type StoredNostrEvent,
} from '@hashtree/nostr';
import { query, subscribe as nostrSubscribe, unsubscribe as nostrUnsubscribe } from './nostrRuntime';
import {
  compareReplaceableEventOrder,
  flushCachedRoots,
  getCachedRoot,
  getCachedRootInfo,
  getTreeRootCacheStore,
  isRedundantRootEvent,
  peekCachedRootInfo,
  setCachedRoot,
  stageCachedRoot,
} from './treeRootCache';
import { CoalescingBatch } from './coalescingBatch';
import type { StoreEntry } from './rootStoreWriter';
import type { SignedEvent, TreeVisibility } from './protocol';
import { nip19 } from 'nostr-tools';

// Active subscriptions by pubkey
const activeSubscriptions = new Map<string, string>(); // pubkeyHex -> subId
const inFlightRootResolutions = new Map<string, Promise<Uint8Array | null>>();
const inFlightHistoricalRootLists = new Map<string, Promise<CID[]>>();
const historicalRootListCache = new Map<string, { roots: CID[]; expiresAt: number }>();
const MAX_HISTORICAL_TREE_ROOT_EVENTS = 20;
const HISTORICAL_TREE_ROOT_CACHE_TTL_MS = 30_000;

// Callback to notify main thread
let notifyCallback: ((npub: string, treeName: string, record: TreeRootRecord) => void) | null = null;

export interface TreeRootRecord {
  hash: Uint8Array;
  key?: Uint8Array;
  visibility: TreeVisibility;
  labels?: string[];
  updatedAt: number;
  snapshotNhash?: string;
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
}

interface LegacyContentPayload {
  hash?: string;
  key?: string;
  visibility?: TreeVisibility;
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
}

export interface ParsedTreeRootEvent {
  hash: string;
  key?: string;
  visibility: TreeVisibility;
  labels?: string[];
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
}

async function storeSnapshotNhash(event: SignedEvent): Promise<string | undefined> {
  const snapshotTarget = getTreeRootCacheStore();
  if (!snapshotTarget) {
    return undefined;
  }

  const snapshot = await storeTreeEventSnapshot(
    snapshotTarget,
    nip19,
    event as StoredNostrEvent,
  );
  return snapshot?.snapshotNhash;
}

function uniqueEvents(events: SignedEvent[]): SignedEvent[] {
  const seen = new Set<string>();
  const result: SignedEvent[] = [];
  for (const event of events) {
    const eventKey = event.id || `${event.created_at}:${event.pubkey}:${event.tags.find((tag) => tag[0] === 'd')?.[1] ?? ''}`;
    if (seen.has(eventKey)) continue;
    seen.add(eventKey);
    result.push(event);
  }
  return result;
}

function compareReplaceableEvents(left: SignedEvent, right: SignedEvent): number {
  const leftCreatedAt = left.created_at ?? 0;
  const rightCreatedAt = right.created_at ?? 0;
  if (leftCreatedAt !== rightCreatedAt) {
    return rightCreatedAt - leftCreatedAt;
  }

  const leftId = left.id ?? '';
  const rightId = right.id ?? '';
  if (leftId === rightId) {
    return 0;
  }

  if (!leftId) return 1;
  if (!rightId) return -1;
  return leftId.localeCompare(rightId);
}

function parseLabels(event: SignedEvent): string[] | undefined {
  const seen = new Set<string>();
  const labels: string[] = [];
  for (const tag of event.tags) {
    if (tag[0] !== 'l' || !tag[1] || seen.has(tag[1])) continue;
    seen.add(tag[1]);
    labels.push(tag[1]);
  }
  return labels.length > 0 ? labels : undefined;
}

function parseLegacyContent(event: SignedEvent): LegacyContentPayload | null {
  const content = event.content?.trim();
  if (!content) return null;

  try {
    const parsed = JSON.parse(content);
    if (parsed && typeof parsed === 'object') {
      const payload = parsed as Record<string, unknown>;
      return {
        hash: typeof payload.hash === 'string' ? payload.hash : undefined,
        key: typeof payload.key === 'string' ? payload.key : undefined,
        visibility: typeof payload.visibility === 'string' ? payload.visibility as TreeVisibility : undefined,
        encryptedKey: typeof payload.encryptedKey === 'string' ? payload.encryptedKey : undefined,
        keyId: typeof payload.keyId === 'string' ? payload.keyId : undefined,
        selfEncryptedKey: typeof payload.selfEncryptedKey === 'string' ? payload.selfEncryptedKey : undefined,
        selfEncryptedLinkKey: typeof payload.selfEncryptedLinkKey === 'string' ? payload.selfEncryptedLinkKey : undefined,
      };
    }
  } catch {
    // Ignore JSON parse errors.
  }

  if (/^[0-9a-fA-F]{64}$/.test(content)) {
    return { hash: content };
  }

  return null;
}

export function parseTreeRootEvent(event: SignedEvent): ParsedTreeRootEvent | null {
  const hashTag = event.tags.find(t => t[0] === 'hash')?.[1];
  const legacyContent = hashTag ? null : parseLegacyContent(event);
  const hash = hashTag ?? legacyContent?.hash;
  if (!hash) return null;

  const keyTag = event.tags.find(t => t[0] === 'key')?.[1];
  const encryptedKeyTag = event.tags.find(t => t[0] === 'encryptedKey')?.[1];
  const keyIdTag = event.tags.find(t => t[0] === 'keyId')?.[1];
  const selfEncryptedKeyTag = event.tags.find(t => t[0] === 'selfEncryptedKey')?.[1];
  const selfEncryptedLinkKeyTag = event.tags.find(t => t[0] === 'selfEncryptedLinkKey')?.[1];

  const key = keyTag ?? legacyContent?.key;
  const encryptedKey = encryptedKeyTag ?? legacyContent?.encryptedKey;
  const keyId = keyIdTag ?? legacyContent?.keyId;
  const selfEncryptedKey = selfEncryptedKeyTag ?? legacyContent?.selfEncryptedKey;
  const selfEncryptedLinkKey = selfEncryptedLinkKeyTag ?? legacyContent?.selfEncryptedLinkKey;

  let visibility: TreeVisibility;
  if (encryptedKey) {
    visibility = 'link-visible';
  } else if (selfEncryptedKey) {
    visibility = 'private';
  } else {
    visibility = legacyContent?.visibility ?? 'public';
  }

  return {
    hash,
    key,
    visibility,
    labels: parseLabels(event),
    encryptedKey,
    keyId,
    selfEncryptedKey,
    selfEncryptedLinkKey,
  };
}

async function fetchTreeRootEvents(
  pubkeyHex: string, treeName: string, timeoutMs: number,
): Promise<SignedEvent[]> {
  const result = await query([{
    kinds: [...HASHTREE_ROOT_KINDS], authors: [pubkeyHex], '#d': [treeName],
    limit: MAX_HISTORICAL_TREE_ROOT_EVENTS,
  }], { deadline: Date.now() + timeoutMs });
  if (!result.complete && result.events.length === 0) {
    throw new Error('Tree history is not available yet');
  }
  return uniqueEvents(result.events).sort(compareReplaceableEvents);
}

function cidKey(cid: CID): string {
  const hash = Array.from(cid.hash, (byte) => byte.toString(16).padStart(2, '0')).join('');
  const key = cid.key
    ? Array.from(cid.key, (byte) => byte.toString(16).padStart(2, '0')).join('')
    : '';
  return `${hash}:${key}`;
}

function dedupeRoots(roots: CID[]): CID[] {
  const seen = new Set<string>();
  const result: CID[] = [];
  for (const root of roots) {
    const key = cidKey(root);
    if (seen.has(key)) continue;
    seen.add(key);
    result.push(root);
  }
  return result;
}

export async function getHistoricalTreeRoots(
  npub: string,
  treeName: string,
  timeoutMs: number = 8000,
): Promise<CID[]> {
  const cacheKey = `${npub}/${treeName}`;
  const cached = historicalRootListCache.get(cacheKey);
  if (cached && cached.expiresAt > Date.now()) {
    return cached.roots;
  }
  if (cached) {
    historicalRootListCache.delete(cacheKey);
  }

  const inFlight = inFlightHistoricalRootLists.get(cacheKey);
  if (inFlight) {
    return await inFlight;
  }

  const lookup = (async (): Promise<CID[]> => {
    let pubkeyHex: string;
    try {
      const decoded = nip19.decode(npub);
      if (decoded.type !== 'npub') return [];
      pubkeyHex = decoded.data as string;
    } catch {
      return [];
    }

    const events = await fetchTreeRootEvents(pubkeyHex, treeName, timeoutMs);

    const roots = dedupeRoots(
      uniqueEvents(events)
        .sort(compareReplaceableEvents)
        .map((event) => {
          const parsed = parseTreeRootEvent(event);
          if (!parsed) return null;
          try {
            return {
              hash: hexToBytes(parsed.hash),
              key: parsed.key ? hexToBytes(parsed.key) : undefined,
            } as CID;
          } catch {
            return null;
          }
        })
        .filter((root): root is CID => !!root),
    );

    historicalRootListCache.set(cacheKey, {
      roots,
      expiresAt: Date.now() + HISTORICAL_TREE_ROOT_CACHE_TTL_MS,
    });
    return roots;
  })();

  inFlightHistoricalRootLists.set(cacheKey, lookup);
  try {
    return await lookup;
  } finally {
    inFlightHistoricalRootLists.delete(cacheKey);
  }
}

export async function resolveTreeRootNow(
  npub: string,
  treeName: string,
  timeoutMs: number = 8000,
): Promise<CID | null> {
  const cached = await getCachedRoot(npub, treeName);
  if (cached) {
    return cached;
  }

  const cacheKey = `${npub}/${treeName}`;
  const inFlight = inFlightRootResolutions.get(cacheKey);
  if (inFlight) {
    return await inFlight;
  }

  const lookup = (async (): Promise<CID | null> => {
    let pubkeyHex: string;
    try {
      const decoded = nip19.decode(npub);
      if (decoded.type !== 'npub') return null;
      pubkeyHex = decoded.data as string;
    } catch {
      return null;
    }

    const fetched = (
      await fetchTreeRootEvents(pubkeyHex, treeName, timeoutMs)
    )[0];
    if (!fetched) {
      return null;
    }

    const parsed = parseTreeRootEvent(fetched);
    if (!parsed) {
      return null;
    }

    const hash = hexToBytes(parsed.hash);
    const key = parsed.key ? hexToBytes(parsed.key) : undefined;
    const { applied, record } = await setCachedRoot(npub, treeName, { hash, key }, parsed.visibility, {
      updatedAt: fetched.created_at,
      eventId: fetched.id,
      labels: parsed.labels,
      snapshotNhash: await storeSnapshotNhash(fetched),
      encryptedKey: parsed.encryptedKey,
      keyId: parsed.keyId,
      selfEncryptedKey: parsed.selfEncryptedKey,
      selfEncryptedLinkKey: parsed.selfEncryptedLinkKey,
    });
    historicalRootListCache.delete(cacheKey);

    if (notifyCallback && applied) {
      notifyCallback(npub, treeName, {
        hash: record.hash,
        key: record.key,
        visibility: record.visibility,
        labels: record.labels,
        updatedAt: record.updatedAt,
        snapshotNhash: record.snapshotNhash,
        encryptedKey: record.encryptedKey,
        keyId: record.keyId,
        selfEncryptedKey: record.selfEncryptedKey,
        selfEncryptedLinkKey: record.selfEncryptedLinkKey,
      });
    }

    return { hash: record.hash, key: record.key };
  })();

  inFlightRootResolutions.set(cacheKey, lookup);
  try {
    return await lookup;
  } finally {
    inFlightRootResolutions.delete(cacheKey);
  }
}

/**
 * Set callback to notify main thread of tree root updates
 */
export function setNotifyCallback(
  callback: (npub: string, treeName: string, record: TreeRootRecord) => void
): void {
  notifyCallback = callback;
}

/**
 * Subscribe to tree roots for a specific pubkey
 */
export function subscribeToTreeRoots(pubkeyHex: string): () => void {
  // Already subscribed?
  if (activeSubscriptions.has(pubkeyHex)) {
    return () => unsubscribeFromTreeRoots(pubkeyHex);
  }

  const subId = `tree-${pubkeyHex.slice(0, 8)}`;
  activeSubscriptions.set(pubkeyHex, subId);

  nostrSubscribe(subId, [{
    kinds: [...HASHTREE_ROOT_KINDS],
    authors: [pubkeyHex],
  }], {
    cache: 'cache-first',
  });

  return () => unsubscribeFromTreeRoots(pubkeyHex);
}

/**
 * Unsubscribe from tree roots for a specific pubkey
 */
export function unsubscribeFromTreeRoots(pubkeyHex: string): void {
  const subId = activeSubscriptions.get(pubkeyHex);
  if (subId) {
    nostrUnsubscribe(subId);
    activeSubscriptions.delete(pubkeyHex);
  }
}

/**
 * Handle incoming tree root event.
 * Called from worker.ts event router
 */
function hasLabel(event: SignedEvent, label: string): boolean {
  return event.tags.some(tag => tag[0] === 'l' && tag[1] === label);
}

function hasAnyLabel(event: SignedEvent): boolean {
  return event.tags.some(tag => tag[0] === 'l');
}

interface RootEventCandidate {
  event: SignedEvent;
  parsed: ParsedTreeRootEvent;
  npub: string;
  treeName: string;
}

const npubByPubkey = new Map<string, string>();

function npubFor(pubkeyHex: string): string {
  let npub = npubByPubkey.get(pubkeyHex);
  if (!npub) {
    npub = nip19.npubEncode(pubkeyHex);
    if (npubByPubkey.size >= 1000) npubByPubkey.clear();
    npubByPubkey.set(pubkeyHex, npub);
  }
  return npub;
}

// Newest pending root event per tree; a burst is applied in grouped rounds.
const rootEvents = new CoalescingBatch<RootEventCandidate>(
  (candidate, current) => compareReplaceableEventOrder(
    candidate.event.created_at,
    candidate.event.id,
    current.event.created_at,
    current.event.id,
  ) > 0,
  applyRootEvents,
);

/**
 * Hash the signed-event snapshot without writing it: the blocks are returned
 * so they can be stored in the same grouped write as the root record.
 */
async function prepareSnapshot(
  event: SignedEvent,
): Promise<{ snapshotNhash?: string; snapshotBlobs: StoreEntry[] }> {
  const snapshotBlobs: StoreEntry[] = [];
  if (!getTreeRootCacheStore()) {
    return { snapshotBlobs };
  }
  const snapshot = await storeTreeEventSnapshot(
    {
      put: async (hash, data) => {
        snapshotBlobs.push({ hash, data });
        return true;
      },
      get: async () => null,
      has: async () => false,
      delete: async () => false,
    },
    nip19,
    event as StoredNostrEvent,
  );
  return { snapshotNhash: snapshot?.snapshotNhash, snapshotBlobs };
}

async function stageRootEvent(
  candidate: RootEventCandidate,
  snapshot: { snapshotNhash?: string; snapshotBlobs: StoreEntry[] },
): Promise<void> {
  const { event, parsed, npub, treeName } = candidate;
  const hash = hexToBytes(parsed.hash);
  const key = parsed.key ? hexToBytes(parsed.key) : undefined;
  const visibility: TreeVisibility = parsed.visibility || 'public';

  const { applied, record } = await stageCachedRoot(npub, treeName, { hash, key }, visibility, {
    updatedAt: event.created_at,
    eventId: event.id,
    labels: parsed.labels,
    snapshotNhash: snapshot.snapshotNhash,
    snapshotBlobs: snapshot.snapshotBlobs,
    encryptedKey: parsed.encryptedKey,
    keyId: parsed.keyId,
    selfEncryptedKey: parsed.selfEncryptedKey,
    selfEncryptedLinkKey: parsed.selfEncryptedLinkKey,
  });
  if (!applied) return;
  historicalRootListCache.delete(`${npub}/${treeName}`);

  // Notify main thread
  if (notifyCallback) {
    notifyCallback(npub, treeName, record);
  }
}

async function applyRootEvents(round: RootEventCandidate[]): Promise<void> {
  const prepared = await Promise.all(round.map(async (candidate) => {
    try {
      const existing = await getCachedRootInfo(candidate.npub, candidate.treeName);
      if (existing && isRedundantRootEvent(existing, candidate.event.created_at, candidate.event.id)) {
        return null;
      }
      return { candidate, snapshot: await prepareSnapshot(candidate.event) };
    } catch (error) {
      console.warn('[Worker] Failed to handle tree root event:', error);
      return null;
    }
  }));

  for (const entry of prepared) {
    if (!entry) continue;
    try {
      await stageRootEvent(entry.candidate, entry.snapshot);
    } catch (error) {
      console.warn('[Worker] Failed to handle tree root event:', error);
    }
  }

  await flushCachedRoots();
}

/**
 * Accept a tree root event without waiting on the store.
 *
 * Events that carry nothing new for the cached root are dropped at once.
 * Otherwise the newest event per tree is applied to the in-memory cache and
 * reported, and its snapshot and root record are persisted in grouped
 * writes. The returned promise resolves once that persistence has finished.
 */
export function handleTreeRootEvent(event: SignedEvent): Promise<void> {
  // Extract tree name from #d tag
  const dTag = event.tags.find(t => t[0] === 'd');
  if (!dTag || !dTag[1]) return Promise.resolve();
  const treeName = dTag[1];

  // Accept unlabeled legacy events, ignore other labeled apps.
  if (hasAnyLabel(event) && !hasLabel(event, HASHTREE_LABEL)) return Promise.resolve();

  const parsed = parseTreeRootEvent(event);
  if (!parsed) return Promise.resolve();

  const npub = npubFor(event.pubkey);
  const known = peekCachedRootInfo(npub, treeName);
  if (known && isRedundantRootEvent(known, event.created_at, event.id)) {
    return rootEvents.settled();
  }

  return rootEvents.offer(`${npub}/${treeName}`, { event, parsed, npub, treeName });
}

/**
 * Check if an event is a tree root event
 */
export function isTreeRootEvent(event: SignedEvent): boolean {
  if (!isHashtreeRootKind(event.kind)) return false;
  if (hasLabel(event, HASHTREE_LABEL)) return true;
  return !hasAnyLabel(event);
}

/**
 * Get all active subscription pubkeys
 */
export function getActiveSubscriptions(): string[] {
  return Array.from(activeSubscriptions.keys());
}

// Helper: hex string to Uint8Array
function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}
