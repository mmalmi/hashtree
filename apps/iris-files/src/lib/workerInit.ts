/**
 * Worker Initialization
 *
 * Initializes the hashtree worker for offloading storage and networking
 * from the main thread.
 */

import { initWorkerAdapter, getWorkerAdapter as getWebWorkerAdapter, type WorkerAdapter } from '../workerAdapter';
import { initTauriWorkerAdapter, getTauriWorkerAdapter, closeTauriWorkerAdapter, type TauriWorkerAdapter } from './tauriWorkerAdapter';
import { settingsStore, waitForSettingsLoaded } from '../stores/settings';
import { refreshWebRTCStats } from '../store';
import { get } from 'svelte/store';
import { createFollowsStore, getFollowsSync } from '../stores/follows';
import { setupVersionCallback } from '../utils/socialGraph';
import { ndk } from '../nostr/ndk';
import { initRelayTracking } from '../nostr/relays';
import { isTauri, hasTauriInvoke } from '../tauri';
import { getAppType } from '../appType';
import { logHtreeDebug } from './htreeDebug';
import type { NDKEvent, NDKFilter, NDKSubscription } from 'ndk';
import type { WorkerNostrFilter, WorkerSignedEvent } from 'hashtree';
// Import worker using Vite's ?worker query - returns a Worker constructor
import HashtreeWorker from '../workers/hashtree.worker.ts?worker';

// Unified adapter type - both implement the same interface
type UnifiedAdapter = WorkerAdapter | TauriWorkerAdapter;
// Track which backend is in use
let usingTauriBackend = false;
const isTestMode = !!import.meta.env.VITE_TEST_MODE;

/**
 * Get the active worker adapter (either web worker or Tauri backend)
 */
export function getWorkerAdapter(): UnifiedAdapter | null {
  if (usingTauriBackend) {
    return getTauriWorkerAdapter();
  }
  return getWebWorkerAdapter();
}

if (typeof window !== 'undefined') {
  (window as typeof window & { __getWorkerAdapter?: () => UnifiedAdapter | null }).__getWorkerAdapter = getWorkerAdapter;
}

export async function waitForWorkerAdapter(maxWaitMs = 5000): Promise<UnifiedAdapter | null> {
  const start = Date.now();
  let adapter = getWorkerAdapter();
  while (!adapter && Date.now() - start < maxWaitMs) {
    await new Promise(resolve => setTimeout(resolve, 50));
    adapter = getWorkerAdapter();
  }
  return adapter;
}

let initialized = false;
let initPromise: Promise<void> | null = null;
let lastPoolConfigHash = '';
let lastFollowsHash = '';
let lastBlossomServersHash = '';
let lastRelaysHash = '';
let followsUnsubscribe: (() => void) | null = null;
const workerSubscriptionIds = new WeakMap<object, string>();

/**
 * Sync pool settings from settings store to worker.
 * Uses a hash to avoid duplicate updates.
 */
function syncPoolSettings(): void {
  const adapter = getWorkerAdapter();
  if (!adapter) return;

  const settings = get(settingsStore);
  const poolConfig = {
    follows: { max: settings.pools.followsMax, satisfied: settings.pools.followsSatisfied },
    other: { max: settings.pools.otherMax, satisfied: settings.pools.otherSatisfied },
  };

  // Hash to avoid duplicate updates
  const configHash = JSON.stringify(poolConfig);
  if (configHash === lastPoolConfigHash) return;
  lastPoolConfigHash = configHash;

  console.log('[WorkerInit] Syncing pool settings to worker:', poolConfig);
  adapter.setWebRTCPools(poolConfig);
}

/**
 * Sync blossom server settings from settings store to worker.
 * Uses a hash to avoid duplicate updates.
 */
function syncBlossomServers(): void {
  const adapter = getWorkerAdapter();
  if (!adapter) return;

  const settings = get(settingsStore);
  const blossomServers = settings.network.blossomServers;

  // Hash to avoid duplicate updates
  const serversHash = JSON.stringify(blossomServers);
  if (serversHash === lastBlossomServersHash) return;
  lastBlossomServersHash = serversHash;

  console.log('[WorkerInit] Syncing blossom servers to worker:', blossomServers.length, 'servers');
  adapter.setBlossomServers(blossomServers);
}

/**
 * Sync relay settings from settings store to worker.
 * Uses a hash to avoid duplicate updates.
 */
function syncRelays(): void {
  const adapter = getWorkerAdapter();
  if (!adapter) return;

  // Only Tauri adapter supports setRelays
  if (!('setRelays' in adapter)) return;

  const settings = get(settingsStore);
  const relays = settings.network.relays;

  // Hash to avoid duplicate updates
  const relaysHash = JSON.stringify(relays);
  if (relaysHash === lastRelaysHash) return;
  lastRelaysHash = relaysHash;

  console.log('[WorkerInit] Syncing relays to worker:', relays.length, 'relays');
  (adapter as TauriWorkerAdapter).setRelays(relays);
}

let lastStorageMaxBytesHash = '';

/**
 * Sync storage limit from settings store to worker.
 * Uses a hash to avoid duplicate updates.
 */
function syncStorageSettings(): void {
  const adapter = getWorkerAdapter();
  if (!adapter) return;

  const settings = get(settingsStore);
  const maxBytes = settings.storage.maxBytes;

  // Hash to avoid duplicate updates
  const storageHash = String(maxBytes);
  if (storageHash === lastStorageMaxBytesHash) return;
  lastStorageMaxBytesHash = storageHash;

  console.log('[WorkerInit] Syncing storage limit to worker:', Math.round(maxBytes / 1024 / 1024), 'MB');
  adapter.setStorageMaxBytes(maxBytes);
}

/**
 * Sync follows list to worker for WebRTC peer classification.
 */
async function syncFollows(follows: string[]): Promise<void> {
  const adapter = getWorkerAdapter();
  if (!adapter) return;

  // Hash to avoid duplicate updates
  const followsHash = follows.join(',');
  if (followsHash === lastFollowsHash) return;
  lastFollowsHash = followsHash;

  console.log('[WorkerInit] Syncing follows to worker:', follows.length, 'pubkeys');
  await adapter.setFollows(follows);
}

// Track follows store for cleanup
let followsStoreDestroy: (() => void) | null = null;

/**
 * Set up follows subscription for the current user.
 */
function setupFollowsSubscription(pubkey: string): void {
  // Clean up previous subscription
  if (followsUnsubscribe) {
    followsUnsubscribe();
    followsUnsubscribe = null;
  }
  if (followsStoreDestroy) {
    followsStoreDestroy();
    followsStoreDestroy = null;
  }

  // Sync current follows if available
  const currentFollows = getFollowsSync(pubkey);
  if (currentFollows) {
    syncFollows(currentFollows.follows);
  }

  // Create follows store and subscribe to changes
  const followsStore = createFollowsStore(pubkey);
  followsStoreDestroy = followsStore.destroy;
  followsUnsubscribe = followsStore.subscribe((follows) => {
    if (follows) {
      syncFollows(follows.follows);
    }
  });
}

export function updateFollowsSubscription(pubkey: string): void {
  if (!initialized) return;
  setupFollowsSubscription(pubkey);
}

export interface WorkerInitIdentity {
  pubkey: string;
  nsec?: string;  // hex-encoded secret key (only for nsec login)
}

/**
 * Wait for service worker to be ready (needed for COOP/COEP headers)
 */
async function waitForServiceWorker(maxWaitMs?: number): Promise<boolean> {
  // Skip in Tauri - no service worker needed
  if (isTauri()) return true;
  if (!('serviceWorker' in navigator)) return true;

  try {
    // Wait for service worker to be ready
    if (navigator.serviceWorker.controller) {
      return true;
    }

    const readyPromise = navigator.serviceWorker.ready.then(() => true);
    if (maxWaitMs === undefined) {
      return await readyPromise;
    }

    const timeoutPromise = new Promise<boolean>((resolve) => {
      setTimeout(() => resolve(false), maxWaitMs);
    });

    return await Promise.race([readyPromise, timeoutPromise]);
  } catch {
    // Service worker not available, continue anyway
    return false;
  }
}

/**
 * Initialize the hashtree worker with user identity.
 * Safe to call multiple times - only initializes once.
 */
export async function initHashtreeWorker(identity: WorkerInitIdentity): Promise<void> {
  if (initialized) return;
  if (initPromise) {
    await initPromise;
    return;
  }

  initPromise = (async () => {
    const t0 = performance.now();
    const logT = (msg: string) => console.log(`[initHashtreeWorker] ${msg}: ${Math.round(performance.now() - t0)}ms`);
    const protocol = typeof window !== 'undefined' ? window.location?.protocol || '' : '';
    const hasTauriGlobals = typeof window !== 'undefined'
      ? ('__TAURI_INTERNALS__' in window || '__TAURI__' in window || '__TAURI_IPC__' in window || '__TAURI_METADATA__' in window)
      : false;
    const tauriInvokeAvailable = hasTauriInvoke();
    logHtreeDebug('worker:init:start', {
      appType: getAppType(),
      protocol,
      hasTauriGlobals,
      isTauri: isTauri(),
      hasTauriInvoke: tauriInvokeAvailable,
    });

    try {
      // Wait for service worker to be ready before loading workers
      // This ensures COOP/COEP headers are in place
      const appType = getAppType();
      const swWaitMs = appType === 'video' ? undefined : 500;
      const serviceWorkerPromise = waitForServiceWorker(swWaitMs).then((ready) => {
        logT(ready ? 'waitForServiceWorker done' : 'waitForServiceWorker timed out');
      });

      // Load settings before worker init so relays/blossom match persisted config.
      // In production, don't block for long if IndexedDB is slow.
      const settingsReady = waitForSettingsLoaded().then(() => {
        logT('waitForSettingsLoaded done');
        return true;
      });
      const settingsPromise = isTestMode
        ? settingsReady
        : Promise.race([
            settingsReady,
            new Promise<boolean>((resolve) => {
              setTimeout(() => {
                logT('waitForSettingsLoaded timed out');
                resolve(false);
              }, 500);
            }),
          ]);

      await Promise.all([serviceWorkerPromise, settingsPromise]);

      // Get settings (may still be defaults if IndexedDB hasn't loaded yet)
      const settings = get(settingsStore);

      const config = {
        storeName: 'hashtree-worker',
        relays: settings.network.relays,
        blossomServers: settings.network.blossomServers,
        pubkey: identity.pubkey,
        nsec: identity.nsec,
      };

      // Use Tauri backend in desktop app when invoke is available; otherwise fall back to web worker.
      if (isTauri() && tauriInvokeAvailable) {
        logT('Starting Tauri native backend');
        try {
          await initTauriWorkerAdapter(config);
          usingTauriBackend = true;
          logT('Tauri native backend ready');
          logHtreeDebug('worker:init:ready', { backend: 'tauri' });
        } catch (err) {
          usingTauriBackend = false;
          closeTauriWorkerAdapter();
          console.error('[initHashtreeWorker] initTauriWorkerAdapter FAILED:', err);
          logHtreeDebug('worker:init:error', { backend: 'tauri', error: String(err) });
        }
      } else if (isTauri() && !tauriInvokeAvailable) {
        logHtreeDebug('worker:init:tauri-missing', { protocol, hasTauriGlobals });
      }

      if (!usingTauriBackend) {
        logT('Starting web worker');
        await initWorkerAdapter(HashtreeWorker, config);
        logT('Web worker ready');
        logHtreeDebug('worker:init:ready', { backend: 'worker' });
      }

      initialized = true;
      logHtreeDebug('worker:init:done', { backend: usingTauriBackend ? 'tauri' : 'worker' });

      // Register worker as transport plugin for NDK publishes and subscriptions
      const adapter = getWorkerAdapter();
      if (adapter) {
        // Set up event dispatch from worker to NDK subscriptions
        adapter.onEvent((event: WorkerSignedEvent) => {
          // Dispatch to all matching subscriptions via subManager
          ndk.subManager.dispatchEvent(event as unknown as NDKEvent, undefined, false);
        });

        const attachWorkerSubscription = (subscription: NDKSubscription, filters: NDKFilter[]) => {
          if (workerSubscriptionIds.has(subscription)) return;
          const subId = adapter.subscribe(
            filters as unknown as WorkerNostrFilter[],
            undefined, // events use global onEvent callback
            () => {
              // Forward EOSE to NDK subscription
              subscription.emit('eose', subscription);
            }
          );
          workerSubscriptionIds.set(subscription, subId);
          // Clean up when subscription closes
          subscription.on('close', () => {
            adapter.unsubscribe(subId);
            workerSubscriptionIds.delete(subscription);
          });
        };

        ndk.transportPlugins.push({
          name: 'worker',
          onPublish: async (event) => {
            // Route publish through worker (optimistic - don't throw on failure)
            try {
              await adapter.publish({
                id: event.id!,
                pubkey: event.pubkey,
                kind: event.kind!,
                content: event.content,
                tags: event.tags,
                created_at: event.created_at!,
                sig: event.sig!,
              });
            } catch (err) {
              // Log but don't throw - optimistic publishing
              console.warn('[WorkerInit] Publish failed:', err);
            }
          },
          onSubscribe: (subscription, filters) => {
            // Route subscription through worker (which has relay connections)
            attachWorkerSubscription(subscription, filters);
          },
        });
        console.log('[WorkerInit] Registered worker transport plugin for NDK');

        // Attach any subscriptions created before the transport plugin was registered.
        let attachedCount = 0;
        for (const subscription of ndk.subManager.subscriptions.values()) {
          attachWorkerSubscription(subscription, subscription.filters);
          attachedCount += 1;
        }
        if (attachedCount > 0) {
          console.log('[WorkerInit] Attached existing NDK subscriptions to worker:', attachedCount);
        }

        // Signal that the worker is ready for tree root subscriptions
        // This allows deferred subscriptions to start
        import('../stores/treeRoot').then(({ signalWorkerReady }) => {
          signalWorkerReady();
        });

        // Start connectivity polling ASAP after worker is ready.
        refreshWebRTCStats();
        setInterval(refreshWebRTCStats, 2000);
        initRelayTracking();
      }

      // Set up social graph version callback
      setupVersionCallback();

      // Subscribe to settings changes to keep worker in sync
      settingsStore.subscribe(() => {
        if (initialized) {
          syncPoolSettings();
          syncBlossomServers();
          syncRelays();
          syncStorageSettings();
        }
      });

      // Initial sync of all settings to worker (critical for WebRTC startup)
      syncPoolSettings();
      syncBlossomServers();
      syncRelays();
      syncStorageSettings();

      // Set up follows subscription for WebRTC peer classification
      setupFollowsSubscription(identity.pubkey);
    } catch (err) {
      console.error('[WorkerInit] Failed to initialize worker:', err);
      // Don't throw - app can still work without worker (fallback to main thread)
    }
  })();

  try {
    await initPromise;
  } finally {
    initPromise = null;
  }
}

/**
 * Check if the worker is initialized and ready.
 */
export function isWorkerReady(): boolean {
  return initialized && getWorkerAdapter() !== null;
}

/**
 * Wait for at least one relay to be connected.
 * Returns immediately if worker is not ready or times out after maxWait ms.
 */
export async function waitForRelayConnection(maxWait = 5000): Promise<boolean> {
  const adapter = getWorkerAdapter();
  if (!adapter) return false;

  const startTime = Date.now();
  const pollInterval = 100;

  while (Date.now() - startTime < maxWait) {
    try {
      const stats = await adapter.getRelayStats();
      const connected = stats.filter(r => r.connected).length;
      if (connected > 0) {
        return true;
      }
    } catch {
      // Ignore errors, keep polling
    }
    await new Promise(resolve => setTimeout(resolve, pollInterval));
  }

  return false;
}

// getWorkerAdapter is now defined locally and exported above
