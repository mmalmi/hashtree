/**
 * E2E test for WebRTC request forwarding
 *
 * Tests the scenario where:
 * - Peer A connects to Peer B (mutual follows)
 * - Peer B connects to Peer C (mutual follows)
 * - Peer A does NOT connect to Peer C (not following each other)
 * - Peer C has content in their store
 * - A requests content by hash and receives it via B's forwarding
 *
 * Pool configuration: others.max = 0 so only followed users connect.
 */
import { test, expect, type Page } from './fixtures';
import { waitForAppReady, waitForRelayConnected, waitForWebRTCConnection, evaluateWithRetry } from './test-utils.js';

test.describe('WebRTC Request Forwarding', () => {
  test.setTimeout(180000);

  /**
   * Clear storage and get a fresh session with auto-generated key
   * Pre-sets pool config in IndexedDB BEFORE reload so WebRTC starts with correct limits
   */
  async function setupFreshPeer(
    page: Page,
    poolConfig: { followsMax: number; followsSatisfied: number; otherMax: number; otherSatisfied: number }
  ): Promise<string> {
    // Clear all storage
    await page.evaluate(async () => {
      localStorage.clear();
      sessionStorage.clear();
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
    });

    // Pre-set pool settings in IndexedDB BEFORE reload
    // This ensures WebRTC initializes with correct pool limits
    await page.evaluate(async (cfg) => {
      const request = indexedDB.open('hashtree-settings', 1);
      await new Promise<void>((resolve, reject) => {
        request.onerror = () => reject(request.error);
        request.onupgradeneeded = () => {
          const db = request.result;
          if (!db.objectStoreNames.contains('settings')) {
            db.createObjectStore('settings', { keyPath: 'key' });
          }
        };
        request.onsuccess = () => {
          const db = request.result;
          const tx = db.transaction('settings', 'readwrite');
          const store = tx.objectStore('settings');
          store.put({
            key: 'pools',
            value: cfg
          });
          tx.oncomplete = () => {
            db.close();
            resolve();
          };
          tx.onerror = () => reject(tx.error);
        };
      });
    }, poolConfig);

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForAppReady(page, 30000);
    await waitForRelayConnected(page, 30000);

    // Wait for app to auto-generate key and initialize
    await page.waitForFunction(
      () => {
        const nostrStore = (window as any).__nostrStore;
        return nostrStore?.getState?.()?.pubkey;
      },
      { timeout: 30000 }
    );

    // Get the pubkey
    const pubkey = await page.evaluate(() => {
      return (window as any).__nostrStore.getState().pubkey;
    });

    return pubkey;
  }

  /**
   * Wait for test helpers to be available
   */
  async function waitForHelpers(page: Page): Promise<void> {
    await page.waitForFunction(
      () => {
        return (
          (window as any).__testHelpers?.followPubkey &&
          (window as any).webrtcStore &&
          (window as any).__localStore &&
          (window as any).__settingsStore
        );
      },
      { timeout: 15000 }
    );
  }

  /**
   * Wait for a pubkey to appear in the worker follows set.
   */
  async function waitForFollowInWorker(page: Page, pubkeyHex: string, timeout = 20000): Promise<boolean> {
    return page.waitForFunction(
      (pk) => {
        const store = (window as any).webrtcStore;
        if (!store) return false;
        if (store.isFollowing?.(pk)) return true;
        const followsSet = store.getFollowsSet?.();
        return followsSet?.has?.(pk) ?? false;
      },
      pubkeyHex,
      { timeout }
    ).then(() => true).catch(() => false);
  }

  /**
   * Set pool config (others.max = 0 means only follows can connect)
   */
  async function setPoolConfig(
    page: Page,
    config: { followsMax: number; followsSatisfied: number; otherMax: number; otherSatisfied: number }
  ): Promise<void> {
    await page.evaluate(async (cfg) => {
      // Update settings store
      const { settingsStore } = await import('/src/stores/settings');
      settingsStore.setPoolSettings({
        followsMax: cfg.followsMax,
        followsSatisfied: cfg.followsSatisfied,
        otherMax: cfg.otherMax,
        otherSatisfied: cfg.otherSatisfied,
      });

      // Update the worker's WebRTC pool config
      const { getWorkerAdapter } = await import('/src/workerAdapter');
      const adapter = getWorkerAdapter();
      adapter?.setWebRTCPools({
        follows: { max: cfg.followsMax, satisfied: cfg.followsSatisfied },
        other: { max: cfg.otherMax, satisfied: cfg.otherSatisfied },
      });
    }, config);
  }

  /**
   * Follow a pubkey using the app's follow system
   */
  async function followUser(page: Page, targetPubkey: string): Promise<boolean> {
    await page.waitForFunction(() => !!(window as any).__testHelpers?.followPubkey, { timeout: 10000 });
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const result = await page.evaluate(async (pk) => {
          const helpers = (window as any).__testHelpers;
          const followPubkey = helpers?.followPubkey;
          if (!followPubkey) {
            console.error('followPubkey not available');
            return false;
          }
          return followPubkey(pk);
        }, targetPubkey);
        if (result) {
          return true;
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        if (!message.includes('Execution context was destroyed')) {
          throw err;
        }
        await page.waitForLoadState('domcontentloaded').catch(() => {});
        await page.waitForFunction(() => !!(window as any).__testHelpers?.followPubkey, { timeout: 10000 });
      }
    }
    return false;
  }

  /**
   * Store content in a peer's local IDB store and return the hash
   */
  async function storeContent(page: Page, content: string): Promise<string> {
    return evaluateWithRetry(page, async (text) => {
      const encoder = new TextEncoder();
      const data = encoder.encode(text);

      // Hash the content
      const hashBuffer = await crypto.subtle.digest('SHA-256', data);
      const hash = new Uint8Array(hashBuffer);

      // Store in IDB via the app's store
      const localStore = (window as any).__localStore;
      if (localStore) {
        await localStore.put(hash, data);
      }

      // Return hash as hex
      return Array.from(hash).map(b => b.toString(16).padStart(2, '0')).join('');
    }, content);
  }

  /**
   * Request content by hash from WebRTC peers
   */
  async function requestContent(page: Page, hashHex: string): Promise<{ found: boolean; data?: string }> {
    return page.evaluate(async (hex) => {
      // Convert hex to Uint8Array
      const hash = new Uint8Array(hex.match(/.{2}/g)!.map((b: string) => parseInt(b, 16)));

      // Request via WebRTC store
      const webrtcStore = (window as any).webrtcStore;
      if (!webrtcStore) {
        return { found: false, error: 'No WebRTC store' };
      }

      let data = await webrtcStore.get(hash);
      for (let attempt = 0; attempt < 30 && !data; attempt++) {
        webrtcStore.sendHello?.();
        await new Promise(r => setTimeout(r, 1000));
        data = await webrtcStore.get(hash);
      }

      if (data) {
        const text = new TextDecoder().decode(data);
        return { found: true, data: text };
      }

      return { found: false };
    }, hashHex);
  }

  /**
   * Get peer info (pubkeys and pools)
   */
  async function getPeerInfo(page: Page): Promise<Array<{ pubkey: string; pool: string; state: string }>> {
    return page.evaluate(() => {
      const webrtcStore = (window as any).webrtcStore;
      const peers = webrtcStore?.getPeers?.() ?? [];
      return peers.map((p: any) => ({
        pubkey: p.pubkey?.slice(0, 16) + '...',
        pool: p.pool,
        state: p.state,
      }));
    });
  }

  /**
   * Wait for specific number of connected peers
   */
  test('peer A receives content from peer C via peer B forwarding', async ({ browser }) => {
    // Pool config: only follows can connect (others.max = 0)
    // B needs to connect to 2 follows (A and C), so satisfiedConnections must be >= 2
    const poolConfig = {
      followsMax: 10,
      followsSatisfied: 2,
      otherMax: 0,
      otherSatisfied: 0,
    };

    // Create three browser contexts with separate storage
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();
    const contextC = await browser.newContext();

    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();
    const pageC = await contextC.newPage();

    // Log relevant console messages for debugging (only errors)
    const logFilter = (label: string) => (msg: any) => {
      if (msg.type() === 'error') {
        console.log(`[${label}] ERROR: ${msg.text().slice(0, 200)}`);
      }
    };
    pageA.on('console', logFilter('A'));
    pageB.on('console', logFilter('B'));
    pageC.on('console', logFilter('C'));

    try {
      // Navigate all pages to the app
      await Promise.all([
        pageA.goto('http://localhost:5173'),
        pageB.goto('http://localhost:5173'),
        pageC.goto('http://localhost:5173'),
      ]);

      // Wait for helpers to be ready on all pages
      await Promise.all([
        waitForHelpers(pageA),
        waitForHelpers(pageB),
        waitForHelpers(pageC),
      ]);

      // Immediately disable others pool on all pages to prevent cross-talk from parallel tests
      await Promise.all([
        setPoolConfig(pageA, poolConfig),
        setPoolConfig(pageB, poolConfig),
        setPoolConfig(pageC, poolConfig),
      ]);

      console.log('\n=== Setting up Peer C (content provider) ===');
      const pubkeyC = await setupFreshPeer(pageC, poolConfig);
      await waitForHelpers(pageC);
      console.log(`Peer C pubkey: ${pubkeyC.slice(0, 16)}...`);

      // Store content on C
      const testContent = 'Hello from Peer C via forwarding!';
      const contentHash = await storeContent(pageC, testContent);
      console.log(`Content stored with hash: ${contentHash.slice(0, 16)}...`);

      console.log('\n=== Setting up Peer B (forwarder) ===');
      const pubkeyB = await setupFreshPeer(pageB, poolConfig);
      await waitForHelpers(pageB);
      console.log(`Peer B pubkey: ${pubkeyB.slice(0, 16)}...`);

      console.log('\n=== Setting up Peer A (requester) ===');
      const pubkeyA = await setupFreshPeer(pageA, poolConfig);
      await waitForHelpers(pageA);
      console.log(`Peer A pubkey: ${pubkeyA.slice(0, 16)}...`);

      console.log('\n=== Setting up follow relationships ===');
      // B <-> C mutual follows
      await followUser(pageB, pubkeyC);
      await followUser(pageC, pubkeyB);
      expect(await waitForFollowInWorker(pageB, pubkeyC, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageC, pubkeyB, 20000)).toBe(true);

      // A <-> B mutual follows
      await followUser(pageA, pubkeyB);
      await followUser(pageB, pubkeyA);
      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);

      // Force all peers to send hellos now that follows are set up
      // This avoids waiting for the 10-second hello interval
      await Promise.all([
        pageA.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
        pageB.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
        pageC.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
      ]);

      console.log('\n=== Waiting for connections to establish ===');

      // Wait for B to have 2 connections (A and C)
      console.log('Waiting for B to connect to both A and C...');
      await Promise.all([
        waitForWebRTCConnection(pageB, 120000, pubkeyA),
        waitForWebRTCConnection(pageB, 120000, pubkeyC),
      ]);

      const bPeers = await getPeerInfo(pageB);
      console.log('B connected peers:', JSON.stringify(bPeers));

      // Wait for A to connect to B
      console.log('Waiting for A to connect to B...');
      await waitForWebRTCConnection(pageA, 60000, pubkeyB);

      const aPeers = await getPeerInfo(pageA);
      console.log('A peers:', JSON.stringify(aPeers));

      // Verify A is NOT directly CONNECTED to C (state === 'connected')
      const aConnectedPeers = aPeers.filter(p => p.state === 'connected');
      const aConnectedToPubkeys = aConnectedPeers.map(p => p.pubkey);
      const cPubkeyPrefix = pubkeyC.slice(0, 16) + '...';
      expect(aConnectedToPubkeys).not.toContain(cPubkeyPrefix);
      console.log('Verified: A is NOT directly connected to C');

      console.log('\n=== A requesting content by hash ===');
      const result = await requestContent(pageA, contentHash);

      console.log('\n=== Results ===');
      console.log(`Content received: ${result.found}`);
      if (result.data) {
        console.log(`Content: "${result.data}"`);
      }

      // Final peer states
      const finalAPeers = await getPeerInfo(pageA);
      const finalBPeers = await getPeerInfo(pageB);
      const finalCPeers = await getPeerInfo(pageC);

      console.log('\nFinal peer states:');
      console.log(`A peers: ${JSON.stringify(finalAPeers)}`);
      console.log(`B peers: ${JSON.stringify(finalBPeers)}`);
      console.log(`C peers: ${JSON.stringify(finalCPeers)}`);

      // Verify others pool is empty on all peers
      const aOtherPeers = finalAPeers.filter(p => p.pool === 'other');
      const bOtherPeers = finalBPeers.filter(p => p.pool === 'other');
      const cOtherPeers = finalCPeers.filter(p => p.pool === 'other');

      console.log('\nOthers pool check:');
      console.log(`A others: ${aOtherPeers.length}, B others: ${bOtherPeers.length}, C others: ${cOtherPeers.length}`);

      // Assertions
      expect(aOtherPeers.length).toBe(0);
      expect(bOtherPeers.length).toBe(0);
      expect(cOtherPeers.length).toBe(0);

      expect(result.found).toBe(true);
      expect(result.data).toBe(testContent);

      // Verify A got content through B (not directly from C)
      const finalAConnectedToPubkeys = finalAPeers.map(p => p.pubkey);
      expect(finalAConnectedToPubkeys).not.toContain(cPubkeyPrefix);

      console.log('\n=== Test passed ===');
      console.log('- A received content from C via B forwarding');
      console.log('- A was NOT directly connected to C');
      console.log('- B was connected to both A and C');
      console.log('- Others pool is 0 for all peers');

    } finally {
      // Cleanup
      await Promise.all([
        contextA.close(),
        contextB.close(),
        contextC.close(),
      ]);
    }
  });
});
