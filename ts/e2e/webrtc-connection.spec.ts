/**
 * E2E test for WebRTC peer connections via temp.iris.to relay
 *
 * Creates two browser contexts, each with a WebRTCStore instance,
 * and verifies they can discover each other, establish connections,
 * and exchange content by hash.
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, waitForAppReady, useLocalRelay, disableOthersPool, followUser, waitForFollowInWorker, waitForWebRTCConnection } from './test-utils.js';

test.describe('WebRTC P2P Connection', () => {
  test.setTimeout(120000);

  test('two hashtree-ts peers can exchange content by hash', async ({ browser }) => {
    // Create two browser contexts (simulating two different peers)
    const context1 = await browser.newContext();
    const context2 = await browser.newContext();

    const page1 = await context1.newPage();
    const page2 = await context2.newPage();

    // Log browser console
    page1.on('console', msg => {
      const text = msg.text();
      if (text.includes('CONNECTED') || text.includes('Check') || text.includes('GOT CONTENT') || text.includes('Stored content')) {
        console.log(`[Peer1] ${text}`);
      }
    });
    page2.on('console', msg => {
      const text = msg.text();
      if (text.includes('CONNECTED') || text.includes('Check') || text.includes('GOT CONTENT') || text.includes('Requesting')) {
        console.log(`[Peer2] ${text}`);
      }
    });

    // Navigate to app (dev server must be running)
    setupPageErrorHandler(page1);
    setupPageErrorHandler(page2);

    await page1.goto('http://localhost:5173');
    await page2.goto('http://localhost:5173');

    await waitForAppReady(page1);
    await waitForAppReady(page2);
    await useLocalRelay(page1);
    await useLocalRelay(page2);
    await disableOthersPool(page1);
    await disableOthersPool(page2);

    const peer1Keys = await page1.evaluate(() => {
      const state = (window as any).__nostrStore?.getState?.();
      return { pubkey: state?.pubkey ?? null, npub: state?.npub ?? null };
    });
    const peer2Keys = await page2.evaluate(() => {
      const state = (window as any).__nostrStore?.getState?.();
      return { pubkey: state?.pubkey ?? null, npub: state?.npub ?? null };
    });

    expect(peer1Keys.pubkey).toBeTruthy();
    expect(peer1Keys.npub).toBeTruthy();
    expect(peer2Keys.pubkey).toBeTruthy();
    expect(peer2Keys.npub).toBeTruthy();

    await followUser(page1, peer2Keys.npub);
    await followUser(page2, peer1Keys.npub);

    expect(await waitForFollowInWorker(page1, peer2Keys.pubkey, 20000)).toBe(true);
    expect(await waitForFollowInWorker(page2, peer1Keys.pubkey, 20000)).toBe(true);

    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForWebRTCConnection(page1, 60000, peer2Keys.pubkey);
    await waitForWebRTCConnection(page2, 60000, peer1Keys.pubkey);

    // Wait for WebRTC test functions to be available
    await page1.waitForFunction(() => typeof (window as any).runWebRTCTestWithContent === 'function', { timeout: 10000 });
    await page2.waitForFunction(() => typeof (window as any).runWebRTCTest === 'function', { timeout: 10000 });

    const testContent = 'Hello from hashtree-ts Peer 1!';

    // Start peer 1 with content first, get the content hash
    const peer1Promise = page1.evaluate((content) => (window as any).runWebRTCTestWithContent(content), testContent);

    // Wait for peer 1 to have pubkey and contentHash set
    await page1.waitForFunction(
      () => (window as any).testResults?.pubkey && (window as any).testResults?.contentHash,
      { timeout: 15000 }
    );

    // Get peer 1's info to find content hash
    const peer1Info = await page1.evaluate(() => (window as any).testResults || {});
    const contentHash = peer1Info.contentHash;
    const peer1Pubkey = peer1Info.pubkey;

    console.log(`\nPeer 1 pubkey: ${peer1Pubkey?.slice(0, 16)}...`);
    console.log(`Content hash: ${contentHash?.slice(0, 16)}...`);

    // Start peer 2 to request content from peer 1
    const runPeer2 = () => page2.evaluate(
      ([pubkey, hash]) => (window as any).runWebRTCTest(pubkey, hash),
      [peer1Pubkey, contentHash] as const
    );
    let result2 = await runPeer2();
    if (result2.connectedPeers === 0 || result2.contentRequestResult?.found !== true) {
      await page2.waitForTimeout(5000);
      result2 = await runPeer2();
    }

    // Wait for peer 1 to finish
    const result1 = await peer1Promise;

    console.log('\n=== Peer 1 Results ===');
    console.log(`Pubkey: ${result1.pubkey?.slice(0, 16)}...`);
    console.log(`Connected peers: ${result1.connectedPeers}`);
    console.log(`Content hash: ${result1.contentHash?.slice(0, 16)}...`);

    console.log('\n=== Peer 2 Results ===');
    console.log(`Pubkey: ${result2.pubkey?.slice(0, 16)}...`);
    console.log(`Connected peers: ${result2.connectedPeers}`);
    console.log(`Content request result:`, result2.contentRequestResult);

    // Clean up
    await context1.close();
    await context2.close();

    // Verify content was exchanged
    const contentReceived = result2.contentRequestResult?.found === true &&
                           result2.contentRequestResult?.data === testContent;

    console.log(`\nContent received correctly: ${contentReceived}`);

    expect(result2.connectedPeers).toBeGreaterThan(0);
    expect(contentReceived).toBe(true);
  });
});
