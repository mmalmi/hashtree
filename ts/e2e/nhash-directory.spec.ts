/**
 * E2E test for nhash directory navigation
 *
 * Tests navigating to an nhash URL that points to a directory
 * and verifying that directory contents are shown.
 *
 * This test helps debug issues where nhash navigation shows empty directories.
 */
import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, useLocalRelay, waitForAppReady, followUser, waitForFollowInWorker } from './test-utils.js';

async function initUser(page: Page): Promise<{ npub: string; pubkeyHex: string }> {
  setupPageErrorHandler(page);
  await page.goto('http://localhost:5173');
  await disableOthersPool(page);
  await useLocalRelay(page);
  await waitForAppReady(page);
  await navigateToPublicFolder(page);

  await page.waitForFunction(() => (window as any).__getMyPubkey?.(), { timeout: 15000 });
  const pubkeyHex = await page.evaluate(() => (window as any).__getMyPubkey?.() ?? null);
  const url = page.url();
  const npubMatch = url.match(/npub1[a-z0-9]+/);
  if (!pubkeyHex || !npubMatch) {
    throw new Error('Could not determine user identity');
  }
  return { npub: npubMatch[0], pubkeyHex };
}

async function waitForPeerConnection(page: Page, pubkeyHex: string, timeoutMs: number = 30000): Promise<void> {
  await page.waitForFunction(
    async (pk: string) => {
      const adapter = (window as any).__workerAdapter;
      if (!adapter) return false;
      const stats = await adapter.getPeerStats();
      return stats.some((peer: { connected?: boolean; pubkey?: string }) => peer.connected && peer.pubkey === pk);
    },
    pubkeyHex,
    { timeout: timeoutMs, polling: 500 }
  );
}

test.describe('nhash directory navigation', () => {
  // Longer timeout for content fetching
  test.setTimeout(60000);

  test('direct npub/path navigation loads own content', async ({ page }) => {
    // Test direct navigation to own tree (after creating content)
    // This tests the race condition fix for npub/path routes
    test.slow();
    setupPageErrorHandler(page);

    await page.goto('/');
    await disableOthersPool(page);
    await useLocalRelay(page);

    // Navigate to public folder and create content
    await navigateToPublicFolder(page);

    // Create a test folder with files
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('direct-nav-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for folder to appear
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'direct-nav-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });

    // Navigate into folder and add files
    await folderLink.click();
    await page.waitForURL(/direct-nav-test/, { timeout: 10000 });

    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content1 = new TextEncoder().encode('Direct nav test file');
      const { cid: cid1, size: size1 } = await tree.putFile(content1);
      rootCid = await tree.setEntry(rootCid, route.path, 'test-file.txt', cid1, size1, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'test-file.txt' })).toBeVisible({ timeout: 10000 });

    // Get current URL to navigate to later
    const currentUrl = page.url();
    console.log('[test] Created content at:', currentUrl);

    // Navigate away to home
    await page.locator('header a:has-text("Iris")').click();
    await page.waitForTimeout(500);

    // Direct navigate back using the full npub/treeName/path URL
    await page.goto(currentUrl);

    // Should resolve and show the file
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'test-file.txt' })).toBeVisible({ timeout: 15000 });
    console.log('[test] SUCCESS: Direct npub/path navigation worked');
  });

  test('same context - nhash navigation resolves after worker ready', async ({ page }) => {
    // This test verifies the race condition fix by navigating to an nhash
    // and checking that currentDirCid eventually resolves
    test.slow();
    setupPageErrorHandler(page);

    await page.goto('/');
    await disableOthersPool(page);
    await useLocalRelay(page);

    // Navigate to public folder and create content
    await navigateToPublicFolder(page);

    // Create a test folder via UI
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('nhash-test-folder');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into the folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'nhash-test-folder' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/nhash-test-folder/, { timeout: 10000 });

    // Create test files via API
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content1 = new TextEncoder().encode('File 1 content');
      const { cid: cid1, size: size1 } = await tree.putFile(content1);
      rootCid = await tree.setEntry(rootCid, route.path, 'file1.txt', cid1, size1, LinkType.Blob);

      const content2 = new TextEncoder().encode('File 2 content');
      const { cid: cid2, size: size2 } = await tree.putFile(content2);
      rootCid = await tree.setEntry(rootCid, route.path, 'file2.txt', cid2, size2, LinkType.Blob);

      autosaveIfOwn(rootCid);
    });

    // Files should be visible
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'file1.txt' })).toBeVisible({ timeout: 10000 });
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'file2.txt' })).toBeVisible({ timeout: 10000 });

    // Get the folder's nhash
    const nhash = await page.evaluate(async () => {
      const { currentDirCidStore } = await import('/src/stores/index.ts');
      const hashtree = await import('/node_modules/hashtree/dist/index.js');

      let dirCid: { hash: Uint8Array; key?: Uint8Array } | null = null;
      const unsub = currentDirCidStore.subscribe((v: { hash: Uint8Array; key?: Uint8Array } | null) => { dirCid = v; });
      unsub();

      if (!dirCid) return null;
      return hashtree.nhashEncode(dirCid);
    });

    expect(nhash).toBeTruthy();
    console.log('[test] Directory nhash:', nhash);

    // Navigate to home first
    await page.locator('header a:has-text("Iris")').click();
    await page.waitForFunction(
      () => window.location.hash === '' || window.location.hash === '#/' || window.location.hash === '#',
      { timeout: 15000 }
    );

    // Now navigate directly to the nhash URL (simulates direct navigation)
    await page.goto(`http://localhost:5173/#/${nhash}`);

    // Wait for resolution (same context, data is in IndexedDB)
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'file1.txt' })).toBeVisible({ timeout: 15000 });
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'file2.txt' })).toBeVisible({ timeout: 15000 });

    console.log('[test] SUCCESS: nhash navigation in same context works');
  });

  test('cross context - nhash navigation resolves via WebRTC', async ({ browser }) => {
    test.slow();

    const context1 = await browser.newContext();
    const page1 = await context1.newPage();
    const user1 = await initUser(page1);

    // Create a test folder and files
    await page1.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page1.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('nhash-test-dir');
    await page1.click('button:has-text("Create")');
    await expect(page1.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into the folder
    const folderLink = page1.locator('[data-testid="file-list"] a').filter({ hasText: 'nhash-test-dir' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page1.waitForURL(/nhash-test-dir/, { timeout: 10000 });

    // Create files via tree API
    await page1.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      // Add test files
      const content1 = new TextEncoder().encode('File 1 content');
      const { cid: cid1, size: size1 } = await tree.putFile(content1);
      rootCid = await tree.setEntry(rootCid, route.path, 'file1.txt', cid1, size1, LinkType.Blob);

      const content2 = new TextEncoder().encode('File 2 content');
      const { cid: cid2, size: size2 } = await tree.putFile(content2);
      rootCid = await tree.setEntry(rootCid, route.path, 'file2.txt', cid2, size2, LinkType.Blob);

      autosaveIfOwn(rootCid);
    });

    // Wait for files to appear
    await expect(page1.locator('[data-testid="file-list"] a').filter({ hasText: 'file1.txt' })).toBeVisible({ timeout: 15000 });
    await expect(page1.locator('[data-testid="file-list"] a').filter({ hasText: 'file2.txt' })).toBeVisible({ timeout: 15000 });

    // Get the directory nhash permalink
    const nhash = await page1.evaluate(async () => {
      const { currentDirCidStore } = await import('/src/stores/index.ts');
      const hashtree = await import('/node_modules/hashtree/dist/index.js');

      // Use store's subscribe synchronously
      let dirCid: { hash: Uint8Array; key?: Uint8Array } | null = null;
      const unsub = currentDirCidStore.subscribe((v: { hash: Uint8Array; key?: Uint8Array } | null) => { dirCid = v; });
      unsub();

      if (!dirCid) return null;

      return hashtree.nhashEncode(dirCid);
    });

    expect(nhash).toBeTruthy();
    console.log('[test] Directory nhash:', nhash);

    const context2 = await browser.newContext();
    const page2 = await context2.newPage();
    const user2 = await initUser(page2);

    // Follow each other to enable WebRTC via follows pool
    await followUser(page1, user2.npub);
    await waitForFollowInWorker(page1, user2.pubkeyHex);
    await followUser(page2, user1.npub);
    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    // Navigate to nhash URL in the second context
    const nhashUrl = `http://localhost:5173/#/${nhash}`;
    console.log('[test] Navigating to nhash URL:', nhashUrl);
    await page2.goto(nhashUrl);
    await disableOthersPool(page2);
    await waitForAppReady(page2);

    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    // Should show directory listing with both files
    await expect(page2.locator('[data-testid="file-list"] a').filter({ hasText: 'file1.txt' })).toBeVisible({ timeout: 30000 });
    await expect(page2.locator('[data-testid="file-list"] a').filter({ hasText: 'file2.txt' })).toBeVisible({ timeout: 30000 });

    console.log('[test] SUCCESS: nhash directory navigation worked');

    await context2.close();
    await context1.close();
  });

  test('can paste nhash in search input to navigate', async ({ page }) => {
    test.slow();
    setupPageErrorHandler(page);

    await page.goto('/');
    await disableOthersPool(page);
    await useLocalRelay(page);

    // Wait for app to load
    await expect(page.locator('header').first()).toBeVisible({ timeout: 30000 });

    // Create a test file first to get an nhash
    await navigateToPublicFolder(page);

    // Create a test file
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content = new TextEncoder().encode('Search test content');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'search-test.txt', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    // Wait for file to appear
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'search-test.txt' })).toBeVisible({ timeout: 15000 });

    // Get current directory nhash
    const nhash = await page.evaluate(async () => {
      const { currentDirCidStore } = await import('/src/stores/index.ts');
      const hashtree = await import('/node_modules/hashtree/dist/index.js');

      // Use store's subscribe synchronously
      let dirCid: { hash: Uint8Array; key?: Uint8Array } | null = null;
      const unsub = currentDirCidStore.subscribe((v: { hash: Uint8Array; key?: Uint8Array } | null) => { dirCid = v; });
      unsub();

      if (!dirCid) return null;
      return hashtree.nhashEncode(dirCid);
    });

    expect(nhash).toBeTruthy();
    console.log('[test] Got nhash:', nhash);

    // Go to home first
    await page.locator('header a:has-text("Iris")').click();
    await page.waitForFunction(
      () => window.location.hash === '' || window.location.hash === '#/' || window.location.hash === '#',
      { timeout: 15000 }
    );

    // Find search input and paste nhash
    const searchInput = page.locator('input[type="search"], input[placeholder*="Search"]').first();
    await expect(searchInput).toBeVisible({ timeout: 5000 });
    await searchInput.fill(nhash!);
    await searchInput.press('Enter');

    // Should navigate to the nhash and show directory contents
    await page.waitForURL(new RegExp(nhash!.slice(0, 20)), { timeout: 10000 });
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'search-test.txt' })).toBeVisible({ timeout: 15000 });

    console.log('[test] SUCCESS: Search input navigation worked');
  });
});
