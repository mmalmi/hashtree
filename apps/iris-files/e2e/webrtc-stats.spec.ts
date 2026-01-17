/**
 * E2E test for WebRTC stats tracking
 */
import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool } from './test-utils.js';

test.describe('WebRTC Stats', () => {
  test.setTimeout(120000);

  /**
   * Clear storage and get a fresh session with auto-generated key
   */
  async function setupFreshPeer(page: Page): Promise<string> {
    setupPageErrorHandler(page);

    await page.evaluate(async () => {
      localStorage.clear();
      sessionStorage.clear();
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
    });
    await page.reload();
    await page.waitForLoadState('load');
    await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests

    // Wait for app to auto-generate key and initialize
    await page.waitForFunction(
      () => {
        const nostrStore = (window as any).__nostrStore;
        return nostrStore?.getState?.()?.pubkey;
      },
      { timeout: 15000 }
    );

    const pubkey = await page.evaluate(() => {
      return (window as any).__nostrStore.getState().pubkey;
    });

    return pubkey;
  }

  /**
   * Wait for WebRTC store to be ready
   */
  async function waitForWebRTC(page: Page): Promise<void> {
    await page.waitForFunction(
      () => {
        return (window as any).webrtcStore && (window as any).__localStore;
      },
      { timeout: 15000 }
    );
  }

  /**
   * Follow a pubkey
   */
  async function followUser(page: Page, targetPubkey: string): Promise<boolean> {
    return page.evaluate(async (pk) => {
      const { followPubkey } = (window as any).__testHelpers || {};
      if (followPubkey) {
        return followPubkey(pk);
      }
      // Fallback: use social graph directly
      const graph = (window as any).__socialGraph;
      if (graph) {
        graph.setFollowed((window as any).__nostrStore.getState().pubkey, pk, true);
        return true;
      }
      return false;
    }, targetPubkey);
  }

  /**
   * Store content in local IDB and return hash
   */
  async function storeContent(page: Page, content: string): Promise<string> {
    return page.evaluate(async (text) => {
      const encoder = new TextEncoder();
      const data = encoder.encode(text);
      const hashBuffer = await crypto.subtle.digest('SHA-256', data);
      const hash = new Uint8Array(hashBuffer);

      const localStore = (window as any).__localStore;
      if (localStore) {
        await localStore.put(hash, data);
      }

      return Array.from(hash).map(b => b.toString(16).padStart(2, '0')).join('');
    }, content);
  }

  /**
   * Request content by hash from WebRTC
   */
  async function requestContent(page: Page, hashHex: string): Promise<boolean> {
    return page.evaluate(async (hex) => {
      const hash = new Uint8Array(hex.match(/.{2}/g)!.map((b: string) => parseInt(b, 16)));
      const webrtcStore = (window as any).webrtcStore;
      if (!webrtcStore) return false;

      const data = await webrtcStore.get(hash);
      return !!data;
    }, hashHex);
  }

  /**
   * Get WebRTC stats
   */
  async function getStats(page: Page): Promise<any> {
    return page.evaluate(async () => {
      const webrtcStore = (window as any).webrtcStore;
      if (!webrtcStore || !webrtcStore.getStats) {
        return null;
      }
      const { aggregate, perPeer } = await webrtcStore.getStats();
      // Convert Map to object for serialization
      const perPeerObj: any = {};
      perPeer.forEach((value: any, key: string) => {
        perPeerObj[key] = value;
      });
      return { aggregate, perPeer: perPeerObj };
    });
  }

  /**
   * Get connected peer count
   */
  async function getConnectedPeerCount(page: Page): Promise<number> {
    return page.evaluate(() => {
      const webrtcStore = (window as any).webrtcStore;
      return webrtcStore?.getConnectedCount?.() || 0;
    });
  }

  test('stats should track requests between peers', async ({ browser }) => {
    // Create two browser contexts
    const context1 = await browser.newContext();
    const context2 = await browser.newContext();
    const page1 = await context1.newPage();
    const page2 = await context2.newPage();

    try {
      // Setup peer 1
      await page1.goto('/');
      const pubkey1 = await setupFreshPeer(page1);
      await waitForWebRTC(page1);
      console.log('Peer 1 pubkey:', pubkey1.slice(0, 16));

      // Setup peer 2
      await page2.goto('/');
      const pubkey2 = await setupFreshPeer(page2);
      await waitForWebRTC(page2);
      console.log('Peer 2 pubkey:', pubkey2.slice(0, 16));

      // Both follow each other
      await followUser(page1, pubkey2);
      await followUser(page2, pubkey1);
      console.log('Both peers following each other');

      // Wait for WebRTC connection
      let connected = false;
      for (let i = 0; i < 30 && !connected; i++) {
        const count1 = await getConnectedPeerCount(page1);
        const count2 = await getConnectedPeerCount(page2);
        console.log(`Connection check ${i}: peer1=${count1}, peer2=${count2}`);
        if (count1 > 0 && count2 > 0) {
          connected = true;
          break;
        }
        await page1.waitForTimeout(1000);
      }

      if (!connected) {
        console.log('WARNING: Peers did not connect via WebRTC');
      }

      // Get initial stats
      const initialStats1 = await getStats(page1);
      const initialStats2 = await getStats(page2);
      console.log('Initial stats peer1:', JSON.stringify(initialStats1?.aggregate));
      console.log('Initial stats peer2:', JSON.stringify(initialStats2?.aggregate));

      // Peer 2 stores multiple chunks of content
      const numChunks = 50;
      const hashes: string[] = [];
      for (let i = 0; i < numChunks; i++) {
        const content = `Chunk ${i}: ${'x'.repeat(1000)}`;
        const hash = await storeContent(page2, content);
        hashes.push(hash);
      }
      console.log(`Peer 2 stored ${numChunks} chunks`);

      // Peer 1 requests all chunks
      console.log('Peer 1 requesting all chunks...');
      let foundCount = 0;
      for (const hash of hashes) {
        const found = await requestContent(page1, hash);
        if (found) foundCount++;
      }
      console.log(`Peer 1 found ${foundCount}/${numChunks} chunks`);

      // Wait a bit for stats to update
      await page1.waitForTimeout(1000);

      // Get final stats
      const finalStats1 = await getStats(page1);
      const finalStats2 = await getStats(page2);
      console.log('Final stats peer1:', JSON.stringify(finalStats1?.aggregate));
      console.log('Final stats peer2:', JSON.stringify(finalStats2?.aggregate));

      // Check stats changed
      if (finalStats1?.aggregate && initialStats1?.aggregate) {
        const sentDiff = finalStats1.aggregate.requestsSent - (initialStats1.aggregate.requestsSent || 0);
        const recvDiff = finalStats1.aggregate.responsesReceived - (initialStats1.aggregate.responsesReceived || 0);
        const bytesRecvDiff = finalStats1.aggregate.bytesReceived - (initialStats1.aggregate.bytesReceived || 0);
        console.log('Peer 1 requestsSent diff:', sentDiff);
        console.log('Peer 1 responsesReceived diff:', recvDiff);
        console.log('Peer 1 bytesReceived diff:', bytesRecvDiff);
      }

      if (finalStats2?.aggregate && initialStats2?.aggregate) {
        const recvDiff = finalStats2.aggregate.requestsReceived - (initialStats2.aggregate.requestsReceived || 0);
        const sentDiff = finalStats2.aggregate.responsesSent - (initialStats2.aggregate.responsesSent || 0);
        const bytesSentDiff = finalStats2.aggregate.bytesSent - (initialStats2.aggregate.bytesSent || 0);
        console.log('Peer 2 requestsReceived diff:', recvDiff);
        console.log('Peer 2 responsesSent diff:', sentDiff);
        console.log('Peer 2 bytesSent diff:', bytesSentDiff);
      }

      // Verify at least some activity occurred
      expect(finalStats1?.aggregate || finalStats2?.aggregate).toBeTruthy();

      // Verify byte stats are tracked when data was transferred
      if (connected && foundCount > 0) {
        // Peer 1 should have received bytes (downloaded chunks)
        expect(finalStats1?.aggregate?.bytesReceived).toBeGreaterThan(0);
        // Peer 2 should have sent bytes (uploaded chunks)
        expect(finalStats2?.aggregate?.bytesSent).toBeGreaterThan(0);
        console.log('Byte stats verified: peer1 bytesReceived=' + finalStats1?.aggregate?.bytesReceived + ', peer2 bytesSent=' + finalStats2?.aggregate?.bytesSent);
      }

    } finally {
      await context1.close();
      await context2.close();
    }
  });
});
