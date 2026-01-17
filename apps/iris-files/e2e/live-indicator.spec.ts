import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, goToTreeList } from './test-utils.js';

// Helper to create tree and navigate into it
async function createAndEnterTree(page: any, name: string) {
  await goToTreeList(page);
  await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 10000 });
  await page.getByRole('button', { name: 'New Folder' }).click();
  await page.locator('input[placeholder="Folder name..."]').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 10000 });
}

// Helper to create a file
async function createFile(page: any, name: string, content: string = '') {
  await page.getByRole('button', { name: /File/ }).first().click();
  await page.locator('input[placeholder="File name..."]').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
  if (content) {
    await page.locator('textarea').fill(content);
    await page.getByRole('button', { name: 'Save' }).click();
    await page.waitForTimeout(500);
  }
  await page.getByRole('button', { name: 'Done' }).click();
  await page.waitForTimeout(500);
}

test.describe('LIVE Indicator', () => {
  test.setTimeout(60000);

  test.beforeEach(async ({ page }) => {
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
  });

  test('should show LIVE badge when file is recently changed', async ({ page }) => {
    // Create tree with file
    await createAndEnterTree(page, 'live-test');

    // Create file - after clicking Done, it should auto-navigate to the file
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('test.txt');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('Hello World');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.waitForTimeout(300);
    await page.getByRole('button', { name: 'Done' }).click();

    // Should now be viewing the file - check for LIVE badge immediately
    await expect(page.locator('.animate-pulse:has-text("LIVE")')).toBeVisible({ timeout: 3000 });
  });

  test('should hide LIVE badge after timeout', async ({ page }) => {
    // Create tree with file
    await createAndEnterTree(page, 'live-timeout-test');

    // Create file - stay on it immediately
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('timeout.txt');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('Test');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.waitForTimeout(300);
    await page.getByRole('button', { name: 'Done' }).click();

    // LIVE badge should be visible initially
    await expect(page.locator('.animate-pulse:has-text("LIVE")')).toBeVisible({ timeout: 3000 });

    // Wait for timeout (5 seconds + buffer)
    await page.waitForTimeout(6000);

    // LIVE badge should be hidden
    await expect(page.locator('.animate-pulse:has-text("LIVE")')).not.toBeVisible();
  });
});
