/**
 * Tree root store for Svelte
 *
 * This provides the rootCid from the URL via resolver subscription:
 * - For tree routes (/npub/treeName/...), subscribes to the resolver
 * - For permalink routes (/nhash1.../...), extracts hash directly from URL
 * - Returns null when no tree context
 *
 * Data flow:
 * - Local writes -> TreeRootRegistry (via treeRootCache.ts)
 * - Resolver events -> TreeRootRegistry (via setFromResolver)
 * - UI/SW reads -> TreeRootRegistry (via get/resolve)
 */
import { writable, get, type Readable } from 'svelte/store';
import { fromHex, toHex, cid, visibilityHex } from 'hashtree';
import type { CID, SubscribeVisibilityInfo, Hash } from 'hashtree';
import { routeStore, parseRouteFromHash } from './route';
import { getRefResolver, getResolverKey } from '../refResolver';
import { nostrStore, decrypt } from '../nostr';
import { logHtreeDebug } from '../lib/htreeDebug';
import { isTauri } from '../tauri';
import { treeRootRegistry } from '../TreeRootRegistry';

// Wait for worker to be ready before creating subscriptions
// This ensures the NDK transport plugin is registered
let workerReadyPromise: Promise<void> | null = null;
let workerReadyResolve: (() => void) | null = null;
const WORKER_READY_TIMEOUT_MS = 10000;

/**
 * Signal that the worker is ready (called from auth.ts after initHashtreeWorker)
 */
export function signalWorkerReady(): void {
  if (workerReadyResolve) {
    workerReadyResolve();
    workerReadyResolve = null;
  }
  logHtreeDebug('worker:ready');
}

/**
 * Wait for the worker to be ready
 */
function waitForWorkerReady(): Promise<void> {
  if (!workerReadyPromise) {
    workerReadyPromise = new Promise((resolve) => {
      // Check if worker is already ready (import dynamically to avoid circular deps)
      import('../lib/workerInit').then(({ isWorkerReady }) => {
        if (isWorkerReady()) {
          resolve();
        } else {
          workerReadyResolve = resolve;
        }
      });
    });
  }
  return workerReadyPromise;
}

// Subscription state - manages resolver subscriptions and listeners
// The actual data is stored in TreeRootRegistry
const subscriptionState = new Map<string, {
  decryptedKey: Hash | undefined;
  listeners: Set<(hash: Hash | null, encryptionKey?: Hash, visibilityInfo?: SubscribeVisibilityInfo) => void>;
  unsubscribe: (() => void) | null;
}>();

/**
 * Build SubscribeVisibilityInfo from registry record
 */
function getVisibilityInfoFromRegistry(key: string): SubscribeVisibilityInfo | undefined {
  const record = treeRootRegistry.getByKey(key);
  if (!record) return undefined;
  return {
    visibility: record.visibility,
    encryptedKey: record.encryptedKey,
    keyId: record.keyId,
    selfEncryptedKey: record.selfEncryptedKey,
    selfEncryptedLinkKey: record.selfEncryptedLinkKey,
  };
}

const tauriRootCache = new Map<string, string>();

async function cacheTreeRootInTauri(key: string, hash: Hash, encryptionKey?: Hash): Promise<void> {
  if (!isTauri()) return;
  const slashIndex = key.indexOf('/');
  if (slashIndex <= 0 || slashIndex === key.length - 1) return;

  const npub = key.slice(0, slashIndex);
  const treeName = key.slice(slashIndex + 1);
  const hashHex = toHex(hash);
  const keyHex = encryptionKey ? toHex(encryptionKey) : '';

  // Get visibility from registry
  const record = treeRootRegistry.getByKey(key);
  const visibility = record?.visibility ?? 'public';

  const cacheSignature = `${hashHex}:${keyHex}:${visibility}`;
  if (tauriRootCache.get(key) === cacheSignature) return;
  tauriRootCache.set(key, cacheSignature);

  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('cache_tree_root', {
      npub,
      treeName,
      hash: hashHex,
      key: keyHex || null,
      visibility,
    });
  } catch (err) {
    console.warn('[treeRoot] Failed to cache tree root in Tauri:', err);
  }
}

/**
 * Update the subscription cache directly (called from treeRootCache on local writes)
 * @deprecated This is now handled by TreeRootRegistry - kept for backward compatibility
 */
export function updateSubscriptionCache(key: string, hash: Hash, encryptionKey?: Hash): void {
  // Note: The registry is already updated by treeRootCache.ts via setLocal()
  // This function now just notifies listeners and updates Tauri cache

  let state = subscriptionState.get(key);
  if (!state) {
    // Create entry if it doesn't exist (for newly created trees)
    state = {
      decryptedKey: undefined,
      listeners: new Set(),
      unsubscribe: null,
    };
    subscriptionState.set(key, state);
  }
  state.decryptedKey = encryptionKey;
  const visibilityInfo = getVisibilityInfoFromRegistry(key);
  state.listeners.forEach(listener => listener(hash, encryptionKey, visibilityInfo));
  void cacheTreeRootInTauri(key, hash, encryptionKey);
}

// Subscribe to registry updates to bridge to Tauri and listeners
treeRootRegistry.subscribeAll((key, record) => {
  if (!record) return;
  const state = subscriptionState.get(key);
  if (state) {
    const visibilityInfo = getVisibilityInfoFromRegistry(key);
    state.listeners.forEach(listener => listener(record.hash, record.key, visibilityInfo));
  }
  void cacheTreeRootInTauri(key, record.hash, record.key);
});

/**
 * Subscribe to tree root updates for a specific npub/treeName
 * Returns an unsubscribe function
 */
export function subscribeToTreeRoot(
  npub: string,
  treeName: string,
  callback: (hash: Hash | null, encryptionKey?: Hash) => void
): () => void {
  const key = `${npub}/${treeName}`;
  return subscribeToResolver(key, callback);
}

/**
 * Start the resolver subscription after worker is ready
 * This is called asynchronously to ensure NDK transport plugin is registered
 */
async function startResolverSubscription(
  key: string,
  options?: { force?: boolean }
): Promise<void> {
  const workerReady = await Promise.race([
    waitForWorkerReady().then(() => true),
    new Promise<boolean>((resolve) => setTimeout(() => resolve(false), WORKER_READY_TIMEOUT_MS)),
  ]);
  if (!workerReady) {
    console.warn('[treeRoot] Worker not ready yet - subscribing anyway');
  }

  const state = subscriptionState.get(key);
  if (!state) return; // Entry was deleted before worker was ready

  // Don't create subscription if one already exists unless forced
  if (state.unsubscribe) {
    if (!options?.force) return;
    state.unsubscribe();
    state.unsubscribe = null;
  }

  const resolver = getRefResolver();
  state.unsubscribe = resolver.subscribe(key, (resolvedCid, visibilityInfo) => {
    console.log('[treeRoot] Resolver callback for', key, {
      hasHash: !!resolvedCid?.hash,
      visibilityInfo: visibilityInfo ? JSON.stringify(visibilityInfo) : 'undefined'
    });
    const entry = subscriptionState.get(key);
    if (entry) {
      // Update registry with resolver data (only if newer)
      if (resolvedCid?.hash) {
        const slashIndex = key.indexOf('/');
        const npub = key.slice(0, slashIndex);
        const treeName = key.slice(slashIndex + 1);
        // Use current time as updatedAt - the resolver doesn't provide created_at in subscribe callback
        const updatedAt = Math.floor(Date.now() / 1000);

        treeRootRegistry.setFromResolver(npub, treeName, resolvedCid.hash, updatedAt, {
          key: resolvedCid.key,
          visibility: visibilityInfo?.visibility ?? 'public',
          encryptedKey: visibilityInfo?.encryptedKey,
          keyId: visibilityInfo?.keyId,
          selfEncryptedKey: visibilityInfo?.selfEncryptedKey,
          selfEncryptedLinkKey: visibilityInfo?.selfEncryptedLinkKey,
        });
      }

      entry.listeners.forEach(listener => listener(resolvedCid?.hash ?? null, resolvedCid?.key, visibilityInfo));
    }
  });
}

function subscribeToResolver(
  key: string,
  callback: (hash: Hash | null, encryptionKey?: Hash, visibilityInfo?: SubscribeVisibilityInfo) => void
): () => void {
  let state = subscriptionState.get(key);

  if (!state) {
    state = {
      decryptedKey: undefined,
      listeners: new Set(),
      unsubscribe: null,
    };
    subscriptionState.set(key, state);

    // Start the subscription asynchronously after worker is ready
    // This ensures the NDK transport plugin is registered before subscribing
    startResolverSubscription(key);
  }

  state.listeners.add(callback);

  // Emit current snapshot from registry if available
  const record = treeRootRegistry.getByKey(key);
  if (record) {
    const visibilityInfo = getVisibilityInfoFromRegistry(key);
    console.log('[treeRoot] Immediate callback from registry for', key, {
      visibilityInfo: visibilityInfo ? JSON.stringify(visibilityInfo) : 'undefined'
    });
    queueMicrotask(() => callback(record.hash, record.key, visibilityInfo));
  }

  return () => {
    const cached = subscriptionState.get(key);
    if (cached) {
      cached.listeners.delete(callback);
      // Note: We don't delete the cache entry when the last listener unsubscribes
      // because the data is still valid and may be needed by other components
      // (e.g., DocCard uses getTreeRootSync after the editor unmounts)
      if (cached.listeners.size === 0) {
        cached.unsubscribe?.();
        // Keep the cached data, just stop the subscription
        // subscriptionState.delete(key);
      }
    }
  };
}

function refreshResolverSubscription(key: string): void {
  if (!subscriptionState.has(key)) return;
  startResolverSubscription(key, { force: true });
}

/**
 * Decrypt the encryption key for a tree based on visibility and available keys
 */
async function decryptEncryptionKey(
  visibilityInfo: SubscribeVisibilityInfo | undefined,
  encryptionKey: Hash | undefined,
  linkKey: string | null
): Promise<Hash | undefined> {
  if (encryptionKey) {
    return encryptionKey;
  }

  if (!visibilityInfo) {
    // Fallback: if linkKey is present but no visibility info, use linkKey directly
    if (linkKey && linkKey.length === 64) {
      try {
        return fromHex(linkKey);
      } catch (e) {
        console.debug('Could not use linkKey directly:', e);
      }
    }
    return undefined;
  }

  // Link-visible tree with linkKey from URL
  if (visibilityInfo.visibility === 'link-visible' && linkKey) {
    console.log('[decryptEncryptionKey] Link-visible with k= param:', {
      hasEncryptedKey: !!visibilityInfo.encryptedKey,
      encryptedKeyPrefix: visibilityInfo.encryptedKey?.slice(0, 16),
      linkKeyPrefix: linkKey.slice(0, 16),
    });

    if (visibilityInfo.encryptedKey) {
      try {
        const decryptedHex = await visibilityHex.decryptKeyFromLink(visibilityInfo.encryptedKey, linkKey);
        console.log('[decryptEncryptionKey] XOR decrypt result:', {
          success: !!decryptedHex,
          resultPrefix: decryptedHex?.slice(0, 16),
        });
        if (decryptedHex) {
          return fromHex(decryptedHex);
        }
        console.warn('[decryptEncryptionKey] Key mismatch - linkKey does not decrypt encryptedKey');
      } catch (e) {
        console.error('[decryptEncryptionKey] Decryption failed:', e);
      }
    } else {
      console.warn('[decryptEncryptionKey] Link-visible but no encryptedKey in visibilityInfo!');
    }
  }

  // Link-visible tree - owner access via selfEncryptedLinkKey
  // Decrypt linkKey, then derive contentKey from encryptedKey
  if (visibilityInfo.visibility === 'link-visible' && visibilityInfo.encryptedKey && visibilityInfo.selfEncryptedLinkKey) {
    try {
      const state = get(nostrStore);
      if (state.pubkey) {
        const decryptedLinkKey = await decrypt(state.pubkey, visibilityInfo.selfEncryptedLinkKey);
        if (decryptedLinkKey && decryptedLinkKey.length === 64) {
          const decryptedHex = await visibilityHex.decryptKeyFromLink(visibilityInfo.encryptedKey, decryptedLinkKey);
          if (decryptedHex) {
            return fromHex(decryptedHex);
          }
        }
      }
    } catch (e) {
      console.debug('Could not decrypt via selfEncryptedLinkKey (not owner?):', e);
    }
  }

  // Private tree - try selfEncryptedKey (owner access)
  if (visibilityInfo.selfEncryptedKey) {
    try {
      const state = get(nostrStore);
      if (state.pubkey) {
        // Use centralized decrypt (works with both nsec and extension login)
        const decrypted = await decrypt(state.pubkey, visibilityInfo.selfEncryptedKey);
        return fromHex(decrypted);
      }
    } catch (e) {
      console.debug('Could not decrypt selfEncryptedKey (not owner?):', e);
    }
  }

  // Fallback: if linkKey is present but couldn't be used via encryptedKey,
  // try using it directly (for legacy content or when visibility info is incomplete)
  if (linkKey && linkKey.length === 64) {
    try {
      return fromHex(linkKey);
    } catch (e) {
      console.debug('Could not use linkKey directly:', e);
    }
  }

  return undefined;
}

// Store for tree root
export const treeRootStore = writable<CID | null>(null);

/**
 * Recover linkKey for URL when owner navigates to link-visible without k= param
 * This allows easy sharing by copying URL from address bar
 */
async function recoverLinkKeyForUrl(resolverKey: string): Promise<void> {
  const npubStr = resolverKey.split('/')[0];
  const treeName = resolverKey.split('/').slice(1).join('/');
  const resolver = getRefResolver();

  // Use list to get fresh visibility data
  const entries = await new Promise<{ visibility?: string; selfEncryptedLinkKey?: string }[] | null>((resolve) => {
    let resolved = false;

    const unsub = resolver.list?.(npubStr, (list) => {
      if (resolved) return;
      resolved = true;
      // Defer unsubscribe to avoid calling it during callback
      setTimeout(() => unsub?.(), 0);
      // list entries have 'key' field like 'npub/treeName', we need to match by treeName
      const entry = list.find(e => {
        const keyParts = e.key?.split('/');
        const entryTreeName = keyParts?.slice(1).join('/');
        return entryTreeName === treeName;
      });
      resolve(entry ? [entry as { visibility?: string; selfEncryptedLinkKey?: string }] : null);
    });

    // Timeout after 2 seconds
    setTimeout(() => {
      if (!resolved) {
        resolved = true;
        unsub?.();
        resolve(null);
      }
    }, 2000);
  });

  if (!entries?.[0]) return;

  const { visibility, selfEncryptedLinkKey } = entries[0];
  if (visibility !== 'link-visible' || !selfEncryptedLinkKey) return;

  try {
    const state = get(nostrStore);
    const { nip19: nip19Mod } = await import('nostr-tools');
    const decoded = nip19Mod.decode(npubStr);
    const treePubkey = decoded.type === 'npub' ? decoded.data as string : null;

    if (state.pubkey && treePubkey && state.pubkey === treePubkey) {
      const decryptedLinkKey = await decrypt(state.pubkey, selfEncryptedLinkKey);
      if (decryptedLinkKey && decryptedLinkKey.length === 64) {
        // Update URL with k= param (use replaceState to avoid history pollution)
        const currentHash = window.location.hash;
        if (!currentHash.includes('k=')) {
          const separator = currentHash.includes('?') ? '&' : '?';
          window.history.replaceState(null, '', currentHash + separator + 'k=' + decryptedLinkKey);
        }
      }
    }
  } catch (e) {
    console.debug('[treeRoot] Could not recover linkKey for URL:', e);
  }
}

// Active subscription cleanup
let activeUnsubscribe: (() => void) | null = null;
let activeResolverKey: string | null = null;
let resolverRetryTimer: ReturnType<typeof setTimeout> | null = null;
let resolverRetryAttempts = 0;
const RESOLVER_RETRY_DELAY_MS = 2000;
const RESOLVER_RETRY_MAX_ATTEMPTS = 5;

function resetResolverRetry(): void {
  if (resolverRetryTimer) {
    clearTimeout(resolverRetryTimer);
    resolverRetryTimer = null;
  }
  resolverRetryAttempts = 0;
}

function scheduleResolverRetry(resolverKey: string): void {
  if (resolverRetryTimer) return;
  if (get(nostrStore).connectedRelays === 0) return;

  resolverRetryTimer = setTimeout(() => {
    resolverRetryTimer = null;
    if (resolverKey !== activeResolverKey) return;

    // Check registry instead of subscriptionCache
    const record = treeRootRegistry.getByKey(resolverKey);
    if (record?.hash) {
      resetResolverRetry();
      return;
    }

    resolverRetryAttempts += 1;
    refreshResolverSubscription(resolverKey);

    if (resolverRetryAttempts < RESOLVER_RETRY_MAX_ATTEMPTS) {
      scheduleResolverRetry(resolverKey);
    }
  }, RESOLVER_RETRY_DELAY_MS);
}

/**
 * Create a tree root store that reacts to route changes
 */
export function createTreeRootStore(): Readable<CID | null> {
  // Subscribe to route changes
  routeStore.subscribe(async (route) => {
    logHtreeDebug('treeRoot:route', {
      npub: route.npub,
      treeName: route.treeName,
      isPermalink: route.isPermalink,
      path: route.path?.join('/') ?? '',
      hasCid: !!route.cid,
    });
    // For permalinks, use CID from route (already Uint8Array from nhashDecode)
    if (route.isPermalink && route.cid) {
      treeRootStore.set(route.cid);
      logHtreeDebug('treeRoot:set', { source: 'permalink' });

      // Cleanup any active subscription
      if (activeUnsubscribe) {
        activeUnsubscribe();
        activeUnsubscribe = null;
        activeResolverKey = null;
      }
      resetResolverRetry();
      return;
    }

    // For tree routes, subscribe to resolver
    const resolverKey = getResolverKey(route.npub ?? undefined, route.treeName ?? undefined);
    if (!resolverKey) {
      treeRootStore.set(null);
      logHtreeDebug('treeRoot:clear', { reason: 'no-resolver-key' });
      if (activeUnsubscribe) {
        activeUnsubscribe();
        activeUnsubscribe = null;
        activeResolverKey = null;
      }
      resetResolverRetry();
      return;
    }

    // Same key, no need to resubscribe
    // But still check if we need to recover k= param for URL
    if (resolverKey === activeResolverKey) {
      const currentRoute = get(routeStore);
      const linkKeyFromUrl = currentRoute.params.get('k');
      if (!linkKeyFromUrl) {
        recoverLinkKeyForUrl(resolverKey);
      }
      logHtreeDebug('treeRoot:reuse', { resolverKey });
      return;
    }

    // Cleanup previous subscription
    if (activeUnsubscribe) {
      activeUnsubscribe();
    }

    // Reset while waiting for new data
    treeRootStore.set(null);
    activeResolverKey = resolverKey;
    resetResolverRetry();
    logHtreeDebug('treeRoot:subscribe', { resolverKey });
    logHtreeDebug('treeRoot:subscribe', { resolverKey });

    // Subscribe to resolver
    activeUnsubscribe = subscribeToResolver(resolverKey, async (hash, encryptionKey, visibilityInfo) => {
      if (!hash) {
        treeRootStore.set(null);
        logHtreeDebug('treeRoot:clear', { reason: 'no-hash', resolverKey });
        return;
      }

      console.log('[treeRoot] Resolver callback:', {
        hasHash: !!hash,
        hasEncryptionKey: !!encryptionKey,
        visibility: visibilityInfo?.visibility,
        hasEncryptedKey: !!visibilityInfo?.encryptedKey,
      });
      logHtreeDebug('treeRoot:resolver', {
        resolverKey,
        hasHash: !!hash,
        hasEncryptionKey: !!encryptionKey,
        visibility: visibilityInfo?.visibility ?? null,
        hasEncryptedKey: !!visibilityInfo?.encryptedKey,
      });

      // Get current route params (not the closure-captured route from subscription time)
      const currentRoute = get(routeStore);
      const linkKeyFromUrl = currentRoute.params.get('k');
      const decryptedKey = await decryptEncryptionKey(visibilityInfo, encryptionKey, linkKeyFromUrl);

      // Cache the decrypted key
      if (decryptedKey) {
        const state = subscriptionState.get(resolverKey);
        if (state) {
          state.decryptedKey = decryptedKey;
        }
      }

      resetResolverRetry();

      // If owner viewing link-visible without k= param, recover linkKey and update URL
      // This allows owner to share the URL easily (copy from address bar includes k=)
      if (!linkKeyFromUrl) {
        // Try to get visibility info - check visibilityInfo first, then fall back to resolver list
        let selfEncryptedLinkKey = visibilityInfo?.selfEncryptedLinkKey;
        let visibility = visibilityInfo?.visibility;

        // If we don't have selfEncryptedLinkKey from the callback, try to get it from resolver list
        if (!selfEncryptedLinkKey) {
          const npubStr = resolverKey.split('/')[0];
          const treeName = resolverKey.split('/').slice(1).join('/');
          const resolver = getRefResolver();

          // Use list to get fresh visibility data
          const entries = await new Promise<{ visibility?: string; selfEncryptedLinkKey?: string }[] | null>((resolve) => {
            let resolved = false;

            const unsub = resolver.list?.(npubStr, (list) => {
              if (resolved) return;
              resolved = true;
              // Defer unsubscribe to avoid calling it during callback
              setTimeout(() => unsub?.(), 0);
              // list entries have 'key' field like 'npub/treeName', we need to match by treeName
              const entry = list.find(e => {
                const keyParts = e.key?.split('/');
                const entryTreeName = keyParts?.slice(1).join('/');
                return entryTreeName === treeName;
              });
              resolve(entry ? [entry as { visibility?: string; selfEncryptedLinkKey?: string }] : null);
            });

            // Timeout after 2 seconds
            setTimeout(() => {
              if (!resolved) {
                resolved = true;
                unsub?.();
                resolve(null);
              }
            }, 2000);
          });

          if (entries && entries[0]) {
            selfEncryptedLinkKey = entries[0].selfEncryptedLinkKey;
            visibility = entries[0].visibility as typeof visibility;
          }
        }

        if (visibility === 'link-visible') {
          const state = get(nostrStore);
          const npubStr = resolverKey.split('/')[0];
          const { nip19: nip19Mod } = await import('nostr-tools');
          const decoded = nip19Mod.decode(npubStr);
          const treePubkey = decoded.type === 'npub' ? decoded.data as string : null;
          const isOwner = state.pubkey && treePubkey && state.pubkey === treePubkey;

          if (isOwner) {
            if (selfEncryptedLinkKey) {
              // Decrypt linkKey and update URL
              try {
                const linkKeyHex = await decrypt(state.pubkey!, selfEncryptedLinkKey);
                if (linkKeyHex && linkKeyHex.length === 64) {
                  const currentHash = window.location.hash;
                  // Only add k= if not already present
                  if (!currentHash.includes('k=')) {
                    const separator = currentHash.includes('?') ? '&' : '?';
                    window.history.replaceState(null, '', currentHash + separator + 'k=' + linkKeyHex);
                  }
                }
              } catch (e) {
                console.error('[treeRoot] Could not decrypt linkKey:', e);
              }
            } else {
              // Migration: old event without selfEncryptedLinkKey
              // Try to derive linkKey from contentKey and encryptedKey (XOR)
              const treeName = resolverKey.split('/').slice(1).join('/');
              const { toHex, visibilityHex } = await import('hashtree');

              // Get encryptedKey from visibilityInfo or list
              let encryptedKeyHex = visibilityInfo?.encryptedKey;
              if (!encryptedKeyHex && entries?.[0]) {
                encryptedKeyHex = (entries[0] as { encryptedKey?: string }).encryptedKey;
              }

              // Get selfEncryptedKey for decrypting contentKey
              let selfEncryptedKey = visibilityInfo?.selfEncryptedKey;
              if (!selfEncryptedKey && entries?.[0]) {
                selfEncryptedKey = (entries[0] as { selfEncryptedKey?: string }).selfEncryptedKey;
              }

              console.log('[treeRoot] Migration check:', {
                hasSelfEncryptedKey: !!selfEncryptedKey,
                hasEncryptedKey: !!encryptedKeyHex,
                visibility,
              });

              if (encryptedKeyHex && selfEncryptedKey) {
                try {
                  // Decrypt contentKey from selfEncryptedKey
                  const contentKeyHex = await decrypt(state.pubkey!, selfEncryptedKey);
                  console.log('[treeRoot] Decrypted selfEncryptedKey:', {
                    contentKeyHex: contentKeyHex?.slice(0, 16) + '...',
                    length: contentKeyHex?.length,
                  });
                  if (contentKeyHex && contentKeyHex.length === 64) {
                    // Derive linkKey = XOR(encryptedKey, contentKey)
                    const linkKeyHex = visibilityHex.encryptKeyForLink(contentKeyHex, encryptedKeyHex);

                    console.log('[treeRoot] Computed linkKey from selfEncryptedKey:', {
                      linkKeyHex: linkKeyHex.slice(0, 16) + '...',
                    });

                    const currentHash = window.location.hash;
                    if (!currentHash.includes('k=')) {
                      const separator = currentHash.includes('?') ? '&' : '?';
                      window.history.replaceState(null, '', currentHash + separator + 'k=' + linkKeyHex);
                    }

                    // Optionally republish with selfEncryptedLinkKey for future URL recovery
                    try {
                      const resolver = getRefResolver();
                      const { fromHex } = await import('hashtree');
                      await resolver.publish(treeName, hash, {
                        visibility: 'link-visible',
                        key: fromHex(contentKeyHex),
                        linkKey: fromHex(linkKeyHex),
                      });
                    } catch (e) {
                      console.debug('[treeRoot] Migration republish failed:', e);
                    }
                  }
                } catch (e) {
                  console.debug('[treeRoot] Could not derive linkKey from selfEncryptedKey:', e);
                }
              } else if (encryptedKeyHex) {
                // Fallback: try to get contentKey from local cache or decryptedKey
                const npubStr = resolverKey.split('/')[0];
                const { getLocalRootKey } = await import('../treeRootCache');
                const cachedKey = getLocalRootKey(npubStr, treeName);
                const contentKey = decryptedKey || cachedKey;

                console.log('[treeRoot] Migration fallback:', {
                  hasDecryptedKey: !!decryptedKey,
                  hasCachedKey: !!cachedKey,
                  hasContentKey: !!contentKey,
                  encryptedKeyHex: encryptedKeyHex?.slice(0, 16) + '...',
                });

                if (contentKey) {
                  try {
                    const contentKeyHex = toHex(contentKey);
                    const linkKeyHex = visibilityHex.encryptKeyForLink(contentKeyHex, encryptedKeyHex);

                    console.log('[treeRoot] Migration computed linkKey:', {
                      contentKeyHex: contentKeyHex.slice(0, 16) + '...',
                      linkKeyHex: linkKeyHex.slice(0, 16) + '...',
                    });

                    const currentHash = window.location.hash;
                    if (!currentHash.includes('k=')) {
                      const separator = currentHash.includes('?') ? '&' : '?';
                      window.history.replaceState(null, '', currentHash + separator + 'k=' + linkKeyHex);
                    }

                    // Republish with new linkKey and selfEncryptedLinkKey (fire and forget)
                    const resolver = getRefResolver();
                    resolver.publish(treeName, hash, {
                      visibility: 'link-visible',
                      key: contentKey,
                    }).catch(e => console.debug('[treeRoot] Migration republish failed:', e));
                  } catch (e) {
                    console.debug('[treeRoot] Could not derive linkKey from contentKey:', e);
                  }
                } else {
                  console.log('[treeRoot] Migration: no contentKey available, cannot derive linkKey');
                }
              }
            }
          }
        }
      }

      // For link-visible content, don't set store until we have the decryption key
      // This prevents the video player from trying to load before decryption is possible
      const visibility = visibilityInfo?.visibility;

      // If we have k= param but no visibilityInfo yet, wait for resolver to fetch the event
      // (we need encryptedKey from event to XOR with linkKey)
      // BUT: if we already have encryptionKey from local cache (owner just created tree),
      // we can proceed without waiting for visibilityInfo
      if (linkKeyFromUrl && !visibilityInfo?.encryptedKey && !encryptionKey) {
        console.log('[treeRoot] Have k= param but no encryptedKey yet, waiting for resolver...');
        return;
      }

      if (visibility === 'link-visible' && !decryptedKey) {
        console.log('[treeRoot] Link-visible but no decryptedKey yet, waiting...');
        // Don't set the store - wait for next callback with key
        return;
      }

      treeRootStore.set(cid(hash, decryptedKey));
      void cacheTreeRootInTauri(resolverKey, hash, decryptedKey);
      logHtreeDebug('treeRoot:set', {
        resolverKey,
        visibility: visibility ?? null,
        hasDecryptedKey: !!decryptedKey,
      });
    });

    scheduleResolverRetry(resolverKey);
  });

  let lastConnectedRelays = get(nostrStore).connectedRelays;
  nostrStore.subscribe((state) => {
    const connected = state.connectedRelays;
    if (connected > 0 && lastConnectedRelays === 0 && activeResolverKey) {
      const record = treeRootRegistry.getByKey(activeResolverKey);
      if (!record?.hash) {
        refreshResolverSubscription(activeResolverKey);
        scheduleResolverRetry(activeResolverKey);
      }
    }
    lastConnectedRelays = connected;
  });

  return treeRootStore;
}

/**
 * Get the current root CID synchronously
 */
export function getTreeRootSync(npub: string | null | undefined, treeName: string | null | undefined): CID | null {
  const key = getResolverKey(npub ?? undefined, treeName ?? undefined);
  if (!key) return null;

  // Check registry first
  const record = treeRootRegistry.getByKey(key);
  if (record?.hash) {
    return cid(record.hash, record.key);
  }

  // Fallback to subscription state for decrypted key
  const state = subscriptionState.get(key);
  if (state?.decryptedKey && record?.hash) {
    return cid(record.hash, state.decryptedKey);
  }

  return null;
}

/**
 * Wait for tree root to be resolved (async version of getTreeRootSync)
 * Subscribes to the resolver and waits for the first non-null result or timeout
 */
export function waitForTreeRoot(
  npub: string,
  treeName: string,
  timeoutMs: number = 10000
): Promise<CID | null> {
  return new Promise((resolve) => {
    // Check registry first
    const record = treeRootRegistry.get(npub, treeName);
    if (record) {
      resolve(cid(record.hash, record.key));
      return;
    }

    let resolved = false;
    let unsub: (() => void) | null = null;

    const timeout = setTimeout(() => {
      if (!resolved) {
        resolved = true;
        unsub?.();
        resolve(null);
      }
    }, timeoutMs);

    unsub = subscribeToTreeRoot(npub, treeName, (hash, encryptionKey) => {
      if (!resolved && hash) {
        resolved = true;
        clearTimeout(timeout);
        unsub?.();
        resolve(cid(hash, encryptionKey));
      }
    });
  });
}

/**
 * Invalidate and refresh the cached root CID
 */
export function invalidateTreeRoot(npub: string | null | undefined, treeName: string | null | undefined): void {
  const key = getResolverKey(npub ?? undefined, treeName ?? undefined);
  if (!key) return;
  // The resolver subscription will automatically pick up the new value
}

// Synchronously parse initial permalink (no resolver needed for nhash URLs)
// This must run BEFORE currentDirHash.ts subscribes to avoid race condition
function initializePermalink(): void {
  if (typeof window === 'undefined') return;

  const route = parseRouteFromHash(window.location.hash);
  if (route.isPermalink && route.cid) {
    // route.cid is already a CID with Uint8Array fields from nhashDecode
    treeRootStore.set(route.cid);
  }
}

// Initialize permalink synchronously (before currentDirHash subscribes)
initializePermalink();

// Initialize the store once - guard against HMR re-initialization
// Store the flag on a global to persist across HMR module reloads
const HMR_KEY = '__treeRootStoreInitialized';
const globalObj = typeof globalThis !== 'undefined' ? globalThis : window;

// Use queueMicrotask to defer until after module initialization completes
// This avoids circular dependency issues with nostr.ts -> store.ts
queueMicrotask(() => {
  if ((globalObj as Record<string, unknown>)[HMR_KEY]) return;
  (globalObj as Record<string, unknown>)[HMR_KEY] = true;
  createTreeRootStore();
});
