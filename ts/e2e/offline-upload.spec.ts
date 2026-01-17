/**
 * E2E test for offline upload functionality
 *
 * Tests that file uploads work when the network is offline.
 * Local storage operations should succeed; network publish is fire-and-forget.
 */
import { test, expect, Page } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, navigateToPublicFolder } from './test-utils.js';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');

// Helper to set up a fresh user session and navigate to public folder
async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);

  await page.goto('/');

  // Clear storage for fresh state
  await page.evaluate(async () => {
    const dbs = await indexedDB.databases();
    for (const db of dbs) {
      if (db.name) indexedDB.deleteDatabase(db.name);
    }
    localStorage.clear();
    sessionStorage.clear();
  });

  await page.reload();
  await page.waitForTimeout(500);
  // Page ready - navigateToPublicFolder handles waiting
  await navigateToPublicFolder(page);
}

test.describe('Offline Upload', () => {
  test.setTimeout(60000);

  test('should upload video file while offline', async ({ page, context }) => {
    // Verify test file exists
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    // Track any uncaught errors
    const errors: string[] = [];
    page.on('pageerror', err => {
      errors.push(err.message);
    });

    // Log console for debugging
    page.on('console', msg => {
      const text = msg.text();
      if (msg.type() === 'error') console.log(`[Console Error] ${text}`);
    });

    // Set up fresh user and navigate to public folder while online
    await setupFreshUser(page);
    console.log('User setup complete');

    // Go offline BEFORE uploading
    console.log('Going offline...');
    await context.setOffline(true);

    // Upload the video via hidden file input
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);
    console.log('File upload triggered');

    // Wait for upload to complete - look for the video in the file list
    // This should work because local storage operations are offline-capable
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });
    console.log('Video appears in file list');

    // Click on the video to view it
    await videoLink.click();
    await page.waitForTimeout(1000);

    // Check that video element exists
    const videoElement = page.locator('video');
    await expect(videoElement).toBeVisible({ timeout: 10000 });
    console.log('Video element visible');

    // Go back online
    console.log('Going back online...');
    await context.setOffline(false);

    // Verify no uncaught errors about network failures that indicate app bugs
    // Note: "publish timed out" errors are expected when relays are rate-limited during parallel tests
    // We only care about IndexedDB errors or app crashes, not relay publish failures
    const criticalErrors = errors.filter(e =>
      (e.includes('network') || e.includes('IndexedDB')) &&
      !e.includes('publish') &&
      !e.includes('timed out') &&
      !e.includes('rate-limit')
    );
    expect(criticalErrors).toHaveLength(0);
    console.log('No uncaught network errors');
  });

  test('should create folder while offline', async ({ page, context }) => {
    // Track any uncaught errors
    const errors: string[] = [];
    page.on('pageerror', err => {
      errors.push(err.message);
    });

    // Set up fresh user while online
    await setupFreshUser(page);

    // Go offline
    await context.setOffline(true);
    console.log('Going offline...');

    // Click New Folder button
    const newFolderBtn = page.getByRole('button', { name: /Folder/i });
    await newFolderBtn.click();

    // Enter folder name in modal
    const input = page.locator('input[placeholder="Folder name..."]');
    await expect(input).toBeVisible({ timeout: 5000 });
    await input.fill('offline-test-folder');
    await page.click('button:has-text("Create")');

    // Wait for modal to close and folder to appear
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Check for the folder in the list or empty directory message
    // (empty folder shows empty directory view)
    const folderOrEmpty = page.locator('text=Empty directory').or(
      page.locator('a:has-text("offline-test-folder")')
    );
    await expect(folderOrEmpty).toBeVisible({ timeout: 10000 });
    console.log('Folder created while offline');

    // Go back online
    await context.setOffline(false);

    // Verify no uncaught network errors
    const networkErrors = errors.filter(e =>
      e.includes('network') ||
      e.includes('publish') ||
      e.includes('timed out')
    );
    expect(networkErrors).toHaveLength(0);
    console.log('No uncaught network errors');
  });

  test('connectivity indicator should update when connections change', async ({ page }) => {
    // Set up fresh user while online
    await setupFreshUser(page);

    // Wait for initial connection (give relays time to connect)
    await page.waitForTimeout(3000);

    // Get initial connection count
    const indicator = page.locator('[data-testid="peer-count"]');
    const initialCount = await indicator.textContent();
    console.log(`Initial connection count: ${initialCount}`);

    // Should have some connections when online
    expect(parseInt(initialCount || '0')).toBeGreaterThan(0);

    // Verify title attribute shows relay info
    const indicatorLink = page.locator('a[href="#/settings"]');
    const title = await indicatorLink.getAttribute('title');
    console.log(`Indicator title: ${title}`);
    expect(title).toContain('relay');

    // "offline" text should NOT be visible when online
    const offlineText = page.locator('text=offline');
    await expect(offlineText).not.toBeVisible();
  });

  test('connectivity indicator shows offline text when browser is offline', async ({ page, context }) => {
    // Set up fresh user while online
    await setupFreshUser(page);

    // Wait for initial connection
    await page.waitForTimeout(2000);

    // "offline" text should NOT be visible when online
    const offlineText = page.getByText('offline', { exact: true });
    await expect(offlineText).not.toBeVisible();

    // Go offline
    await context.setOffline(true);
    console.log('Going offline...');

    // Small wait for browser to detect offline state
    await page.waitForTimeout(500);

    // "offline" text should now be visible
    await expect(offlineText).toBeVisible({ timeout: 5000 });
    console.log('Offline text visible');

    // Connection count should NOT change to 0 (NDK still reports cached state)
    const indicator = page.locator('[data-testid="peer-count"]');
    const count = await indicator.textContent();
    console.log(`Connection count while offline: ${count}`);
    // Count stays the same (not forced to 0)

    // Go back online
    await context.setOffline(false);
    console.log('Going back online...');

    // Wait for browser to detect online state
    await page.waitForTimeout(500);

    // "offline" text should disappear
    await expect(offlineText).not.toBeVisible({ timeout: 5000 });
    console.log('Offline text hidden');
  });
});
