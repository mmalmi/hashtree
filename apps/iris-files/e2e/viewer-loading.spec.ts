import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, goToTreeList, disableOthersPool, configureBlossomServers } from './test-utils.js';

// Helper to create tree and navigate into it
async function createAndEnterTree(page: any, name: string) {
  await goToTreeList(page);
  await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 30000 });
  await page.getByRole('button', { name: 'New Folder' }).click();
  await page.locator('input[placeholder="Folder name..."]').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 30000 });
}

// Helper to create a file
async function createFile(page: any, name: string, content: string = '') {
  await page.getByRole('button', { name: /File/ }).first().click();
  await page.locator('input[placeholder="File name..."]').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 30000 });
  if (content) {
    await page.locator('textarea').fill(content);
    await page.getByRole('button', { name: 'Save' }).click();
    await page.waitForTimeout(500);
  }
  await page.getByRole('button', { name: 'Done' }).click();
  await page.waitForTimeout(500);
}

test.describe('Viewer Loading Indicator', () => {
  test.setTimeout(60000);

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await configureBlossomServers(page);

    // Clear storage for fresh state (including OPFS)
    await page.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();

      // Clear OPFS
      try {
        const root = await navigator.storage.getDirectory();
        for await (const name of root.keys()) {
          await root.removeEntry(name, { recursive: true });
        }
      } catch {
        // OPFS might not be available
      }
    });

    await page.reload();
    await page.waitForTimeout(500);
    await disableOthersPool(page);
    await configureBlossomServers(page);
    // Page ready - navigateToPublicFolder handles waiting
    await navigateToPublicFolder(page);
  });

  test('should NOT show loading indicator for fast file loads', async ({ page }) => {
    // Create a tree with a simple file
    await createAndEnterTree(page, 'fast-load-test');
    await createFile(page, 'quick.txt', 'Quick content');

    // Go back and click on the file
    await page.locator('a:has-text("fast-load-test")').first().click();
    await page.waitForTimeout(500);
    await page.locator('a:has-text("quick.txt")').first().click();

    // Wait a moment - loading should NOT show (file loads quickly)
    await page.waitForTimeout(500);

    // Loading indicator should NOT be visible (fast load)
    const loadingIndicator = page.getByTestId('loading-indicator');
    await expect(loadingIndicator).not.toBeVisible();

    // Content should be visible
    await expect(page.locator('pre')).toContainText('Quick content', { timeout: 5000 });
  });

  test('should show file content without flashing loading indicator', async ({ page }) => {
    await createAndEnterTree(page, 'no-flash-test');
    await createFile(page, 'content.txt', 'Test file content here');

    await page.locator('a:has-text("no-flash-test")').first().click();
    await page.waitForTimeout(500);
    
    // Click on file and immediately check - loading should not appear for instant loads
    await page.locator('a:has-text("content.txt")').first().click();
    
    // Short wait - content should appear without loading flash
    await page.waitForTimeout(200);
    
    // The loading indicator shouldn't be visible
    const loadingIndicator = page.getByTestId('loading-indicator');
    const isLoadingVisible = await loadingIndicator.isVisible().catch(() => false);
    expect(isLoadingVisible).toBe(false);

    // Content should be there
    await expect(page.locator('pre')).toContainText('Test file content here', { timeout: 5000 });
  });
});
