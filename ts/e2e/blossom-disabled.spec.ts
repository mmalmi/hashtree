import { test, expect, type Route } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';
import * as fs from 'fs';
import * as path from 'path';
import * as os from 'os';

test.describe('Blossom Disabled', () => {
  test.describe.configure({ timeout: 90000 });
  test('no blossom requests when all servers are disabled', { timeout: 60000 }, async ({ page }) => {
    setupPageErrorHandler(page);
    // Track all blossom requests
    const blossomRequests: string[] = [];
    await page.route('**/*.iris.to/**', async (route: Route) => {
      const url = route.request().url();
      console.log('[Test] Intercepted blossom request:', url);
      blossomRequests.push(url);
      // Let the request fail - we just want to track it
      await route.abort();
    });

    // Go to home page
    await page.goto('/');
    await disableOthersPool(page);
    // Disable all blossom servers by setting empty array
    await page.evaluate(() => {
      const configure = (window as unknown as { __configureBlossomServers?: (servers: unknown[]) => void }).__configureBlossomServers;
      if (!configure) {
        throw new Error('__configureBlossomServers not found');
      }
      // Disable all servers
      configure([]);
    });

    await page.waitForFunction(() => {
      const settings = (window as any).__settingsStore?.getState?.();
      return settings?.network?.blossomServers?.length === 0;
    }, null, { timeout: 5000 });

    // Navigate to public folder
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Create a test file
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'blossom-disabled-test-'));
    const testFilePath = path.join(tmpDir, 'test-file.txt');
    fs.writeFileSync(testFilePath, 'Test content for disabled blossom servers');

    try {
      // Upload file
      const fileInput = page.locator('input[type="file"]').first();
      await fileInput.setInputFiles(testFilePath);

      // Wait for file to appear in the list
      await expect(page.locator('[data-testid="file-list"] a:has-text("test-file.txt")')).toBeVisible({ timeout: 10000 });

      // Verify no blossom requests were made
      console.log('[Test] Blossom requests made:', blossomRequests);
      expect(blossomRequests.length).toBe(0);
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });

  test('no blossom requests when servers have read/write disabled', { timeout: 60000 }, async ({ page }) => {
    setupPageErrorHandler(page);

    // Track all blossom requests
    const blossomRequests: string[] = [];
    await page.route('**/*.iris.to/**', async (route: Route) => {
      const url = route.request().url();
      console.log('[Test] Intercepted blossom request:', url);
      blossomRequests.push(url);
      await route.abort();
    });

    await page.goto('/');
    await disableOthersPool(page);

    // Configure servers with read/write disabled
    await page.evaluate(() => {
      const configure = (window as unknown as { __configureBlossomServers?: (servers: unknown[]) => void }).__configureBlossomServers;
      if (!configure) {
        throw new Error('__configureBlossomServers not found');
      }
      // Servers exist but with read/write disabled
      configure([
        { url: 'https://upload.iris.to', read: false, write: false },
        { url: 'https://cdn.iris.to', read: false, write: false },
      ]);
    });

    await page.waitForFunction(() => {
      const settings = (window as any).__settingsStore?.getState?.();
      const servers = settings?.network?.blossomServers;
      return Array.isArray(servers)
        && servers.length === 2
        && servers.every((server: { read?: boolean; write?: boolean }) => server.read === false && server.write === false);
    }, null, { timeout: 5000 });

    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'blossom-rw-disabled-test-'));
    const testFilePath = path.join(tmpDir, 'test-file2.txt');
    fs.writeFileSync(testFilePath, 'Test content for disabled read/write');

    try {
      const fileInput = page.locator('input[type="file"]').first();
      await fileInput.setInputFiles(testFilePath);

      await expect(page.locator('[data-testid="file-list"] a:has-text("test-file2.txt")')).toBeVisible({ timeout: 10000 });

      console.log('[Test] Blossom requests made:', blossomRequests);
      expect(blossomRequests.length).toBe(0);
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });
});
