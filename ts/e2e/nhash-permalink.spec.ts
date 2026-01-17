/**
 * E2E test for nhash file permalinks
 * Tests direct navigation to /nhash1.../filename URLs
 *
 * Two browsers are used:
 * - Browser 1: Creates content and seeds it (stays open)
 * - Browser 2: Navigates directly to the permalink URL
 */
import { test, expect, type Page } from './fixtures';
import path from 'path';
import fs from 'fs';
import os from 'os';
import {
  setupPageErrorHandler,
  waitForAppReady,
  navigateToPublicFolder,
  goToTreeList,
  createFolder,
  disableOthersPool,
  followUser,
  waitForFollowInWorker,
} from './test-utils.js';

// Helper to create a temp file and upload it
async function uploadTempFile(page: Page, name: string, content: string | Buffer) {
  const tmpDir = os.tmpdir();
  const filePath = path.join(tmpDir, name);
  fs.writeFileSync(filePath, content);
  const fileInput = page.locator('input[type="file"]').first();
  await fileInput.setInputFiles(filePath);
  fs.unlinkSync(filePath);
}

async function initUser(page: Page): Promise<{ npub: string; pubkeyHex: string }> {
  setupPageErrorHandler(page);
  await page.goto('http://localhost:5173');
  await disableOthersPool(page);
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

async function createTextFile(page: Page, fileName: string, content: string): Promise<void> {
  await page.getByRole('button', { name: 'New File' }).click();
  const filenameInput = page.locator('input[placeholder="File name..."]');
  await expect(filenameInput).toBeVisible({ timeout: 10000 });
  await filenameInput.fill(fileName);
  await page.getByRole('button', { name: 'Create' }).click();

  const editor = page.locator('textarea');
  await expect(editor).toBeVisible({ timeout: 10000 });
  await editor.fill(content);

  const saveButton = page.getByRole('button', { name: /Save|Saved|Saving/ });
  if (await saveButton.isEnabled().catch(() => false)) {
    await saveButton.click();
  }
  await expect(saveButton).toBeDisabled({ timeout: 30000 });

  await page.getByRole('button', { name: 'Done' }).click();
  await expect(editor).not.toBeVisible({ timeout: 30000 });
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

test.describe('nhash file permalinks', () => {
  // Increase timeout for WebRTC content transfer tests
  test.setTimeout(60000);

  test('should display file content when navigating directly to nhash permalink URL', async ({ browser }) => {
    test.slow();
    // Browser 1: Create content and seed it
    const context1 = await browser.newContext();
    const page1 = await context1.newPage();

    const user1 = await initUser(page1);

    // Browser 2: Initialize second user
    const context2 = await browser.newContext();
    const page2 = await context2.newPage();
    const user2 = await initUser(page2);

    // Follow each other for reliable WebRTC connection via follows pool
    await followUser(page1, user2.npub);
    await waitForFollowInWorker(page1, user2.pubkeyHex);
    await followUser(page2, user1.npub);
    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    // Create a new tree for testing (more reliable than using public folder)
    await goToTreeList(page1);
    await createFolder(page1, 'permalink-test');
    await expect(page1).toHaveURL(/permalink-test/, { timeout: 30000 });
    await expect(page1.getByRole('button', { name: 'New File' })).toBeVisible({ timeout: 30000 });

    await createTextFile(page1, 'test-permalink.txt', 'Hello from permalink test!');

    // Find the Permalink link in preview (use exact match to avoid matching tree/file names containing "permalink")
    const permalinkLink = page1.getByRole('link', { name: 'Permalink', exact: true });
    await expect(permalinkLink).toBeVisible({ timeout: 15000 });

    // Get the href from the permalink
    const permalinkHref = await permalinkLink.getAttribute('href');
    expect(permalinkHref).toBeTruthy();
    // HashRouter URLs start with #/
    expect(permalinkHref).toMatch(/^#\/nhash1/);

    // Construct full URL (href already includes #)
    const permalinkUrl = `http://localhost:5173/${permalinkHref}`;
    console.log('Permalink URL:', permalinkUrl);

    // Navigate directly to the permalink URL
    await page2.goto(permalinkUrl);
    await disableOthersPool(page2);
    await waitForAppReady(page2);

    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    // Wait for content to load (browser 1 should be seeding via WebRTC)
    await expect(page2.getByRole('button', { name: 'Download' })).toBeVisible({ timeout: 30000 });

    // The file content should be visible in preview
    await expect(page2.getByText('Hello from permalink test!')).toBeVisible({ timeout: 30000 });

    // Cleanup
    await context1.close();
    await context2.close();
  });

  test('should display directory content when navigating directly to nhash directory URL', async ({ browser }) => {
    test.slow();
    // Browser 1: Create content and seed it
    const context1 = await browser.newContext();
    const page1 = await context1.newPage();

    const user1 = await initUser(page1);

    const context2 = await browser.newContext();
    const page2 = await context2.newPage();
    const user2 = await initUser(page2);

    // Follow each other for reliable WebRTC connection via follows pool
    await followUser(page1, user2.npub);
    await waitForFollowInWorker(page1, user2.pubkeyHex);
    await followUser(page2, user1.npub);
    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    // Create a new tree for testing
    await goToTreeList(page1);
    await createFolder(page1, 'dir-permalink-test');
    await expect(page1).toHaveURL(/dir-permalink-test/, { timeout: 30000 });

    // Upload a file
    await uploadTempFile(page1, 'dir-test.txt', 'Directory test content');
    const fileList = page1.getByTestId('file-list');
    await expect(fileList.getByText('dir-test.txt')).toBeVisible({ timeout: 15000 });

    const backLink = page1.getByRole('link', { name: 'Back to folder' });
    await backLink.waitFor({ timeout: 5000 }).then(async () => {
      await backLink.click();
      await expect(page1.getByTestId('file-list')).toBeVisible({ timeout: 15000 });
    }).catch(() => {});

    // Get the directory permalink from folder actions
    const dirPermalinkLink = page1.locator('[data-testid="permalink-link"]:visible').first();
    await expect(dirPermalinkLink).toBeVisible({ timeout: 10000 });

    const dirPermalinkHref = await dirPermalinkLink.getAttribute('href');
    expect(dirPermalinkHref).toBeTruthy();
    // HashRouter URLs start with #/
    expect(dirPermalinkHref).toMatch(/^#\/nhash1/);

    // Construct full URL (href already includes #)
    const dirPermalinkUrl = `http://localhost:5173/${dirPermalinkHref}`;
    console.log('Directory Permalink URL:', dirPermalinkUrl);

    // Browser 2: Navigate directly to the directory permalink
    await page2.goto(dirPermalinkUrl);
    await disableOthersPool(page2);
    await waitForAppReady(page2);

    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    // Wait for directory listing to appear
    const fileList2 = page2.getByTestId('file-list');
    await expect(fileList2.getByText('dir-test.txt')).toBeVisible({ timeout: 30000 });

    // Cleanup
    await context1.close();
    await context2.close();
  });
});
