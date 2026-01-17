/**
 * NostrRefResolver - Maps npub/treename keys to merkle root hashes (refs)
 *
 * Key format: "npub1.../treename"
 *
 * This resolver provides direct callback subscriptions that bypass React's
 * render cycle. Components can subscribe to hash changes and update directly
 * (e.g., MediaSource append) without triggering re-renders.
 */
import type { RefResolver, CID, RefResolverListEntry, SubscribeVisibilityInfo, PublishOptions, PublishResult } from '../types.js';
import { fromHex, toHex, cid } from '../types.js';
import {
  encryptKeyForLink,
  computeKeyId,
  generateLinkKey,
  type TreeVisibility,
} from '../visibility.js';

// Nostr event structure (minimal)
export interface NostrEvent {
  id?: string;
  pubkey: string;
  kind: number;
  content: string;
  tags: string[][];
  created_at: number;
}

// Type definitions for nip19
export interface Nip19Like {
  decode(str: string): { type: string; data: unknown };
  npubEncode(pubkey: string): string;
}

// Filter for querying events
export interface NostrFilter {
  kinds?: number[];
  authors?: string[];
  '#d'?: string[];
  '#l'?: string[];
}

// Subscription entry
interface SubscriptionEntry {
  unsubscribe: () => void;
  callbacks: Set<(cid: CID | null, visibilityInfo?: SubscribeVisibilityInfo) => void>;
  currentHash: string | null;
  currentKey: string | null;
  currentVisibility: SubscribeVisibilityInfo | null;
  latestCreatedAt: number;
}

/**
 * Parsed visibility info from a nostr event
 */
export interface ParsedTreeVisibility {
  hash: string;
  visibility: TreeVisibility;
  /** Plaintext key (for public trees) */
  key?: string;
  /** Encrypted key (for link-visible trees) - decrypt with link key */
  encryptedKey?: string;
  /** Key ID (for link-visible trees) - identifies which link key to use */
  keyId?: string;
  /** Self-encrypted key (for private/link-visible trees) - decrypt with NIP-44 */
  selfEncryptedKey?: string;
  /** Self-encrypted link key (for link-visible trees) - allows owner to recover link key for sharing */
  selfEncryptedLinkKey?: string;
}

/**
 * Parse hash and visibility info from a nostr event
 * Supports all visibility levels: public, link-visible, private
 */
function parseHashAndVisibility(event: NostrEvent): ParsedTreeVisibility | null {
  const hashTag = event.tags.find(t => t[0] === 'hash')?.[1];
  if (!hashTag) return null;

  const key = event.tags.find(t => t[0] === 'key')?.[1];
  const encryptedKey = event.tags.find(t => t[0] === 'encryptedKey')?.[1];
  const keyId = event.tags.find(t => t[0] === 'keyId')?.[1];
  const selfEncryptedKey = event.tags.find(t => t[0] === 'selfEncryptedKey')?.[1];
  const selfEncryptedLinkKey = event.tags.find(t => t[0] === 'selfEncryptedLinkKey')?.[1];

  let visibility: TreeVisibility;
  if (encryptedKey) {
    // encryptedKey means link-visible (shareable via link)
    // May also have selfEncryptedKey for owner access
    visibility = 'link-visible';
  } else if (selfEncryptedKey) {
    // Only selfEncryptedKey (no encryptedKey) means private
    visibility = 'private';
  } else {
    visibility = 'public';
  }

  return { hash: hashTag, visibility, key, encryptedKey, keyId, selfEncryptedKey, selfEncryptedLinkKey };
}

/**
 * Legacy parse function for backwards compatibility
 * @deprecated Use parseHashAndVisibility instead
 */
function parseHashAndKey(event: NostrEvent): { hash: string; key?: string } | null {
  const result = parseHashAndVisibility(event);
  if (!result) return null;
  return { hash: result.hash, key: result.key };
}

/** Event type for publishing (created_at optional - usually auto-set by NDK) */
export type NostrPublishEvent = Omit<NostrEvent, 'id' | 'pubkey' | 'created_at'> & { created_at?: number };

/**
 * Visibility callbacks for encryption/decryption
 * Used for private trees (NIP-44 self-encryption)
 */
export interface VisibilityCallbacks {
  /** NIP-44 encrypt data to a pubkey */
  encrypt?: (data: Uint8Array, toPubkey: string) => Promise<string>;
  /** NIP-44 decrypt data from a pubkey */
  decrypt?: (ciphertext: string, fromPubkey: string) => Promise<Uint8Array | null>;
  /** Get link key from URL context (for link-visible trees) */
  getLinkKey?: () => Uint8Array | null;
}

export interface NostrRefResolverConfig {
  /** Subscribe to nostr events - returns unsubscribe function */
  subscribe: (filter: NostrFilter, onEvent: (event: NostrEvent) => void) => () => void;
  /** Publish a nostr event - returns true on success. created_at is optional (for delete events) */
  publish: (event: NostrPublishEvent) => Promise<boolean>;
  /** Get current user's pubkey (for ownership checks) */
  getPubkey: () => string | null;
  /** nip19 encode/decode functions from nostr-tools */
  nip19: Nip19Like;
  /** Optional visibility callbacks for private/link-visible trees */
  visibility?: VisibilityCallbacks;
}

/**
 * Create a NostrRefResolver instance
 */

export function createNostrRefResolver(config: NostrRefResolverConfig): RefResolver {
  const { subscribe: nostrSubscribe, publish: nostrPublish, getPubkey, nip19, visibility } = config;

  // Active subscriptions by key
  const subscriptions = new Map<string, SubscriptionEntry>();

  // Active list subscriptions by npub prefix
  // Supports multiple callbacks per npub (multiple components can subscribe)
  interface ListSubscriptionEntry {
    npub: string;
    entriesByDTag: Map<string, ParsedTreeVisibility & { created_at: number }>;
    callbacks: Set<(entries: RefResolverListEntry[]) => void>;
    unsubscribe: () => void;
  }
  const listSubscriptions = new Map<string, ListSubscriptionEntry>();

  // Persistent local cache for list entries (survives subscription lifecycle)
  // Key is npub, value is map of tree name -> entry
  const localListCache = new Map<string, Map<string, ParsedTreeVisibility & { created_at: number }>>();

  /**
   * Parse a pointer key into pubkey and tree name
   * Key format: "npub1.../treename" or "npub1.../path/to/treename"
   */
  function parseKey(key: string): { pubkey: string; treeName: string } | null {
    const slashIdx = key.indexOf('/');
    if (slashIdx === -1) return null;

    const npubStr = key.slice(0, slashIdx);
    const treeName = key.slice(slashIdx + 1);
    if (!treeName) return null;

    try {
      const decoded = nip19.decode(npubStr);
      if (decoded.type !== 'npub') return null;
      return { pubkey: decoded.data as string, treeName };
    } catch {
      return null;
    }
  }

  return {
    /**
     * Resolve a key to its current CID.
     * Waits indefinitely until found - caller should apply timeout if needed.
     */
    async resolve(key: string): Promise<CID | null> {
      const parsed = parseKey(key);
      if (!parsed) return null;

      const { pubkey, treeName } = parsed;

      return new Promise((resolve) => {
        let latestData: { hash: string; key?: string } | null = null;
        let latestCreatedAt = 0;
        let resolveTimeout: ReturnType<typeof setTimeout> | null = null;

        const doResolve = () => {
          unsubscribe();
          if (latestData) {
            resolve(cid(fromHex(latestData.hash), latestData.key ? fromHex(latestData.key) : undefined));
          } else {
            resolve(null);
          }
        };

        const unsubscribe = nostrSubscribe(
          {
            kinds: [30078],
            authors: [pubkey],
            '#d': [treeName],
            '#l': ['hashtree'],
          },
          (event) => {
            const dTag = event.tags.find(t => t[0] === 'd')?.[1];
            if (dTag !== treeName) return;

            const hashAndKey = parseHashAndKey(event);
            if (!hashAndKey) return;

            if ((event.created_at || 0) > latestCreatedAt) {
              latestCreatedAt = event.created_at || 0;
              latestData = hashAndKey;
            }

            // Debounce: wait 100ms after last event to allow newer events to arrive
            // This handles the case where relay sends multiple events for replaceable events
            if (resolveTimeout) {
              clearTimeout(resolveTimeout);
            }
            resolveTimeout = setTimeout(doResolve, 100);
          }
        );
      });
    },

    /**
     * Subscribe to CID changes for a key.
     * Callback fires on each update (including initial value).
     * This runs outside React render cycle.
     */
    subscribe(key: string, callback: (cid: CID | null, visibilityInfo?: SubscribeVisibilityInfo) => void): () => void {
      const parsed = parseKey(key);
      if (!parsed) {
        callback(null);
        return () => {};
      }

      const { pubkey, treeName } = parsed;

      // Check if we already have a subscription for this key
      let sub = subscriptions.get(key);

      if (sub) {
        // Add callback to existing subscription
        sub.callbacks.add(callback);
        // Fire immediately with current value
        console.log('[Resolver] Reusing subscription for', key, {
          hasCurrentHash: !!sub.currentHash,
          currentVisibility: sub.currentVisibility ? JSON.stringify(sub.currentVisibility) : 'null'
        });
        if (sub.currentHash) {
          const keyBytes = sub.currentKey ? fromHex(sub.currentKey) : undefined;
          callback(cid(fromHex(sub.currentHash), keyBytes), sub.currentVisibility ?? undefined);
        }
      } else {
        // Helper to notify all callbacks
        const notifyCallbacks = (subEntry: SubscriptionEntry) => {
          if (!subEntry.currentHash) return;
          const keyBytes = subEntry.currentKey ? fromHex(subEntry.currentKey) : undefined;
          const visibilityInfo = subEntry.currentVisibility ?? undefined;
          for (const cb of subEntry.callbacks) {
            try {
              cb(cid(fromHex(subEntry.currentHash), keyBytes), visibilityInfo);
            } catch (e) {
              console.error('Resolver callback error:', e);
            }
          }
        };

        // Create new subscription for live updates
        const unsubscribe = nostrSubscribe(
          {
            kinds: [30078],
            authors: [pubkey],
            '#d': [treeName],
            '#l': ['hashtree'],
          },
          (event) => {
            const dTag = event.tags.find(t => t[0] === 'd')?.[1];
            if (dTag !== treeName) return;

            const subEntry = subscriptions.get(key);
            if (!subEntry) return;

            const visibilityData = parseHashAndVisibility(event);
            console.log('[Resolver] Event received for', key, {
              visibilityData: visibilityData ? {
                visibility: visibilityData.visibility,
                hasSelfEncryptedLinkKey: !!visibilityData.selfEncryptedLinkKey,
                hasEncryptedKey: !!visibilityData.encryptedKey
              } : 'null'
            });
            if (!visibilityData) return;

            const eventCreatedAt = event.created_at || 0;
            const newHash = visibilityData.hash;
            const newKey = visibilityData.key;

            // Only update if this event is newer (or same timestamp with different hash)
            if (eventCreatedAt >= subEntry.latestCreatedAt && newHash && newHash !== subEntry.currentHash) {
              subEntry.currentHash = newHash;
              subEntry.currentKey = newKey || null;
              subEntry.latestCreatedAt = eventCreatedAt;

              // Build visibility info for callback
              const visibilityInfo: SubscribeVisibilityInfo = {
                visibility: visibilityData.visibility,
                encryptedKey: visibilityData.encryptedKey,
                keyId: visibilityData.keyId,
                selfEncryptedKey: visibilityData.selfEncryptedKey,
                selfEncryptedLinkKey: visibilityData.selfEncryptedLinkKey,
              };
              subEntry.currentVisibility = visibilityInfo;

              // Update localListCache so other subscriptions can find this data
              const npubStr = key.split('/')[0];
              let npubCache = localListCache.get(npubStr);
              if (!npubCache) {
                npubCache = new Map();
                localListCache.set(npubStr, npubCache);
              }
              npubCache.set(treeName, {
                hash: newHash,
                visibility: visibilityData.visibility,
                key: newKey,
                encryptedKey: visibilityData.encryptedKey,
                keyId: visibilityData.keyId,
                selfEncryptedKey: visibilityData.selfEncryptedKey,
                selfEncryptedLinkKey: visibilityData.selfEncryptedLinkKey,
                created_at: eventCreatedAt,
              });

              notifyCallbacks(subEntry);
            }
          }
        );

        // Check localListCache for initial values (from recent publish calls)
        const npubStr = key.split('/')[0];
        const cachedEntry = localListCache.get(npubStr)?.get(treeName);

        sub = {
          unsubscribe,
          callbacks: new Set([callback]),
          currentHash: cachedEntry?.hash ?? null,
          currentKey: cachedEntry?.key ?? null,
          currentVisibility: cachedEntry ? {
            visibility: cachedEntry.visibility,
            encryptedKey: cachedEntry.encryptedKey,
            keyId: cachedEntry.keyId,
            selfEncryptedKey: cachedEntry.selfEncryptedKey,
            selfEncryptedLinkKey: cachedEntry.selfEncryptedLinkKey,
          } : null,
          latestCreatedAt: cachedEntry?.created_at ?? 0,
        };
        subscriptions.set(key, sub);

        // Fire callback immediately with cached value if available
        if (cachedEntry?.hash) {
          const keyBytes = cachedEntry.key ? fromHex(cachedEntry.key) : undefined;
          callback(cid(fromHex(cachedEntry.hash), keyBytes), sub.currentVisibility ?? undefined);
        }
      }

      // Return unsubscribe function
      return () => {
        const subEntry = subscriptions.get(key);
        if (!subEntry) return;

        subEntry.callbacks.delete(callback);

        // If no more callbacks, close the subscription
        if (subEntry.callbacks.size === 0) {
          subEntry.unsubscribe();
          subscriptions.delete(key);
        }
      };
    },

    /**
     * Publish/update a pointer
     * Handles all visibility types: public, link-visible, private
     * Updates local cache immediately (optimistic), then publishes to network.
     * @param key - The key to publish to
     * @param rootCid - The CID to publish (with encryption key if encrypted)
     * @param options - Publish options (visibility, linkKey, labels)
     * @returns Result with success status and linkKey (for link-visible)
     */
    async publish(key: string, rootCid: CID, options?: PublishOptions): Promise<PublishResult> {
      const parsed = parseKey(key);
      if (!parsed) return { success: false };

      const { treeName } = parsed;
      const pubkey = getPubkey();

      if (!pubkey) return { success: false };

      const visibilityType = options?.visibility ?? 'public';
      const hashHex = toHex(rootCid.hash);
      const chkKey = rootCid.key; // Raw encryption key (Uint8Array)
      const chkKeyHex = chkKey ? toHex(chkKey) : undefined;
      const now = Math.floor(Date.now() / 1000);
      const npubStr = key.split('/')[0];

      // Build visibility info for caches and tags
      const visibilityInfo: SubscribeVisibilityInfo = { visibility: visibilityType };
      let resultLinkKey: Uint8Array | undefined;

      // Build nostr event tags
      const tags: string[][] = [
        ['d', treeName],
        ['l', 'hashtree'],
        ['hash', hashHex],
      ];

      // Add directory prefix labels for discoverability
      const parts = treeName.split('/');
      for (let i = 1; i < parts.length; i++) {
        const prefix = parts.slice(0, i).join('/');
        tags.push(['l', prefix]);
      }

      // Add extra labels if provided
      if (options?.labels) {
        for (const label of options.labels) {
          tags.push(['l', label]);
        }
      }

      // Handle visibility-specific encryption and tags
      if (chkKey) {
        switch (visibilityType) {
          case 'public':
            // Plaintext key - anyone can access
            tags.push(['key', chkKeyHex!]);
            visibilityInfo.encryptedKey = undefined;
            visibilityInfo.keyId = undefined;
            visibilityInfo.selfEncryptedKey = undefined;
            break;

          case 'link-visible': {
            // Encrypt key with link key (XOR one-time pad)
            const linkKey = options?.linkKey ?? generateLinkKey();
            resultLinkKey = linkKey;
            const encryptedKey = encryptKeyForLink(chkKey, linkKey);
            const keyId = await computeKeyId(linkKey);
            tags.push(['encryptedKey', toHex(encryptedKey)]);
            tags.push(['keyId', toHex(keyId)]);
            visibilityInfo.encryptedKey = toHex(encryptedKey);
            visibilityInfo.keyId = toHex(keyId);

            if (visibility?.encrypt) {
              // Encrypt contentKey to self (so owner can always access without linkKey)
              try {
                const selfEncrypted = await visibility.encrypt(chkKey, pubkey);
                tags.push(['selfEncryptedKey', selfEncrypted]);
                visibilityInfo.selfEncryptedKey = selfEncrypted;
              } catch (e) {
                console.error('Failed to self-encrypt content key:', e);
              }

              // Encrypt linkKey to self for URL recovery (so owner can share URLs easily)
              // Owner can also derive linkKey from XOR(encryptedKey, contentKey)
              try {
                const selfEncryptedLinkKey = await visibility.encrypt(linkKey, pubkey);
                tags.push(['selfEncryptedLinkKey', selfEncryptedLinkKey]);
                visibilityInfo.selfEncryptedLinkKey = selfEncryptedLinkKey;
              } catch (e) {
                console.error('Failed to self-encrypt link key:', e);
              }
            }
            break;
          }

          case 'private': {
            // Encrypt key to self using NIP-44
            if (!visibility?.encrypt) {
              console.error('Cannot publish private tree: no encrypt callback provided');
              return { success: false };
            }
            try {
              const selfEncrypted = await visibility.encrypt(chkKey, pubkey);
              tags.push(['selfEncryptedKey', selfEncrypted]);
              visibilityInfo.selfEncryptedKey = selfEncrypted;
            } catch (e) {
              console.error('Failed to encrypt key:', e);
              return { success: false };
            }
            break;
          }
        }
      }

      // 1. Update local caches FIRST (optimistic update for instant UI)

      // Update local list cache (persists even without active subscription)
      let npubCache = localListCache.get(npubStr);
      if (!npubCache) {
        npubCache = new Map();
        localListCache.set(npubStr, npubCache);
      }
      npubCache.set(treeName, {
        hash: hashHex,
        visibility: visibilityType,
        key: visibilityType === 'public' ? chkKeyHex : undefined,
        encryptedKey: visibilityInfo.encryptedKey,
        keyId: visibilityInfo.keyId,
        selfEncryptedKey: visibilityInfo.selfEncryptedKey,
        selfEncryptedLinkKey: visibilityInfo.selfEncryptedLinkKey,
        created_at: now,
      });

      // Update active subscription state
      const sub = subscriptions.get(key);
      if (sub) {
        sub.currentHash = hashHex;
        sub.currentKey = visibilityType === 'public' ? chkKeyHex || null : null;
        sub.latestCreatedAt = now;
        sub.currentVisibility = visibilityInfo;
        // Notify callbacks with CID
        for (const cb of sub.callbacks) {
          try {
            cb(rootCid, visibilityInfo);
          } catch (e) {
            console.error('Resolver callback error:', e);
          }
        }
      }

      // Update active list subscriptions
      const listSub = listSubscriptions.get(npubStr);
      if (listSub) {
        const existingEntry = listSub.entriesByDTag.get(treeName);
        const existingWasDeleted = existingEntry && !existingEntry.hash;
        const timeDiff = existingEntry ? now - existingEntry.created_at : 0;

        // Skip if this would undelete a recently-deleted entry (within 30 seconds)
        if (existingWasDeleted && hashHex && timeDiff < 30) {
          // Blocked stale undelete
        } else {
          listSub.entriesByDTag.set(treeName, {
            hash: hashHex,
            visibility: visibilityType,
            key: visibilityType === 'public' ? chkKeyHex : undefined,
            encryptedKey: visibilityInfo.encryptedKey,
            keyId: visibilityInfo.keyId,
            selfEncryptedKey: visibilityInfo.selfEncryptedKey,
        selfEncryptedLinkKey: visibilityInfo.selfEncryptedLinkKey,
            created_at: now,
          });
          // Emit updated state immediately to ALL callbacks
          const result: RefResolverListEntry[] = [];
          for (const [dTag, entry] of listSub.entriesByDTag) {
            if (!entry.hash) continue; // Skip deleted trees
            result.push({
              key: `${npubStr}/${dTag}`,
              cid: cid(fromHex(entry.hash), entry.key ? fromHex(entry.key) : undefined),
              visibility: entry.visibility,
              encryptedKey: entry.encryptedKey,
              keyId: entry.keyId,
              selfEncryptedKey: entry.selfEncryptedKey,
              selfEncryptedLinkKey: entry.selfEncryptedLinkKey,
              createdAt: entry.created_at,
            });
          }
          for (const cb of listSub.callbacks) {
            try {
              cb(result);
            } catch (e) {
              console.error('List callback error:', e);
            }
          }
        }
      }

      // 2. Publish to network
      try {
        const success = await nostrPublish({
          kind: 30078,
          content: '',
          tags,
        });
        if (!success) {
          return { success: false, linkKey: resultLinkKey };
        }
      } catch (e) {
        console.error('Failed to publish to nostr:', e);
        return { success: false, linkKey: resultLinkKey };
      }

      return { success: true, linkKey: resultLinkKey };
    },

    /**
     * List all trees for a user.
     * Streams results as they arrive - returns unsubscribe function.
     * Caller decides when to stop listening.
     * Supports multiple subscribers per npub - all callbacks receive updates.
     */
    list(prefix: string, callback: (entries: RefResolverListEntry[]) => void): () => void {
      const parts = prefix.split('/');
      if (parts.length === 0) {
        callback([]);
        return () => {};
      }

      const npubStr = parts[0];
      let pubkey: string;

      try {
        const decoded = nip19.decode(npubStr);
        if (decoded.type !== 'npub') {
          callback([]);
          return () => {};
        }
        pubkey = decoded.data as string;
      } catch {
        callback([]);
        return () => {};
      }

      // Check if we already have a subscription for this npub
      const existingSub = listSubscriptions.get(npubStr);

      if (existingSub) {
        // Add callback to existing subscription
        existingSub.callbacks.add(callback);

        // Fire immediately with current state
        const result: RefResolverListEntry[] = [];
        for (const [dTag, entry] of existingSub.entriesByDTag) {
          if (!entry.hash) {
            continue;
          }
          result.push({
            key: `${npubStr}/${dTag}`,
            cid: cid(fromHex(entry.hash), entry.key ? fromHex(entry.key) : undefined),
            visibility: entry.visibility,
            encryptedKey: entry.encryptedKey,
            keyId: entry.keyId,
            selfEncryptedKey: entry.selfEncryptedKey,
            selfEncryptedLinkKey: entry.selfEncryptedLinkKey,
            createdAt: entry.created_at,
          });
        }
        callback(result);

        // Return unsubscribe that removes this callback
        return () => {
          existingSub.callbacks.delete(callback);
          // If no more callbacks, clean up the subscription
          if (existingSub.callbacks.size === 0) {
            existingSub.unsubscribe();
            listSubscriptions.delete(npubStr);
          }
        };
      }

      // Create new subscription
      const entriesByDTag = new Map<string, ParsedTreeVisibility & { created_at: number }>();
      const callbacks = new Set<(entries: RefResolverListEntry[]) => void>([callback]);

      // Pre-populate from local cache (for instant display of locally-created trees)
      const cachedEntries = localListCache.get(npubStr);
      if (cachedEntries) {
        for (const [treeName, entry] of cachedEntries) {
          entriesByDTag.set(treeName, entry);
        }
      }

      const emitCurrentState = () => {
        const result: RefResolverListEntry[] = [];
        for (const [dTag, entry] of entriesByDTag) {
          // Skip entries with empty/null hash (deleted trees)
          if (!entry.hash) continue;
          result.push({
            key: `${npubStr}/${dTag}`,
            cid: cid(fromHex(entry.hash), entry.key ? fromHex(entry.key) : undefined),
            visibility: entry.visibility,
            encryptedKey: entry.encryptedKey,
            keyId: entry.keyId,
            selfEncryptedKey: entry.selfEncryptedKey,
            selfEncryptedLinkKey: entry.selfEncryptedLinkKey,
            createdAt: entry.created_at,
          });
        }
        // Notify ALL callbacks
        for (const cb of callbacks) {
          try {
            cb(result);
          } catch (e) {
            console.error('List callback error:', e);
          }
        }
      };

      // Emit cached entries immediately if any
      if (entriesByDTag.size > 0) {
        emitCurrentState();
      }

      const unsubscribe = nostrSubscribe(
        {
          kinds: [30078],
          authors: [pubkey],
          '#l': ['hashtree'],
        },
        (event) => {
          const dTag = event.tags.find(t => t[0] === 'd')?.[1];
          if (!dTag) return;

          const parsed = parseHashAndVisibility(event);
          const hasHash = !!parsed?.hash;
          const existing = entriesByDTag.get(dTag);
          const eventTime = event.created_at || 0;

          // Timestamp-based update logic:
          // 1. Accept if no existing entry
          // 2. Accept if event is strictly newer
          // 3. Accept if same timestamp and this is a delete (no hash) overwriting a create (has hash)
          // 4. Reject if same timestamp and this would "undelete" (create overwriting delete)
          // 5. Reject if existing is deleted and new event is within 30s (prevent stale event undelete)

          if (existing) {
            const existingIsDeleted = !existing.hash;
            const timeDiff = eventTime - existing.created_at;

            // Block stale events from "undeleting" within 30 seconds
            if (existingIsDeleted && hasHash && timeDiff < 30) {
              return;
            }

            // Block same-timestamp undelete (create can't overwrite delete at same time)
            if (existingIsDeleted && hasHash && timeDiff === 0) {
              return;
            }

            // Only update if newer, or if same time and delete wins over create
            const shouldUpdate = eventTime > existing.created_at ||
              (eventTime === existing.created_at && !hasHash && existing.hash);

            if (!shouldUpdate) return;
          }

          // Update the entry
          const entryData = {
            hash: parsed?.hash ?? '',
            visibility: parsed?.visibility ?? 'public',
            key: parsed?.key,
            encryptedKey: parsed?.encryptedKey,
            keyId: parsed?.keyId,
            selfEncryptedKey: parsed?.selfEncryptedKey,
            selfEncryptedLinkKey: parsed?.selfEncryptedLinkKey,
            created_at: eventTime,
          };
          entriesByDTag.set(dTag, entryData);

          // Also update localListCache so subscribe() can find this data
          // This enables cross-function caching: list() receives data, subscribe() can use it
          let npubCache = localListCache.get(npubStr);
          if (!npubCache) {
            npubCache = new Map();
            localListCache.set(npubStr, npubCache);
          }
          npubCache.set(dTag, entryData);

          emitCurrentState();
        }
      );

      // Register this list subscription
      listSubscriptions.set(npubStr, {
        npub: npubStr,
        entriesByDTag,
        callbacks,
        unsubscribe,
      });

      return () => {
        callbacks.delete(callback);
        if (callbacks.size === 0) {
          unsubscribe();
          listSubscriptions.delete(npubStr);
        }
      };
    },

    /**
     * Stop all subscriptions
     */
    stop(): void {
      for (const [, sub] of subscriptions) {
        sub.unsubscribe();
      }
      subscriptions.clear();
      listSubscriptions.clear();
    },

    /**
     * Delete a tree by publishing event without hash tag
     * This nullifies the tree - it will be filtered from list results
     */
    async delete(key: string): Promise<boolean> {
      const parsed = parseKey(key);
      if (!parsed) return false;

      const { treeName } = parsed;
      const pubkey = getPubkey();
      if (!pubkey) return false;

      const now = Math.floor(Date.now() / 1000);
      const npubStr = key.split('/')[0];

      // Update local list cache with empty hash (marks as deleted)
      let npubCache = localListCache.get(npubStr);
      if (!npubCache) {
        npubCache = new Map();
        localListCache.set(npubStr, npubCache);
      }
      npubCache.set(treeName, {
        hash: '', // Empty hash marks as deleted
        visibility: 'public',
        key: undefined,
        created_at: now,
      });

      // Update active subscription state
      const sub = subscriptions.get(key);
      if (sub) {
        sub.currentHash = null;
        sub.currentKey = null;
        sub.latestCreatedAt = now;
        sub.currentVisibility = null;
        // Notify callbacks with null CID
        for (const cb of sub.callbacks) {
          try {
            cb(null);
          } catch (e) {
            console.error('Resolver callback error:', e);
          }
        }
      }

      // Update active list subscriptions - set hash to empty and emit
      const listSub = listSubscriptions.get(npubStr);
      if (listSub) {
        listSub.entriesByDTag.set(treeName, {
          hash: '', // Empty hash marks as deleted
          visibility: 'public',
          key: undefined,
          created_at: now,
        });
        // Emit - filter out empty hashes (deleted trees)
        const result: RefResolverListEntry[] = [];
        for (const [dTag, entry] of listSub.entriesByDTag) {
          if (!entry.hash) continue;
          result.push({
            key: `${npubStr}/${dTag}`,
            cid: cid(fromHex(entry.hash), entry.key ? fromHex(entry.key) : undefined),
            visibility: entry.visibility,
            encryptedKey: entry.encryptedKey,
            keyId: entry.keyId,
            selfEncryptedKey: entry.selfEncryptedKey,
            selfEncryptedLinkKey: entry.selfEncryptedLinkKey,
            createdAt: entry.created_at,
          });
        }
        for (const cb of listSub.callbacks) {
          try {
            cb(result);
          } catch (e) {
            console.error('List callback error:', e);
          }
        }
      }

      // 2. Publish to Nostr - event without hash tag
      // Use now + 1 to ensure delete timestamp is strictly higher than any create event
      // This is critical for NIP-33: when timestamps are equal, event ID breaks the tie (random)
      nostrPublish({
        kind: 30078,
        content: '',
        tags: [
          ['d', treeName],
          ['l', 'hashtree'],
          // No hash tag = deleted
        ],
        created_at: now + 1,
      }).catch(e => console.error('Failed to publish delete to nostr:', e));

      return true;
    },

    /**
     * Inject a local list entry (for instant UI updates)
     * This updates the local cache to make trees appear immediately.
     */
    injectListEntry(entry: RefResolverListEntry): void {
      const parts = entry.key.split('/');
      if (parts.length !== 2) return;
      const [npubStr, treeName] = parts;

      const now = Math.floor(Date.now() / 1000);
      const hasHash = !!toHex(entry.cid.hash);

      // Update the local list cache
      let npubCache = localListCache.get(npubStr);
      if (!npubCache) {
        npubCache = new Map();
        localListCache.set(npubStr, npubCache);
      }

      const existing = npubCache.get(treeName);
      const existingWasDeletedCache = existing && !existing.hash;
      const timeDiffCache = existing ? now - existing.created_at : 0;

      // Skip if this would undelete a recently-deleted entry (within 30 seconds)
      if (existingWasDeletedCache && hasHash && timeDiffCache < 30) {
        return;
      }

      if (!existing || now >= existing.created_at) {
        npubCache.set(treeName, {
          hash: toHex(entry.cid.hash),
          visibility: entry.visibility ?? 'public',
          key: entry.cid.key ? toHex(entry.cid.key) : undefined,
          encryptedKey: entry.encryptedKey,
          keyId: entry.keyId,
          selfEncryptedKey: entry.selfEncryptedKey,
            selfEncryptedLinkKey: entry.selfEncryptedLinkKey,
          created_at: now,
        });
      }

      // If there's an active list subscription for this npub, update it too
      const listSub = listSubscriptions.get(npubStr);
      if (listSub) {
        const existingSub = listSub.entriesByDTag.get(treeName);
        const existingWasDeleted = existingSub && !existingSub.hash;
        const timeDiff = existingSub ? now - existingSub.created_at : 0;

        // Skip if this would undelete a recently-deleted entry (within 30 seconds)
        if (existingWasDeleted && hasHash && timeDiff < 30) {
          return;
        }

        if (!existingSub || now >= existingSub.created_at) {
          listSub.entriesByDTag.set(treeName, {
            hash: toHex(entry.cid.hash),
            visibility: entry.visibility ?? 'public',
            key: entry.cid.key ? toHex(entry.cid.key) : undefined,
            encryptedKey: entry.encryptedKey,
            keyId: entry.keyId,
            selfEncryptedKey: entry.selfEncryptedKey,
            selfEncryptedLinkKey: entry.selfEncryptedLinkKey,
            created_at: now,
          });
          // Emit updated state to ALL callbacks
          const result: RefResolverListEntry[] = [];
          for (const [dTag, e] of listSub.entriesByDTag) {
            if (!e.hash) continue; // Skip deleted trees
            result.push({
              key: `${npubStr}/${dTag}`,
              cid: cid(fromHex(e.hash), e.key ? fromHex(e.key) : undefined),
              visibility: e.visibility,
              encryptedKey: e.encryptedKey,
              keyId: e.keyId,
              selfEncryptedKey: e.selfEncryptedKey,
              selfEncryptedLinkKey: e.selfEncryptedLinkKey,
              createdAt: e.created_at,
            });
          }
          for (const cb of listSub.callbacks) {
            try {
              cb(result);
            } catch (err) {
              console.error('List callback error:', err);
            }
          }
        }
      }
    },
  };
}
