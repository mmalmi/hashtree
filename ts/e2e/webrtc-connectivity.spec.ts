/**
 * WebRTC Connectivity E2E Test
 *
 * Tests that WebRTC connections work and are reflected in the UI:
 * - Connectivity indicator changes color when peers connect
 * - Peers are shown on settings page
 */
import { test, expect, type Page } from './fixtures';
import { disableOthersPool, setupPageErrorHandler, waitForAppReady } from './test-utils';

test.describe('WebRTC Connectivity', () => {
  test.setTimeout(90000);

  // Max time from hello broadcast to peer connection (regression guard)
  const MAX_CONNECTION_TIME_MS = 3000;

  /**
   * Get pubkey from page
   */
  async function getPubkey(page: Page): Promise<string> {
    return page.evaluate(() => {
      const store = (window as any).__nostrStore;
      if (!store) return '';
      let pubkey = '';
      store.subscribe((s: { pubkey?: string }) => { pubkey = s?.pubkey || ''; })();
      return pubkey;
    });
  }

  /**
   * Follow a pubkey
   */
  async function followUser(page: Page, targetPubkey: string): Promise<void> {
    await page.evaluate(async (pk) => {
      const { followPubkey } = (window as any).__testHelpers || {};
      if (followPubkey) await followPubkey(pk);
    }, targetPubkey);
  }

  /**
   * Wait for connected peer count by querying worker directly
   */
  async function waitForPeers(page: Page, count: number, timeout = 4000): Promise<void> {
    await page.waitForFunction(
      async (expected) => {
        // Use window global for adapter - more reliable than dynamic import
        const adapter = (window as any).__workerAdapter;
        if (!adapter) return false;
        const stats = await adapter.getPeerStats();
        const connectedCount = stats.filter((p: { connected?: boolean }) => p.connected).length;
        return connectedCount >= expected;
      },
      count,
      { timeout }
    );
  }

  test('connectivity indicator shows green when peers connect', async ({ browser }) => {
    const ctx1 = await browser.newContext();
    const ctx2 = await browser.newContext();
    const page1 = await ctx1.newPage();
    const page2 = await ctx2.newPage();
    setupPageErrorHandler(page1);
    setupPageErrorHandler(page2);

    try {
      await Promise.all([
        page1.goto('http://localhost:5173/'),
        page2.goto('http://localhost:5173/'),
      ]);

      // Wait for apps to be ready
      await Promise.all([
        waitForAppReady(page1),
        waitForAppReady(page2),
      ]);

      await Promise.all([
        page1.waitForFunction(() => (window as any).__testHelpers?.followPubkey, undefined, { timeout: 15000 }),
        page2.waitForFunction(() => (window as any).__testHelpers?.followPubkey, undefined, { timeout: 15000 }),
      ]);

      await Promise.all([
        disableOthersPool(page1),
        disableOthersPool(page2),
      ]);

      const [pubkey1, pubkey2] = await Promise.all([getPubkey(page1), getPubkey(page2)]);

      // Mutual follows
      await Promise.all([
        followUser(page1, pubkey2),
        followUser(page2, pubkey1),
      ]);

      // Wait for follows to propagate to worker's socialGraph (condition-based, not time-based)
      await Promise.all([
        page1.waitForFunction(
          async (pk: string) => {
            const store = (window as any).webrtcStore;
            if (!store?.isFollowing) return false;
            return await store.isFollowing(pk);
          },
          pubkey2,
          { timeout: 15000 }
        ),
        page2.waitForFunction(
          async (pk: string) => {
            const store = (window as any).webrtcStore;
            if (!store?.isFollowing) return false;
            return await store.isFollowing(pk);
          },
          pubkey1,
          { timeout: 15000 }
        ),
      ]);

      // Trigger hello broadcast and measure connection time
      const helloTime = Date.now();
      await Promise.all([
        page1.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
        page2.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
      ]);

      // Wait for connection (should be fast now that we wait for pool config)
      await waitForPeers(page1, 1, MAX_CONNECTION_TIME_MS);
      const connectionTime = Date.now() - helloTime;
      console.log(`WebRTC connection time: ${connectionTime}ms`);

      // Regression guard: connection should be fast
      expect(connectionTime).toBeLessThan(MAX_CONNECTION_TIME_MS);

      // Wait for indicator to turn green (peers) or blue (follows peers)
      const indicator = page1.getByTestId('peer-indicator-dot');
      await expect(indicator).toBeVisible({ timeout: 5000 });

      // Wait for color to change from yellow to green/blue
      // Force UI refresh and verify in single function to avoid race condition
      await page1.waitForFunction(
        async () => {
          // Check worker state first
          const adapter = (window as any).__workerAdapter;
          if (!adapter) return false;
          const stats = await adapter.getPeerStats();
          const connected = stats.filter((p: { connected?: boolean }) => p.connected).length;
          if (connected === 0) return false;

          // Force UI refresh
          const { refreshWebRTCStats } = await import('/src/store');
          await refreshWebRTCStats();

          // Small delay to let Svelte re-render
          await new Promise(r => setTimeout(r, 100));

          // Check indicator color
          const el = document.querySelector('[data-testid="peer-indicator-dot"]');
          if (!el) return false;
          const color = getComputedStyle(el).color;
          // green: rgb(63, 185, 80) or blue: rgb(88, 166, 255)
          return color === 'rgb(63, 185, 80)' || color === 'rgb(88, 166, 255)';
        },
        undefined,
        { timeout: 30000, polling: 1000 }
      );
    } finally {
      await ctx1.close();
      await ctx2.close();
    }
  });

  test('peers are shown on settings page', async ({ browser }) => {
    const ctx1 = await browser.newContext();
    const ctx2 = await browser.newContext();
    const page1 = await ctx1.newPage();
    const page2 = await ctx2.newPage();
    setupPageErrorHandler(page1);
    setupPageErrorHandler(page2);

    try {
      await Promise.all([
        page1.goto('http://localhost:5173/'),
        page2.goto('http://localhost:5173/'),
      ]);

      // Wait for apps to be ready
      await Promise.all([
        waitForAppReady(page1),
        waitForAppReady(page2),
      ]);

      await Promise.all([
        page1.waitForFunction(() => (window as any).__testHelpers?.followPubkey, undefined, { timeout: 15000 }),
        page2.waitForFunction(() => (window as any).__testHelpers?.followPubkey, undefined, { timeout: 15000 }),
      ]);

      await Promise.all([
        disableOthersPool(page1),
        disableOthersPool(page2),
      ]);

      const [pubkey1, pubkey2] = await Promise.all([getPubkey(page1), getPubkey(page2)]);

      await Promise.all([
        followUser(page1, pubkey2),
        followUser(page2, pubkey1),
      ]);

      // Wait for follows to propagate to worker's socialGraph (condition-based, not time-based)
      await Promise.all([
        page1.waitForFunction(
          async (pk: string) => {
            const store = (window as any).webrtcStore;
            if (!store?.isFollowing) return false;
            return await store.isFollowing(pk);
          },
          pubkey2,
          { timeout: 15000 }
        ),
        page2.waitForFunction(
          async (pk: string) => {
            const store = (window as any).webrtcStore;
            if (!store?.isFollowing) return false;
            return await store.isFollowing(pk);
          },
          pubkey1,
          { timeout: 15000 }
        ),
      ]);

      await Promise.all([
        page1.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
        page2.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
      ]);

      await waitForPeers(page1, 1, 4000);

      await page1.goto('http://localhost:5173/#/settings');
      await page1.getByRole('button', { name: 'P2P' }).click();

      await page1.waitForFunction(
        () => document.body.innerText.includes('Peers'),
        undefined,
        { timeout: 30000 }
      );

      // Wait for Settings UI to show connected peers
      // Use waitForFunction that checks worker state, refreshes UI, and verifies
      await page1.waitForFunction(
        async () => {
          // Check worker state
          const adapter = (window as any).__workerAdapter;
          if (!adapter) return false;
          const stats = await adapter.getPeerStats();
          const connected = stats.filter((p: { connected?: boolean }) => p.connected).length;
          if (connected === 0) return false;

          // Force UI refresh
          const { refreshWebRTCStats } = await import('/src/store');
          await refreshWebRTCStats();

          // Small delay for Svelte re-render
          await new Promise(r => setTimeout(r, 100));

          // Check if UI updated
          const match = document.body.innerText.match(/Peers \((\d+)\)/);
          const count = match ? parseInt(match[1], 10) : 0;
          return count > 0;
        },
        undefined,
        { timeout: 30000, polling: 1000 }
      );
    } finally {
      await ctx1.close();
      await ctx2.close();
    }
  });
});
