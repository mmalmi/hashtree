/**
 * Tree Root Subscription Handler
 *
 * Worker subscribes directly to tree root events (kind 30078 with #l=hashtree).
 * Updates local cache and notifies main thread of changes.
 */

import { subscribe as ndkSubscribe, unsubscribe as ndkUnsubscribe } from './ndk';
import { setCachedRoot } from './treeRootCache';
import type { SignedEvent, TreeVisibility } from './protocol';
import { nip19 } from 'nostr-tools';

// Active subscriptions by pubkey
const activeSubscriptions = new Map<string, string>(); // pubkeyHex -> subId

// Callback to notify main thread
let notifyCallback: ((npub: string, treeName: string, record: TreeRootRecord) => void) | null = null;

export interface TreeRootRecord {
  hash: Uint8Array;
  key?: Uint8Array;
  visibility: TreeVisibility;
  updatedAt: number;
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
  encryptedKey?: string;
  keyId?: string;
  selfEncryptedKey?: string;
  selfEncryptedLinkKey?: string;
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
    encryptedKey,
    keyId,
    selfEncryptedKey,
    selfEncryptedLinkKey,
  };
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

  ndkSubscribe(subId, [{
    kinds: [30078],
    authors: [pubkeyHex],
  }]);

  return () => unsubscribeFromTreeRoots(pubkeyHex);
}

/**
 * Unsubscribe from tree roots for a specific pubkey
 */
export function unsubscribeFromTreeRoots(pubkeyHex: string): void {
  const subId = activeSubscriptions.get(pubkeyHex);
  if (subId) {
    ndkUnsubscribe(subId);
    activeSubscriptions.delete(pubkeyHex);
  }
}

/**
 * Handle incoming tree root event (kind 30078 with #l=hashtree)
 * Called from worker.ts event router
 */
function hasLabel(event: SignedEvent, label: string): boolean {
  return event.tags.some(tag => tag[0] === 'l' && tag[1] === label);
}

function hasAnyLabel(event: SignedEvent): boolean {
  return event.tags.some(tag => tag[0] === 'l');
}

export async function handleTreeRootEvent(event: SignedEvent): Promise<void> {
  // Extract tree name from #d tag
  const dTag = event.tags.find(t => t[0] === 'd');
  if (!dTag || !dTag[1]) return;
  const treeName = dTag[1];

  // Accept unlabeled legacy events, ignore other labeled apps.
  if (hasAnyLabel(event) && !hasLabel(event, 'hashtree')) return;

  const parsed = parseTreeRootEvent(event);
  if (!parsed) return;

  // Convert pubkey to npub
  const npub = nip19.npubEncode(event.pubkey);

  // Parse hash and optional key
  const hash = hexToBytes(parsed.hash);
  const key = parsed.key ? hexToBytes(parsed.key) : undefined;
  const visibility: TreeVisibility = parsed.visibility || 'public';

  // Build record
  const record: TreeRootRecord = {
    hash,
    key,
    visibility,
    updatedAt: event.created_at,
    encryptedKey: parsed.encryptedKey,
    keyId: parsed.keyId,
    selfEncryptedKey: parsed.selfEncryptedKey,
    selfEncryptedLinkKey: parsed.selfEncryptedLinkKey,
  };

  // Update cache
  await setCachedRoot(npub, treeName, { hash, key }, visibility, {
    encryptedKey: parsed.encryptedKey,
    keyId: parsed.keyId,
    selfEncryptedKey: parsed.selfEncryptedKey,
    selfEncryptedLinkKey: parsed.selfEncryptedLinkKey,
  });

  // Notify main thread
  if (notifyCallback) {
    notifyCallback(npub, treeName, record);
  }
}

/**
 * Check if an event is a tree root event
 */
export function isTreeRootEvent(event: SignedEvent): boolean {
  if (event.kind !== 30078) return false;
  if (hasLabel(event, 'hashtree')) return true;
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
