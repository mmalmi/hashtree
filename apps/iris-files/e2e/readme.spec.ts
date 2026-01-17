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

test.describe('README Panel', () => {
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

  test('should display README.md content in directory view', async ({ page }) => {
    // Create tree with README
    await createAndEnterTree(page, 'readme-test');
    await createFile(page, 'README.md', '# Hello World\n\nThis is a test readme.');

    // Navigate back to tree to see the readme panel
    await goToTreeList(page);
    await page.locator('a:has-text("readme-test")').first().click();
    await page.waitForTimeout(1000);

    // Check that README panel is visible with rendered content
    // The panel has a header with book-open icon and "README.md" text
    await expect(page.locator('.i-lucide-book-open')).toBeVisible();
    await expect(page.locator('text=Hello World')).toBeVisible();
    await expect(page.locator('text=This is a test readme')).toBeVisible();
  });

  test('should have edit button for README when user can edit', async ({ page }) => {
    // Create tree with README
    await createAndEnterTree(page, 'readme-edit-test');
    await createFile(page, 'README.md', '# Editable');

    // Navigate back to tree
    await goToTreeList(page);
    await page.locator('a:has-text("readme-edit-test")').first().click();
    await page.waitForTimeout(1000);

    // Check edit button exists in README panel
    await expect(page.locator('.i-lucide-book-open')).toBeVisible();
    // Edit button should be in the README panel header
    await expect(page.getByRole('button', { name: 'Edit' })).toBeVisible();
  });
});
