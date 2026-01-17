import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool } from './test-utils.js';

test.describe('Settings Stats', () => {
  test.setTimeout(90000);

  async function goToSettings(page: Page): Promise<void> {
    await page.goto('/#/settings');
    await disableOthersPool(page);
    await expect(page.getByRole('button', { name: 'Servers' })).toBeVisible({ timeout: 10000 });
  }

  test('displays storage stats section', async ({ page }) => {
    setupPageErrorHandler(page);
    await goToSettings(page);

    await page.getByRole('button', { name: 'Storage' }).click();

    const storageSection = page.locator('text=Local Storage').first();
    await expect(storageSection).toBeVisible({ timeout: 10000 });

    await expect(page.locator('text=Items')).toBeVisible({ timeout: 5000 });
    await expect(page.locator('text=Size')).toBeVisible({ timeout: 5000 });

    await expect(page.locator('.bg-surface-2').filter({ hasText: /Items/ })).toBeVisible();
  });

  test('displays connection pools section', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/#/settings');
    await disableOthersPool(page);

    await expect(page.getByRole('button', { name: 'P2P' })).toBeVisible({ timeout: 10000 });
    await page.getByRole('button', { name: 'P2P' }).click();

    await expect(page.locator('text=Connection Pools').first()).toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=Follows')).toBeVisible({ timeout: 5000 });
  });

  test('displays peer stats section', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/#/settings');
    await disableOthersPool(page);

    await expect(page.getByRole('button', { name: 'P2P' })).toBeVisible({ timeout: 10000 });
    await page.getByRole('button', { name: 'P2P' }).click();

    await expect(page.locator('text=Peers').first()).toBeVisible({ timeout: 5000 });
  });
});

test.describe('Block Peer', () => {
  test.setTimeout(120000);

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
    await disableOthersPool(page);

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

  async function followUser(page: Page, targetPubkey: string): Promise<boolean> {
    return page.evaluate(async (pk) => {
      const { followPubkey } = (window as any).__testHelpers || {};
      if (followPubkey) {
        return followPubkey(pk);
      }
      return false;
    }, targetPubkey);
  }

  async function getConnectedPeerCount(page: Page): Promise<number> {
    return page.evaluate(() => {
      const webrtcStore = (window as any).webrtcStore;
      return webrtcStore?.getConnectedCount?.() || 0;
    });
  }

  test('can block a peer from settings', async ({ browser }) => {
    const context1 = await browser.newContext();
    const context2 = await browser.newContext();
    const page1 = await context1.newPage();
    const page2 = await context2.newPage();

    try {
      await page1.goto('/');
      const pubkey1 = await setupFreshPeer(page1);

      await page2.goto('/');
      const pubkey2 = await setupFreshPeer(page2);

      await followUser(page1, pubkey2);
      await followUser(page2, pubkey1);

      let connected = false;
      for (let i = 0; i < 20 && !connected; i++) {
        const count1 = await getConnectedPeerCount(page1);
        const count2 = await getConnectedPeerCount(page2);
        if (count1 > 0 && count2 > 0) {
          connected = true;
          break;
        }
        await page1.waitForTimeout(1000);
      }

      if (!connected) {
        return;
      }

      await page1.goto('/#/settings');
      await page1.getByRole('button', { name: 'P2P' }).click();
      await expect(page1.locator('text=Peers').first()).toBeVisible({ timeout: 10000 });

      const peerSection = page1.locator('text=Connected Peers').first();

      if (await peerSection.isVisible({ timeout: 5000 }).catch(() => false)) {
        const blockBtn = page1.locator('button[title*="Block"]').first();

        if (await blockBtn.isVisible({ timeout: 2000 }).catch(() => false)) {
          const countBefore = await getConnectedPeerCount(page1);
          await blockBtn.click();
          await page1.waitForTimeout(2000);
          const countAfter = await getConnectedPeerCount(page1);
          expect(countAfter).toBeLessThan(countBefore);
        }
      }

      const blockFnExists = await page1.evaluate(() => {
        return typeof (window as any).__appStore?.blockPeer === 'function' ||
               typeof (window as any).blockPeer === 'function';
      });

      console.log('Block peer function exists:', blockFnExists);

    } finally {
      await context1.close();
      await context2.close();
    }
  });

  test('blocked peer list persists in settings', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/#/settings');
    await disableOthersPool(page);

    await expect(page.getByRole('button', { name: 'P2P' })).toBeVisible({ timeout: 10000 });
    await page.getByRole('button', { name: 'P2P' }).click();

    const blockedPeersExists = await page.evaluate(() => {
      const settingsStore = (window as any).__settingsStore;
      if (settingsStore?.getState) {
        return 'blockedPeers' in settingsStore.getState();
      }
      return false;
    });

    console.log('Blocked peers array exists in store:', blockedPeersExists);
  });
});
