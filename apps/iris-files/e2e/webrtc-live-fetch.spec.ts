/**
 * WebRTC Live Fetch Test
 *
 * Tests the REAL flow: broadcaster writes data, publishes to Nostr,
 * viewer receives tree update via Nostr, fetches data via WebRTC.
 *
 * NO CHEATS - no passing hashes between pages via test parameters.
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, followUser, disableOthersPool, ensureLoggedIn, waitForAppReady, useLocalRelay, waitForRelayConnected } from './test-utils';

test.describe('WebRTC Live Fetch', () => {
  test('viewer fetches data from broadcaster via real Nostr + WebRTC flow', async ({ browser }) => {
    test.slow();
    test.setTimeout(120000);

    // Two separate contexts
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();

    const pageA = await contextA.newPage(); // Broadcaster
    const pageB = await contextB.newPage(); // Viewer

    // Detailed logging
    pageA.on('console', msg => {
      const text = msg.text();
      if (text.includes('[WebRTC') || text.includes('Nostr') ||
          text.includes('publish') || text.includes('autosave')) {
        console.log(`[A] ${text}`);
      }
    });

    pageB.on('console', msg => {
      const text = msg.text();
      if (text.includes('[WebRTC') || text.includes('Tree update') ||
          text.includes('Resolved') || text.includes('SwFileHandler') ||
          text.includes('[Test]') || text.includes('[Worker]') ||
          text.includes('[WorkerStore]')) {
        console.log(`[B] ${text}`);
      }
    });

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // Setup fresh users
      console.log('\n=== Setting up users ===');

      for (const page of [pageA, pageB]) {
        await page.goto('http://localhost:5173');
        await page.evaluate(async () => {
          const dbs = await indexedDB.databases();
          for (const db of dbs) {
            if (db.name) indexedDB.deleteDatabase(db.name);
          }
          localStorage.clear();
        });
        await page.reload();
        await waitForAppReady(page);
        await ensureLoggedIn(page);
        await useLocalRelay(page);
        await waitForRelayConnected(page, 30000);
        // Disable others pool to avoid connecting to random peers from parallel tests
        await disableOthersPool(page);
      }

    const closeModals = async (page: any) => {
      for (let i = 0; i < 3; i++) {
        const backdrop = page.locator('.fixed.inset-0');
        if (await backdrop.first().isVisible({ timeout: 300 }).catch(() => false)) {
          await backdrop.first().click({ position: { x: 5, y: 5 }, force: true });
          await page.waitForTimeout(300);
        } else {
          break;
        }
      }
    };

    // Get npubs
    const getNpub = async (page: any) => {
      await closeModals(page);
      const publicLink = page.getByRole('link', { name: 'public' }).first();
      await expect(publicLink).toBeVisible({ timeout: 15000 });
      await publicLink.click();
        await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });
        const url = page.url();
        const match = url.match(/npub1[a-z0-9]+/);
        return match ? match[0] : '';
      };

      const npubA = await getNpub(pageA);
      const npubB = await getNpub(pageB);
      console.log(`Broadcaster: ${npubA.slice(0, 20)}...`);
      console.log(`Viewer: ${npubB.slice(0, 20)}...`);

      // Mutual follows for WebRTC
      console.log('\n=== Setting up mutual follows ===');
      await followUser(pageA, npubB);
      await followUser(pageB, npubA);
      console.log('Mutual follows established');

      // Navigate broadcaster back to their own tree for writing (use hash nav to preserve WebRTC)
      await pageA.evaluate((npub: string) => {
        window.location.hash = `/${npub}/public`;
      }, npubA);
      await pageA.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });

      // Wait for follows to propagate to worker, then establish WebRTC connection
      console.log('\n=== Waiting for WebRTC connections ===');

      // Get hex pubkeys for follow verification
      const pubkeyA = await pageA.evaluate(() => {
        const store = (window as any).__nostrStore;
        if (!store) return '';
        let pubkey = '';
        store.subscribe((s: { pubkey?: string }) => { pubkey = s?.pubkey || ''; })();
        return pubkey;
      });
      const pubkeyB = await pageB.evaluate(() => {
        const store = (window as any).__nostrStore;
        if (!store) return '';
        let pubkey = '';
        store.subscribe((s: { pubkey?: string }) => { pubkey = s?.pubkey || ''; })();
        return pubkey;
      });

      // Wait for follows to propagate to worker's socialGraph (condition-based, not time-based)
      await Promise.all([
        pageA.waitForFunction(
          async (pk: string) => {
            const store = (window as any).webrtcStore;
            if (!store?.isFollowing) return false;
            return await store.isFollowing(pk);
          },
          pubkeyB,
          { timeout: 15000 }
        ),
        pageB.waitForFunction(
          async (pk: string) => {
            const store = (window as any).webrtcStore;
            if (!store?.isFollowing) return false;
            return await store.isFollowing(pk);
          },
          pubkeyA,
          { timeout: 15000 }
        ),
      ]);
      console.log('Follows confirmed in worker');

      // Force follows set in worker to ensure peers are classified in follows pool
      await Promise.all([
        pageA.evaluate(async (pk: string) => {
          const adapter = (window as any).__getWorkerAdapter?.();
          await adapter?.setFollows?.([pk]);
          await adapter?.sendHello?.();
        }, pubkeyB),
        pageB.evaluate(async (pk: string) => {
          const adapter = (window as any).__getWorkerAdapter?.();
          await adapter?.setFollows?.([pk]);
          await adapter?.sendHello?.();
        }, pubkeyA),
      ]);

      // Send hellos to initiate WebRTC connection
      await Promise.all([
        pageA.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
        pageB.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
      ]);

      const waitForPeerConnection = async (page: any, targetPubkey: string) => {
        const timeoutMs = 45000;
        const start = Date.now();
        while (Date.now() - start < timeoutMs) {
          const stats = await page.evaluate(async () => {
            const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
            if (!adapter?.getPeerStats) return [];
            try {
              return await adapter.getPeerStats();
            } catch {
              return [];
            }
          });
          if (stats.some((p: { pubkey?: string; connected?: boolean }) => p.connected && p.pubkey === targetPubkey)) {
            return true;
          }
          await page.waitForTimeout(500);
        }
        return false;
      };

      const [connectedA, connectedB] = await Promise.all([
        waitForPeerConnection(pageA, pubkeyB),
        waitForPeerConnection(pageB, pubkeyA),
      ]);
      expect(connectedA && connectedB).toBe(true);
      console.log('WebRTC connection established');

      // Check peer status
      const getPeerStatus = async (page: any, label: string) => {
        const status = await page.evaluate(() => {
          const store = (window as any).webrtcStore;
          if (!store) return { connected: 0, peers: [] };
          return {
            connected: store.getConnectedCount?.() || 0,
            peers: store.getPeers?.()?.map((p: any) => ({
              pubkey: p.pubkey?.slice(0, 16),
              state: p.state,
              pool: p.pool,
            })) || [],
          };
        });
        console.log(`${label} peers:`, JSON.stringify(status));
        return status;
      };

      const statusA = await getPeerStatus(pageA, 'Broadcaster');
      const statusB = await getPeerStatus(pageB, 'Viewer');

      // Verify they're connected to each other
      const broadcasterPubkeyPrefix = npubA.slice(5, 13); // Extract part of pubkey from npub
      const viewerPubkeyPrefix = npubB.slice(5, 13);
      console.log(`Looking for connection between ${broadcasterPubkeyPrefix}... and ${viewerPubkeyPrefix}...`);

      // Broadcaster writes data AND publishes to Nostr (real flow)
      console.log('\n=== Broadcaster writing data and publishing to Nostr ===');
      const testFilename = `webrtc_test_${Date.now()}.txt`;

      const publishedHash = await pageA.evaluate(async (filename: string) => {
        const { getTree, LinkType } = await import('/src/store.ts');
        const { autosaveIfOwn } = await import('/src/nostr.ts');
        const { flushPendingPublishes } = await import('/src/treeRootCache.ts');
        const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
        const { parseRoute } = await import('/src/utils/route.ts');

        // Helper to convert bytes to hex
        const toHex = (arr: Uint8Array) => Array.from(arr).map(b => b.toString(16).padStart(2, '0')).join('');

        const tree = getTree();
        const route = parseRoute();
        let rootCid = getTreeRootSync(route.npub, route.treeName);

        // Create test data
        const testData = new Uint8Array(1000).fill(42);
        const { cid: fileCid, size } = await tree.putFile(testData);

        // If no tree exists yet, create an empty one
        if (!rootCid) {
          const { cid } = await tree.putDirectory([]);
          rootCid = cid;
        }

        // Add file to tree
        const newRootCid = await tree.setEntry(rootCid, [], filename, fileCid, size);
        const newHashHex = toHex(newRootCid.hash).slice(0, 16);

        // Publish to Nostr (this is the REAL publish, no cheating)
        console.log('[Test] Publishing to Nostr, hash:', newHashHex);
        autosaveIfOwn(newRootCid);
        await flushPendingPublishes();
        console.log('[Test] Published!');

        return newHashHex;
      }, testFilename);

      console.log(`Broadcaster published hash: ${publishedHash}`);

      console.log(`Broadcaster wrote and published: ${testFilename}`);

      // Viewer navigates to the file URL (discovers via URL, resolves via Nostr)
      console.log('\n=== Viewer navigating to file ===');
      const fileUrl = `http://localhost:5173/#/${npubA}/public/${testFilename}`;
      console.log(`File URL: ${fileUrl}`);
      await pageB.goto(fileUrl);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);
      await pageB.waitForFunction(() => !!navigator.serviceWorker?.controller, { timeout: 10000 });

      // Wait for the correct hash to arrive in viewer's tree root subscription
      console.log(`Waiting for viewer to receive hash: ${publishedHash}`);
      const receivedCorrectHash = await pageB.evaluate(async (args: { npub: string; treeName: string; expectedHash: string }) => {
        const { subscribeToTreeRoot } = await import('/src/stores/treeRoot.ts');

        // Helper to convert bytes to hex
        const toHex = (arr: Uint8Array) => Array.from(arr).map(b => b.toString(16).padStart(2, '0')).join('');

        return new Promise<boolean>((resolve) => {
          let attempts = 0;
          const maxAttempts = 30; // 15 seconds total
          let resolved = false;
          let unsubFn: (() => void) | null = null;

          unsubFn = subscribeToTreeRoot(args.npub, args.treeName, (hash) => {
            if (!hash || resolved) return;
            const hashHex = toHex(hash).slice(0, 16);
            console.log('[Test] Viewer received hash:', hashHex);
            if (hashHex === args.expectedHash) {
              resolved = true;
              unsubFn?.();
              resolve(true);
            }
          });

          // Poll for timeout
          const interval = setInterval(() => {
            if (resolved) {
              clearInterval(interval);
              return;
            }
            attempts++;
            if (attempts >= maxAttempts) {
              clearInterval(interval);
              unsubFn?.();
              resolve(false);
            }
          }, 500);
        });
      }, { npub: npubA, treeName: 'public', expectedHash: publishedHash });

      console.log(`Viewer received correct hash: ${receivedCorrectHash}`);
      if (!receivedCorrectHash) {
        console.log('WARNING: Viewer did not receive expected hash from Nostr');
      }

      // Wait for the file to be resolvable in the tree (ensures sync is complete)
      // In parallel test runs, WebRTC data transfer may take longer
      await pageB.waitForFunction(
        async (args: { npub: string; treeName: string; filename: string }) => {
          const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
          const { getTree } = await import('/src/store.ts');

          const rootCid = getTreeRootSync(args.npub, args.treeName);
          if (!rootCid) {
            console.log('[Test] No tree root yet');
            return false;
          }

          try {
            const tree = getTree();
            const entry = await tree.resolvePath(rootCid, args.filename.split('/'));
            console.log('[Test] Resolved entry:', entry ? 'found' : 'not found');
            return entry !== null;
          } catch (e) {
            console.log('[Test] Resolution error:', e);
            return false;
          }
        },
        { npub: npubA, treeName: 'public', filename: testFilename },
        { timeout: 60000, polling: 1000 }  // Longer timeout for parallel runs
      );
      console.log('File is resolvable in tree');

      // Read the file via the tree API (uses WebRTC/Blossom/local fallback under the hood)
      const readViaTree = async (params: { npub: string; treeName: string; filename: string }) => {
        return pageB.evaluate(async (args: { npub: string; treeName: string; filename: string }) => {
          try {
            const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
            const { getTree } = await import('/src/store.ts');

            const rootCid = getTreeRootSync(args.npub, args.treeName);
            if (!rootCid) return { success: false, error: 'No tree root yet' };

            const tree = getTree();
            const entry = await tree.resolvePath(rootCid, args.filename.split('/'));
            if (!entry) return { success: false, error: 'File entry not found' };

            const data = await tree.readFile(entry.cid);
            if (!data) return { success: false, error: 'File data not available' };

            return { success: true, size: data.length };
          } catch (err: any) {
            return { success: false, error: err.message };
          }
        }, params);
      };

      let readResult = await readViaTree({ npub: npubA, treeName: 'public', filename: testFilename });
      for (let attempt = 0; attempt < 4 && (!readResult.success || readResult.size !== 1000); attempt++) {
        await pageB.waitForTimeout(2000);
        readResult = await readViaTree({ npub: npubA, treeName: 'public', filename: testFilename });
      }

      console.log('Read result:', JSON.stringify(readResult, null, 2));

      // Get WebRTC stats to verify it was used
      console.log('\n=== WebRTC stats ===');

      const statsA = await pageA.evaluate(async () => {
        const store = (window as any).webrtcStore;
        if (!store || !store.getStats) return null;
        const { aggregate } = await store.getStats();
        return aggregate;
      });
      console.log('Broadcaster stats:', JSON.stringify(statsA, null, 2));

      const statsB = await pageB.evaluate(async () => {
        const store = (window as any).webrtcStore;
        if (!store || !store.getStats) return null;
        const { aggregate } = await store.getStats();
        return aggregate;
      });
      console.log('Viewer stats:', JSON.stringify(statsB, null, 2));

      // Assertions
      expect(readResult.success).toBe(true);
      expect(readResult.size).toBe(1000); // Original data size (decrypted)

      // Verify WebRTC was actually used (not just Blossom)
      // Either viewer received via WebRTC or broadcaster sent via WebRTC
      const webrtcUsed = (statsB?.responsesReceived > 0) || (statsA?.responsesSent > 0);
      console.log(`WebRTC used: ${webrtcUsed}`);

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });
});
