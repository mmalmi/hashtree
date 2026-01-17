/**
 * Shared test utilities for e2e tests
 */

import { expect } from './fixtures';

/**
 * Filter out noisy errors from relays that are irrelevant to tests.
 * - rate-limited: Some relays rate-limit nostr events
 * - pow: Some relays require Proof of Work on events (e.g., "pow: 28 bits needed")
 */
export function setupPageErrorHandler(page: any) {
  page.on('pageerror', (err: Error) => {
    const msg = err.message;
    if (!msg.includes('rate-limited') && !msg.includes('pow:') && !msg.includes('bits needed')) {
      console.log('Page error:', msg);
    }
  });
}

async function waitForTestHelpers(page: any, timeoutMs: number = 20000) {
  await page.waitForFunction(
    () => (window as any).__testHelpersReady === true,
    undefined,
    { timeout: timeoutMs }
  );
}

async function waitForWorkerAdapter(page: any, timeoutMs: number = 20000) {
  await page.waitForFunction(
    () => {
      const win = window as any;
      return typeof win.__getWorkerAdapter === 'function' && !!win.__getWorkerAdapter();
    },
    undefined,
    { timeout: timeoutMs }
  );
}

export async function evaluateWithRetry<T, R>(
  page: any,
  fn: (arg: T) => Promise<R> | R,
  arg: T,
  retries: number = 3
): Promise<R> {
  let lastError: unknown;
  for (let attempt = 0; attempt < retries; attempt++) {
    try {
      return await page.evaluate(fn, arg);
    } catch (err) {
      lastError = err;
      const message = err instanceof Error ? err.message : String(err);
      if (!message.includes('Execution context was destroyed')) {
        throw err;
      }
      await page.waitForLoadState('domcontentloaded');
      await page.waitForFunction(() => document.readyState === 'complete', { timeout: 5000 }).catch(() => {});
    }
  }
  throw lastError ?? new Error('Failed to evaluate after retries');
}

/**
 * Wait for the app to be ready (header visible).
 * Call this after page.reload() before calling disableOthersPool or configureBlossomServers.
 */
export async function waitForAppReady(page: any, timeoutMs: number = 30000) {
  await page.waitForLoadState('domcontentloaded').catch(() => {});
  await expect(page.locator('header').first()).toBeVisible({ timeout: timeoutMs });
  await waitForTestHelpers(page, timeoutMs);
}

/**
 * Wait for at least one relay connection.
 * Use this before tests that require Nostr publishes to be queryable by other users.
 */
export async function waitForRelayConnected(page: any, timeoutMs: number = 15000) {
  await waitForTestHelpers(page, timeoutMs);
  await page.waitForFunction(
    () => {
      const store = (window as any).__nostrStore;
      return (store?.getState?.().connectedRelays ?? 0) > 0;
    },
    undefined,
    { timeout: timeoutMs }
  );
}

/**
 * Ensure a user is logged in (create a new account if needed).
 */
export async function ensureLoggedIn(page: any, timeoutMs: number = 15000) {
  const alreadyLoggedIn = await page.evaluate(() => {
    const nostrStore = (window as any).__nostrStore;
    return (nostrStore?.getState?.().pubkey?.length ?? 0) === 64;
  }).catch(() => false);

  if (!alreadyLoggedIn) {
    const newBtn = page.getByRole('button', { name: 'New', exact: true });
    if (await newBtn.isVisible().catch(() => false)) {
      const modalBackdrop = page.locator('[data-modal-backdrop], div.fixed.inset-0.bg-black\\/70').first();
      if (await modalBackdrop.isVisible().catch(() => false)) {
        await page.keyboard.press('Escape').catch(() => {});
        if (await modalBackdrop.isVisible().catch(() => false)) {
          await modalBackdrop.click({ position: { x: 5, y: 5 } }).catch(() => {});
        }
        await modalBackdrop.waitFor({ state: 'hidden', timeout: 5000 }).catch(() => {});
      }
      await newBtn.click().catch(async () => {
        await newBtn.click({ force: true });
      });
    } else {
      await page.evaluate(async () => {
        const { generateNewKey } = await import('/src/nostr.ts');
        await generateNewKey();
      });
    }
  }

  await page.waitForFunction(() => {
    const nostrStore = (window as any).__nostrStore;
    return nostrStore?.getState()?.pubkey?.length === 64;
  }, { timeout: timeoutMs });
}

/**
 * Wait for new user setup to complete and navigate to public folder.
 * New users get three default folders created (public, link, private).
 * This function waits for setup, then clicks into the public folder.
 */
export async function navigateToPublicFolder(
  page: any,
  options?: { timeoutMs?: number; requireRelay?: boolean }
) {
  const timeoutMs = options?.timeoutMs ?? 30000;
  const requireRelay = options?.requireRelay ?? true;
  const relayTimeoutMs = requireRelay ? timeoutMs : Math.min(timeoutMs, 3000);
  const waitForRelay = () => requireRelay
    ? waitForRelayConnected(page, relayTimeoutMs)
    : waitForRelayConnected(page, relayTimeoutMs).catch(() => {});

  // First wait for the app to be ready - look for the Iris header
  await waitForAppReady(page, timeoutMs);
  await ensureLoggedIn(page, timeoutMs);
  await waitForRelay();

  // If we're already inside public (auto-redirect), just wait for actions and return
  const alreadyInPublic = await page.waitForFunction(() => {
    return /\/#\/npub[^/]+\/public/.test(window.location.hash);
  }, { timeout: 5000 }).then(() => true).catch(() => false);
  if (alreadyInPublic) {
    const actionsButton = page.getByRole('button', { name: /New Folder|File/i }).first();
    await expect(actionsButton).toBeVisible({ timeout: 10000 });
    return;
  }

  // Wait for the public folder link to appear in the tree list (indicates setup complete)
  // This can take a while for new users since default folders are created async
  // and published to Nostr fire-and-forget style
  const publicLink = page.getByRole('link', { name: 'public' }).first();

  if (!await publicLink.isVisible().catch(() => false)) {
    const logoLink = page.locator('header a:has-text("Iris")').first();
    if (await logoLink.isVisible().catch(() => false)) {
      await logoLink.click();
    }
    await page.evaluate(async () => {
      const nostrStore = (window as any).__nostrStore;
      const npub = nostrStore?.getState?.().npub;
      if (!npub) return;
      const { getLocalRootCache } = await import('/src/treeRootCache.ts');
      const { createTree } = await import('/src/actions/tree.ts');
      const defaults: Array<{ name: string; visibility: 'public' | 'link-visible' | 'private' }> = [
        { name: 'public', visibility: 'public' },
        { name: 'link', visibility: 'link-visible' },
        { name: 'private', visibility: 'private' },
      ];
      for (const { name, visibility } of defaults) {
        if (!getLocalRootCache(npub, name)) {
          await createTree(name, visibility, true);
        }
      }
    }).catch(() => {});
  }

  for (let attempt = 0; attempt < 2 && !await publicLink.isVisible().catch(() => false); attempt++) {
    await page.reload({ waitUntil: 'domcontentloaded' }).catch(() => {});
    await waitForAppReady(page, timeoutMs);
    await waitForRelay();
  }

  // Wait for public folder to appear
  await expect(publicLink).toBeVisible({ timeout: Math.max(60000, timeoutMs) });

  // Close any modal backdrop that might intercept clicks
  const modalBackdrop = page.locator('[data-modal-backdrop], div.fixed.inset-0.bg-black\\/70').first();
  if (await modalBackdrop.isVisible().catch(() => false)) {
    await modalBackdrop.click({ position: { x: 5, y: 5 } }).catch(() => {});
  }

  // Click into the public folder
  await publicLink.click();

  // Wait for navigation to complete and folder actions to be visible
  await page.waitForURL(/\/#\/npub.*\/public/, { timeout: timeoutMs });
  await expect(page.getByRole('button', { name: /New Folder|File/i }).first()).toBeVisible({ timeout: Math.max(20000, timeoutMs) });
}

/**
 * Navigate to user's tree list (home/root).
 * Clicks the logo in the header which links to home.
 */
export async function goToTreeList(page: any) {
  // Click the hashtree logo to go home
  const logoLink = page.locator('header a:has-text("Iris")').first();
  await expect(logoLink).toBeVisible({ timeout: 30000 });
  await logoLink.click();
  await page.waitForFunction(
    () => window.location.hash === '' || window.location.hash === '#/' || window.location.hash === '#',
    { timeout: 15000 }
  );

  // Wait for tree list to be visible (list view uses the same file list container)
  await expect(page.locator('[data-testid="file-list"]').first()).toBeVisible({ timeout: 30000 });
}

/**
 * Disable the "others pool" for WebRTC connections.
 * This prevents the app from connecting to random peers from other parallel tests.
 * Use this for single-user tests that don't need WebRTC connections but might be
 * affected by incoming data from parallel test instances.
 *
 * IMPORTANT: Call this BEFORE any navigation or state changes in the test.
 */
export async function disableOthersPool(page: any) {
  await waitForTestHelpers(page);
  await waitForWorkerAdapter(page);
  await page.waitForFunction(() => {
    const win = window as any;
    return !!win.__setPoolSettings || !!win.__settingsStore;
  }, { timeout: 10000 }).catch(() => {});
  await evaluateWithRetry(page, async () => {
    const win = window as any;
    const setPoolSettings = win.__setPoolSettings || win.__settingsStore?.setPoolSettings;
    if (setPoolSettings) {
      setPoolSettings({ otherMax: 0, otherSatisfied: 0 });
    } else {
      const { settingsStore } = await import('/src/stores/settings');
      settingsStore.setPoolSettings({ otherMax: 0, otherSatisfied: 0 });
    }

    let adapter = win.__getWorkerAdapter?.();
    if (!adapter) {
      const { getWorkerAdapter } = await import('/src/workerAdapter');
      adapter = getWorkerAdapter();
    }
    adapter?.setWebRTCPools({
      follows: { max: 20, satisfied: 10 },
      other: { max: 0, satisfied: 0 },
    });
  }, undefined);
}

/**
 * Force any pending tree publishes to complete.
 * Useful when a test needs newly created trees to be visible to another user.
 */
export async function flushPendingPublishes(page: any): Promise<void> {
  await waitForTestHelpers(page);
  await waitForWorkerAdapter(page);
  await evaluateWithRetry(page, async () => {
    const { flushPendingPublishes: flush } = await import('/src/treeRootCache');
    await flush();
  }, undefined);
}

/**
 * Enable the "others pool" for WebRTC connections.
 * Use this for tests that need same-user cross-device sync (same account on two browsers).
 * In test mode, the others pool is disabled by default to prevent interference.
 *
 * @param page - Playwright page
 * @param max - Maximum number of peers (default: 10)
 *
 * IMPORTANT: Call this AFTER login but BEFORE operations that need WebRTC.
 */
export async function enableOthersPool(page: any, max: number = 10) {
  await waitForTestHelpers(page);
  await waitForWorkerAdapter(page);
  await page.waitForFunction(() => {
    const win = window as any;
    return !!win.__setPoolSettings || !!win.__settingsStore;
  }, { timeout: 10000 }).catch(() => {});
  await evaluateWithRetry(page, async (maxPeers: number) => {
    const win = window as any;
    const setPoolSettings = win.__setPoolSettings || win.__settingsStore?.setPoolSettings;
    if (setPoolSettings) {
      setPoolSettings({ otherMax: maxPeers, otherSatisfied: Math.floor(maxPeers / 5), followsMax: 20, followsSatisfied: 10 });
    } else {
      const { settingsStore } = await import('/src/stores/settings');
      settingsStore.setPoolSettings({ otherMax: maxPeers, otherSatisfied: Math.floor(maxPeers / 5), followsMax: 20, followsSatisfied: 10 });
    }

    // Update the worker's WebRTC pool config - use the exposed global to avoid module duplication issues
    let adapter = win.__getWorkerAdapter?.();
    if (!adapter) {
      const { getWorkerAdapter } = await import('/src/workerAdapter');
      adapter = getWorkerAdapter();
    }
    if (adapter) {
      await adapter.setWebRTCPools({
        follows: { max: 20, satisfied: 10 },
        other: { max: maxPeers, satisfied: Math.floor(maxPeers / 5) },
      });
      console.log('[Test] Pool config updated via adapter, otherMax:', maxPeers);
    } else {
      console.error('[Test] No worker adapter available for pool config!');
    }
  }, max);
}

/**
 * Pre-set pool settings in IndexedDB before page load/reload.
 * This ensures WebRTC initializes with correct pool limits since it starts
 * before enableOthersPool can be called.
 *
 * IMPORTANT: Call this BEFORE reload when you need others pool enabled on init.
 */
export async function presetOthersPoolInDB(page: any) {
  await page.evaluate(async () => {
    // Open without version to use current version (Dexie manages versioning)
    const request = indexedDB.open('hashtree-settings');
    await new Promise<void>((resolve, reject) => {
      request.onerror = () => reject(request.error);
      request.onsuccess = () => {
        const db = request.result;
        // Check if the 'settings' object store exists
        if (!db.objectStoreNames.contains('settings')) {
          db.close();
          // Need to create the object store with version upgrade
          const upgradeRequest = indexedDB.open('hashtree-settings', db.version + 1);
          upgradeRequest.onupgradeneeded = () => {
            const upgradeDb = upgradeRequest.result;
            if (!upgradeDb.objectStoreNames.contains('settings')) {
              upgradeDb.createObjectStore('settings', { keyPath: 'key' });
            }
          };
          upgradeRequest.onsuccess = () => {
            const newDb = upgradeRequest.result;
            const tx = newDb.transaction('settings', 'readwrite');
            const store = tx.objectStore('settings');
            store.put({
              key: 'pools',
              value: {
                followsMax: 20,
                followsSatisfied: 10,
                otherMax: 10,
                otherSatisfied: 2
              }
            });
            tx.oncomplete = () => {
              newDb.close();
              resolve();
            };
            tx.onerror = () => reject(tx.error);
          };
          upgradeRequest.onerror = () => reject(upgradeRequest.error);
        } else {
          const tx = db.transaction('settings', 'readwrite');
          const store = tx.objectStore('settings');
          store.put({
            key: 'pools',
            value: {
              followsMax: 20,
              followsSatisfied: 10,
              otherMax: 10,
              otherSatisfied: 2
            }
          });
          tx.oncomplete = () => {
            db.close();
            resolve();
          };
          tx.onerror = () => reject(tx.error);
        }
      };
    });
  });
}

/**
 * Pre-set network relay settings in IndexedDB before page load.
 * This ensures the worker initializes with the local relay in test runs.
 *
 * IMPORTANT: Call this BEFORE the first page navigation.
 */
export function getTestRelayUrl(): string {
  return process.env.PW_TEST_RELAY_URL || process.env.VITE_TEST_RELAY || 'ws://localhost:4736';
}

export function getCrosslangPort(workerIndex: number): number {
  const baseEnv = Number(process.env.CROSSLANG_BASE_PORT);
  const basePort = Number.isFinite(baseEnv) && baseEnv > 0 ? baseEnv : 19090;
  const offset = Number.isFinite(workerIndex) && workerIndex >= 0 ? workerIndex : 0;
  return basePort + offset;
}

export async function presetLocalRelayInDB(page: any, relayUrl: string = getTestRelayUrl()) {
  await page.evaluate(async (relay: string) => {
    const request = indexedDB.open('hashtree-settings');
    await new Promise<void>((resolve, reject) => {
      request.onerror = () => reject(request.error);
      request.onsuccess = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains('settings')) {
          db.close();
          const upgradeRequest = indexedDB.open('hashtree-settings', db.version + 1);
          upgradeRequest.onupgradeneeded = () => {
            const upgradeDb = upgradeRequest.result;
            if (!upgradeDb.objectStoreNames.contains('settings')) {
              upgradeDb.createObjectStore('settings', { keyPath: 'key' });
            }
          };
          upgradeRequest.onsuccess = () => {
            const newDb = upgradeRequest.result;
            const tx = newDb.transaction('settings', 'readwrite');
            const store = tx.objectStore('settings');
            store.put({
              key: 'network',
              value: {
                relays: [relay],
                blossomServers: [],
                negentropyEnabled: false,
              },
            });
            tx.oncomplete = () => {
              newDb.close();
              resolve();
            };
            tx.onerror = () => reject(tx.error);
          };
          upgradeRequest.onerror = () => reject(upgradeRequest.error);
          return;
        }

        const tx = db.transaction('settings', 'readwrite');
        const store = tx.objectStore('settings');
        store.put({
          key: 'network',
          value: {
            relays: [relay],
            blossomServers: [],
            negentropyEnabled: false,
          },
        });
        tx.oncomplete = () => {
          db.close();
          resolve();
        };
        tx.onerror = () => reject(tx.error);
      };
    });
  }, relayUrl);
}

/**
 * Pre-set production relay settings in IndexedDB before page load.
 * This ensures the worker initializes with public relays even on localhost.
 *
 * IMPORTANT: Call this BEFORE reload or initial navigation that should use production relays.
 */
export async function presetProductionRelaysInDB(page: any) {
  await page.evaluate(async () => {
    const relays = [
      'wss://relay.damus.io',
      'wss://relay.primal.net',
      'wss://nos.lol',
      'wss://relay.nostr.band',
      'wss://relay.snort.social',
      'wss://temp.iris.to',
    ];
    const blossomServers = [
      { url: 'https://upload.iris.to', read: false, write: true },
      { url: 'https://cdn.iris.to', read: true, write: false },
      { url: 'https://hashtree.iris.to', read: true, write: false },
    ];

    const request = indexedDB.open('hashtree-settings');
    await new Promise<void>((resolve, reject) => {
      request.onerror = () => reject(request.error);
      request.onsuccess = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains('settings')) {
          db.close();
          const upgradeRequest = indexedDB.open('hashtree-settings', db.version + 1);
          upgradeRequest.onupgradeneeded = () => {
            const upgradeDb = upgradeRequest.result;
            if (!upgradeDb.objectStoreNames.contains('settings')) {
              upgradeDb.createObjectStore('settings', { keyPath: 'key' });
            }
          };
          upgradeRequest.onsuccess = () => {
            const newDb = upgradeRequest.result;
            const tx = newDb.transaction('settings', 'readwrite');
            const store = tx.objectStore('settings');
            store.put({
              key: 'network',
              value: {
                relays,
                blossomServers,
                negentropyEnabled: false,
              },
            });
            tx.oncomplete = () => {
              newDb.close();
              resolve();
            };
            tx.onerror = () => reject(tx.error);
          };
          upgradeRequest.onerror = () => reject(upgradeRequest.error);
          return;
        }

        const tx = db.transaction('settings', 'readwrite');
        const store = tx.objectStore('settings');
        store.put({
          key: 'network',
          value: {
            relays,
            blossomServers,
            negentropyEnabled: false,
          },
        });
        tx.oncomplete = () => {
          db.close();
          resolve();
        };
        tx.onerror = () => reject(tx.error);
      };
    });
  });
}

/**
 * Configure the app to use the local test relay instead of public relays.
 * This eliminates network flakiness and rate limiting issues during tests.
 *
 * Updates both settings store (for future store creations) and the
 * existing WebRTC store (for immediate effect on current connections).
 */
export async function useLocalRelay(page: any) {
  await waitForTestHelpers(page);
  const localRelay = getTestRelayUrl();
  await evaluateWithRetry(page, async (relay) => {
    // Update settings store for future store creations
    const { settingsStore } = await import('/src/stores/settings');
    settingsStore.setNetworkSettings({
      relays: [relay],
    });

    // Also update the running WebRTC store if it exists
    // Use window global which is always in sync with the app
    const store = (window as unknown as { webrtcStore?: { setRelays?: (relays: string[]) => void } }).webrtcStore;
    if (store && typeof store.setRelays === 'function') {
      store.setRelays([relay]);
    }
  }, localRelay);
}

/**
 * Configure Blossom servers for tests that need them.
 * By default, test mode disables Blossom servers to avoid external HTTP requests.
 * Call this for tests that specifically test Blossom functionality.
 *
 * Uses a global function exposed by the settings module to avoid Vite module duplication issues.
 */
export async function configureBlossomServers(page: any) {
  await waitForTestHelpers(page);
  await evaluateWithRetry(page, async () => {
    const configure = (window as unknown as { __configureBlossomServers?: (servers: unknown[]) => void }).__configureBlossomServers;
    if (!configure) {
      throw new Error('__configureBlossomServers not found - settings module may not be loaded');
    }
    configure([
      { url: 'https://upload.iris.to', read: false, write: true },
      { url: 'https://cdn.iris.to', read: true, write: false },
      { url: 'https://hashtree.iris.to', read: true, write: false },
    ]);
  }, undefined);
}

/**
 * Helper to follow a user by their npub.
 * Navigates to target's profile and clicks Follow, waiting for completion.
 * Use this to establish reliable WebRTC connections via the "follows pool".
 */
export async function followUser(page: any, targetNpub: string) {
  // Navigate to the user's profile page
  await page.goto(`http://localhost:5173/#/${targetNpub}`);

  // Dismiss any modal that might intercept the follow button
  const modalBackdrop = page.locator('.fixed.inset-0').first();
  if (await modalBackdrop.isVisible().catch(() => false)) {
    await page.keyboard.press('Escape').catch(() => {});
    await modalBackdrop.waitFor({ state: 'hidden', timeout: 1000 }).catch(() => {});
    if (await modalBackdrop.isVisible().catch(() => false)) {
      const closeBtn = modalBackdrop.getByRole('button', { name: /close|cancel|back/i }).first();
      if (await closeBtn.isVisible().catch(() => false)) {
        await closeBtn.click().catch(() => {});
      } else {
        await modalBackdrop.click({ position: { x: 5, y: 5 } }).catch(() => {});
      }
    }
    await modalBackdrop.waitFor({ state: 'hidden', timeout: 5000 }).catch(() => {});
  }

  // Click the Follow button
  const followButton = page.getByRole('button', { name: 'Follow', exact: true });
  await expect(followButton).toBeVisible({ timeout: 5000 });
  await followButton.click();

  // Wait for follow to complete - button becomes disabled or changes to "Following" or "Unfollow"
  await expect(
    page.getByRole('button', { name: 'Following' })
      .or(page.getByRole('button', { name: 'Unfollow' }))
      .or(followButton.and(page.locator('[disabled]')))
  ).toBeVisible({ timeout: 10000 });
}

/**
 * Wait for a pubkey to be in the worker's follows set.
 * This is essential before sending WebRTC hellos - ensures the peer will be
 * classified in the "follows" pool rather than "other" pool.
 */
export async function waitForFollowInWorker(page: any, pubkeyHex: string, timeoutMs: number = 15000): Promise<boolean> {
  return page.waitForFunction(
    (pk: string) => {
      const store = (window as any).webrtcStore;
      if (!store) return false;
      // Check via internal API that exposes followsSet
      const isFollowing = store.isFollowing?.(pk);
      if (isFollowing) return true;
      // Fallback: check directly if available
      const followsSet = store.getFollowsSet?.();
      return followsSet?.has?.(pk) ?? false;
    },
    pubkeyHex,
    { timeout: timeoutMs }
  ).then(() => true).catch(() => false);
}

/**
 * Wait for WebRTC connection to be established.
 * Polls until at least one peer is connected with data channel open.
 * If targetPubkey is provided, waits for that specific peer to connect.
 * Use this after users follow each other to ensure WebRTC is ready.
 */
export async function waitForWebRTCConnection(page: any, timeoutMs: number = 15000, targetPubkey?: string): Promise<boolean> {
  return page.waitForFunction(
    async (target: string | null) => {
      const adapter = (window as unknown as { __workerAdapter?: { getPeerStats: () => Promise<Array<{ connected?: boolean; pubkey?: string }>> } }).__workerAdapter;
      if (!adapter) return false;
      try {
        const stats = await adapter.getPeerStats();
        const connected = stats.filter((p: { connected?: boolean }) => p.connected);
        if (!target) return connected.length > 0;
        return connected.some((p: { pubkey?: string }) => p.pubkey === target);
      } catch {
        return false;
      }
    },
    targetPubkey ?? null,
    { timeout: timeoutMs, polling: 500 }
  ).then(() => true).catch(() => false);
}

/**
 * Login as a test user with a given nsec.
 * Sets the nsec in localStorage and reloads the page.
 *
 * @param page - Playwright page
 * @param nsec - Nostr secret key in bech32 format (nsec1...)
 */
export async function loginAsTestUser(page: any, nsec: string) {
  await page.evaluate((secret: string) => {
    localStorage.setItem('hashtree:loginType', 'nsec');
    localStorage.setItem('hashtree:nsec', secret);
  }, nsec);
  await page.reload();
  // Wait for app to be ready after login
  await expect(page.locator('header').first()).toBeVisible({ timeout: 30000 });
  await page.waitForFunction(() => {
    const store = (window as any).__nostrStore;
    return store?.getState?.().pubkey?.length === 64;
  }, { timeout: 30000 });
}

/**
 * Create a new folder using the UI.
 * Clicks "New Folder" button, fills the name, and waits for modal to close.
 *
 * @param page - Playwright page
 * @param folderName - Name of the folder to create
 */
export async function createFolder(page: any, folderName: string) {
  await page.getByRole('button', { name: 'New Folder' }).click();
  const input = page.locator('input[placeholder="Folder name..."]');
  await input.waitFor({ timeout: 5000 });
  await input.fill(folderName);
  await page.click('button:has-text("Create")');
  await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });
}

/**
 * Clear all browser storage (IndexedDB, localStorage, sessionStorage).
 * Use this to reset state between tests or create a fresh user.
 */
export async function clearAllStorage(page: any, options?: { clearOpfs?: boolean }) {
  await page.evaluate(async (opts?: { clearOpfs?: boolean }) => {
    const dbs = await indexedDB.databases();
    for (const db of dbs) {
      if (db.name) indexedDB.deleteDatabase(db.name);
    }
    localStorage.clear();
    sessionStorage.clear();

    if (opts?.clearOpfs) {
      try {
        const root = await navigator.storage.getDirectory();
        for await (const name of root.keys()) {
          await root.removeEntry(name, { recursive: true });
        }
      } catch {
        // OPFS might not be available
      }
    }
  });
}

/**
 * Setup a fresh user by clearing all storage and reloading.
 * Combines setupPageErrorHandler, goto, clearAllStorage, reload, and waitForAppReady.
 */
export async function setupFreshUser(page: any, options?: { timeoutMs?: number; clearOpfs?: boolean }) {
  setupPageErrorHandler(page);
  await page.goto('/');
  await clearAllStorage(page, { clearOpfs: options?.clearOpfs });
  await page.reload();
  await waitForAppReady(page, options?.timeoutMs ?? 30000);
}

/**
 * Add a file to the tree via the tree API.
 * This is faster than using the UI for file creation in tests.
 *
 * @param page - Playwright page
 * @param routePath - Path segments to the parent directory (empty for root)
 * @param filename - Name of the file to create
 * @param content - Text content of the file
 * @returns The new root CID, or null if failed
 */
export async function addFileViaTreeAPI(page: any, routePath: string[], filename: string, content: string): Promise<string | null> {
  return page.evaluate(async ({ routePath, filename, content }: { routePath: string[], filename: string, content: string }) => {
    const { getTree, LinkType } = await import('/src/store.ts');
    const { autosaveIfOwn } = await import('/src/nostr.ts');
    const { getCurrentRootCid } = await import('/src/actions/route.ts');
    const tree = getTree();
    const rootCid = getCurrentRootCid();
    if (!rootCid) return null;
    const data = new TextEncoder().encode(content);
    const { cid: fileCid, size } = await tree.putFile(data);
    const newRootCid = await tree.setEntry(rootCid, routePath, filename, fileCid, size, LinkType.Blob);
    autosaveIfOwn(newRootCid);
    return newRootCid;
  }, { routePath, filename, content });
}

/**
 * Navigate into a folder by clicking its link in the file list.
 * Waits for the folder to appear and for navigation to complete.
 *
 * @param page - Playwright page
 * @param folderName - Name of the folder to navigate into
 */
export async function navigateIntoFolder(page: any, folderName: string) {
  const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: folderName }).first();
  await expect(folderLink).toBeVisible({ timeout: 15000 });
  await folderLink.click();
  await page.waitForURL(new RegExp(folderName), { timeout: 10000 });
}
