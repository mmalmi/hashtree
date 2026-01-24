import { test, expect, type Route } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, configureBlossomServers } from './test-utils.js';
import * as fs from 'fs';
import * as path from 'path';
import * as os from 'os';

test.describe('Blossom Push', () => {
  test('can open push modal', { timeout: 60000 }, async ({ page }) => {
    test.setTimeout(90000);
    setupPageErrorHandler(page);

    // Go to home page first (creates user session)
    await page.goto('/');
    await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests
    await configureBlossomServers(page); // Enable Blossom servers for this test

    // Navigate to public folder where we have files
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Wait for Push button to be visible
    const pushBtn = page.getByRole('button', { name: 'Push' }).first();
    await expect(pushBtn).toBeVisible({ timeout: 10000 });

    // Click push button
    await pushBtn.click();

    // Modal should appear
    const modal = page.locator('[data-testid="blossom-push-modal"]');
    await expect(modal).toBeVisible({ timeout: 5000 });

    // Should show modal title
    await expect(modal.locator('text=Push to File Servers')).toBeVisible();

    // Should show the Push button
    await expect(modal.locator('[data-testid="start-push-btn"]')).toBeVisible();

    // Close modal by pressing Escape
    await page.keyboard.press('Escape');
    await expect(modal).not.toBeVisible({ timeout: 5000 });
  });

  test('push modal can be cancelled', { timeout: 60000 }, async ({ page }) => {
    test.setTimeout(90000);
    test.slow();
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await configureBlossomServers(page);

    // Navigate to public folder
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Click push button
    await page.getByRole('button', { name: 'Push' }).first().click();

    // Modal should appear
    const modal = page.locator('[data-testid="blossom-push-modal"]');
    await expect(modal).toBeVisible({ timeout: 5000 });

    // Should show cancel button
    const cancelBtn = modal.locator('button', { hasText: 'Cancel' });
    await expect(cancelBtn).toBeVisible();

    // Close modal
    await cancelBtn.click();
    await expect(modal).not.toBeVisible({ timeout: 5000 });
  });

  test('push modal with mocked server shows progress', { timeout: 90000 }, async ({ page }) => {
    test.slow(); // Network operations can be slow under parallel load
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await configureBlossomServers(page);

    // Track uploaded blobs
    const uploadedBlobs: string[] = [];

    // Mock blossom server responses
    await page.route('**/upload', async (route: Route) => {
      const request = route.request();
      if (request.method() === 'PUT') {
        const hash = request.headers()['x-sha-256'];
        if (hash) {
          uploadedBlobs.push(hash);
        }
        // Simulate successful upload
        await route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({
            sha256: hash,
            size: 100,
            type: 'application/octet-stream',
            uploaded: Date.now(),
          }),
        });
      } else {
        await route.continue();
      }
    });

    // Navigate to public folder
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Create a test file to upload - we need actual content to test multi-block push
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'blossom-test-'));
    const testFilePath = path.join(tmpDir, 'test-content.txt');
    // Create content that will result in multiple merkle tree blocks
    // Each chunk is ~16KB, so create ~50KB of content for multiple chunks
    const testContent = 'Hello from Blossom push test! '.repeat(2000);
    fs.writeFileSync(testFilePath, testContent);

    try {
      // Upload file to the public folder
      const fileInput = page.locator('input[type="file"]:not([webkitdirectory])').first();
      await expect(fileInput).toBeAttached({ timeout: 30000 });
      await fileInput.setInputFiles(testFilePath, { timeout: 60000 });

      // Wait for file to appear in the tree (may be hidden on smaller layouts)
      await expect(page.locator('[data-testid="file-list"] a:has-text("test-content.txt")')).toBeAttached({ timeout: 10000 });

      // Navigate back to folder view without relying on sidebar visibility
      const npub = await page.evaluate(() => (window as any).__nostrStore?.getState?.()?.npub || '');
      await page.evaluate((userNpub) => {
        window.location.hash = `#/${userNpub}/public`;
      }, npub);
      await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });

      // Wait for folder actions to be visible
      await expect(page.getByRole('button', { name: 'Push' }).first()).toBeVisible({ timeout: 10000 });

      // Click push button
      await page.getByRole('button', { name: 'Push' }).first().click();

      // Modal should appear
      const modal = page.locator('[data-testid="blossom-push-modal"]');
      await expect(modal).toBeVisible({ timeout: 5000 });

      // Start push
      const startPushBtn = page.locator('[data-testid="start-push-btn"]');
      await expect(startPushBtn).toBeVisible({ timeout: 5000 });
      await startPushBtn.click();

      // Should show progress or completion
      // Wait for done state (may be quick with mocked server)
      await expect(modal.locator('button', { hasText: 'Done' })).toBeVisible({ timeout: 30000 });

      // Verify uploads - with actual file content we should have multiple blocks:
      // - directory node
      // - file tree node (if chunked)
      // - chunk blobs
      const uniqueHashes = new Set(uploadedBlobs);
      console.log('Uploaded blobs:', uploadedBlobs.length, 'unique:', uniqueHashes.size);
      console.log('Unique hashes:', [...uniqueHashes]);

      // Should have at least 2 unique blocks (directory + file/chunk)
      // Each block is uploaded to 2 servers, so total uploads = uniqueBlocks * 2
      expect(uniqueHashes.size).toBeGreaterThanOrEqual(2);

      // Close modal
      await page.locator('button', { hasText: 'Done' }).click();
      await expect(modal).not.toBeVisible({ timeout: 5000 });
    } finally {
      // Cleanup temp files
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  test('push modal handles upload errors gracefully', { timeout: 90000 }, async ({ page }) => {
    test.setTimeout(90000);
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await configureBlossomServers(page);

    // Mock failing blossom server
    await page.route('**/upload', async (route: Route) => {
      const request = route.request();
      if (request.method() === 'PUT') {
        await route.fulfill({
          status: 500,
          contentType: 'text/plain',
          body: 'Internal Server Error',
        });
      } else {
        await route.continue();
      }
    });

    // Navigate to public folder
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Create a test file so we have something to push
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'blossom-error-test-'));
    const testFilePath = path.join(tmpDir, 'error-test.txt');
    fs.writeFileSync(testFilePath, 'Test content for error handling');

    try {
      // Upload file to the public folder
      const fileInput = page.locator('input[type="file"]:not([webkitdirectory])').first();
      await expect(fileInput).toBeAttached({ timeout: 30000 });
      await fileInput.setInputFiles(testFilePath, { timeout: 60000 });

      // Wait for file to appear in the tree (may be hidden on smaller layouts)
      await expect(page.locator('[data-testid="file-list"] a:has-text("error-test.txt")')).toBeAttached({ timeout: 10000 });

      // Navigate back to folder view without relying on sidebar visibility
      const npub = await page.evaluate(() => (window as any).__nostrStore?.getState?.()?.npub || '');
      await page.evaluate((userNpub) => {
        window.location.hash = `#/${userNpub}/public`;
      }, npub);
      await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });

      // Wait for folder actions to be visible
      await expect(page.getByRole('button', { name: 'Push' }).first()).toBeVisible({ timeout: 10000 });

      // Click push button
      await page.getByRole('button', { name: 'Push' }).first().click();

      // Start push
      const modal = page.locator('[data-testid="blossom-push-modal"]');
      await expect(modal).toBeVisible({ timeout: 5000 });

      await page.locator('[data-testid="start-push-btn"]').click();

      // Wait for completion with errors
      await expect(modal.locator('button', { hasText: 'Done' })).toBeVisible({ timeout: 30000 });

      // Should show error count (Failed label should be visible in stats grid)
      await expect(modal.locator('text=Failed').first()).toBeVisible({ timeout: 10000 });

      // Close modal
      await page.locator('button', { hasText: 'Done' }).click();
      await expect(modal).not.toBeVisible({ timeout: 5000 });
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });
});
