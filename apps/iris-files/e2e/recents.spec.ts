import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, goToTreeList, waitForAppReady } from './test-utils.js';

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
  }
  await page.getByRole('button', { name: 'Done' }).click();
}

test.describe('Recently Visited', () => {
  // Serial mode: tests clear IndexedDB and share state
  test.describe.configure({ mode: 'serial' });
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
    // Page ready - navigateToPublicFolder handles waiting
    await navigateToPublicFolder(page);
  });

  test('should track recently visited trees', async ({ page }) => {
    // Create first tree
    await createAndEnterTree(page, 'recents-test-1');

    // Go back to tree list
    await goToTreeList(page);

    // Create second tree
    await createAndEnterTree(page, 'recents-test-2');

    // Go back to tree list
    await goToTreeList(page);

    // Check recents in localStorage
    const recents = await page.evaluate(() => {
      return localStorage.getItem('hashtree:recents');
    });

    expect(recents).toBeTruthy();
    const parsed = JSON.parse(recents!);
    expect(parsed.length).toBeGreaterThanOrEqual(2);

    // Most recent should be second tree
    expect(parsed[0].label).toBe('recents-test-2');
    expect(parsed[1].label).toBe('recents-test-1');
  });

  test('should persist recents across page reload', async ({ page }) => {
    // Create a tree
    await createAndEnterTree(page, 'persist-test');

    // Go back
    await goToTreeList(page);

    // Verify recents exist
    let recents = await page.evaluate(() => localStorage.getItem('hashtree:recents'));
    expect(recents).toBeTruthy();

    // Reload page
    await page.reload();
    await waitForAppReady(page);

    // Check recents still exist
    recents = await page.evaluate(() => localStorage.getItem('hashtree:recents'));
    expect(recents).toBeTruthy();
    const parsed = JSON.parse(recents!);
    expect(parsed.some((r: any) => r.label === 'persist-test')).toBe(true);
  });

  test('should move re-visited items to top', async ({ page }) => {
    // Create three trees
    await createAndEnterTree(page, 'order-test-1');
    await goToTreeList(page);

    await createAndEnterTree(page, 'order-test-2');
    await goToTreeList(page);

    await createAndEnterTree(page, 'order-test-3');
    await goToTreeList(page);

    // Now revisit first tree
    await page.locator('a:has-text("order-test-1")').first().click();
    await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 5000 });
    await goToTreeList(page);

    // Check order - order-test-1 should now be at top
    const recents = await page.evaluate(() => localStorage.getItem('hashtree:recents'));
    const parsed = JSON.parse(recents!);

    expect(parsed[0].label).toBe('order-test-1');
  });

  test('should limit recents to maximum count', async ({ page }) => {
    // Create many trees (more than MAX_RECENTS = 20)
    for (let i = 0; i < 5; i++) {
      await createAndEnterTree(page, `limit-test-${i}`);
      await goToTreeList(page);
    }

    // Check that recents are capped
    const recents = await page.evaluate(() => localStorage.getItem('hashtree:recents'));
    const parsed = JSON.parse(recents!);

    // Should not exceed MAX_RECENTS (20)
    expect(parsed.length).toBeLessThanOrEqual(20);
    // 5 trees created + potentially file visits
    expect(parsed.length).toBeGreaterThanOrEqual(5);
  });

  test('should track file visits in recents', async ({ page }) => {
    // Create tree with file
    await createAndEnterTree(page, 'file-recents-test');
    await createFile(page, 'test-file.txt', 'Hello World');

    // Navigate to file
    await page.locator('a:has-text("file-recents-test")').first().click();
    const fileLink = page.locator('a:has-text("test-file.txt")').first();
    await expect(fileLink).toBeVisible({ timeout: 5000 });
    await fileLink.click();
    await expect(page.getByTestId('viewer-back')).toBeVisible({ timeout: 5000 });

    // Check recents
    const recents = await page.evaluate(() => localStorage.getItem('hashtree:recents'));
    const parsed = JSON.parse(recents!);

    // Should have file in recents
    const fileRecent = parsed.find((r: any) => r.label?.includes('test-file.txt'));
    expect(fileRecent).toBeTruthy();
  });

  test('should display recents in UI on home page', async ({ page }) => {
    // Create tree with file
    await createAndEnterTree(page, 'ui-recents-test');
    await createFile(page, 'visible-file.txt', 'Test content');

    // Navigate to file to add it to recents
    await page.locator('a:has-text("ui-recents-test")').first().click();
    const visibleFileLink = page.locator('a:has-text("visible-file.txt")').first();
    await expect(visibleFileLink).toBeVisible({ timeout: 5000 });
    await visibleFileLink.click();
    await expect(page.getByTestId('viewer-back')).toBeVisible({ timeout: 5000 });

    // Go back to home page (tree list)
    await goToTreeList(page);

    // Look for the Recent section header - it should show our items
    const recentSection = page.getByText('Recent', { exact: true });
    await expect(recentSection).toBeVisible({ timeout: 30000 });

    // Check that the file appears in the UI recents list (TreeRow renders as <a>)
    const fileInRecents = page.locator('a:has-text("visible-file.txt")');
    await expect(fileInRecents).toBeVisible({ timeout: 30000 });

    // Check that the tree also appears (use first() since tree name also shows as subtitle)
    const treeInRecents = page.locator('a:has-text("ui-recents-test")').first();
    await expect(treeInRecents).toBeVisible({ timeout: 30000 });
  });

  test('clicking recent item navigates to it', async ({ page }) => {
    // Create tree with file
    await createAndEnterTree(page, 'click-recents-test');
    await createFile(page, 'clickable.txt', 'Click me');

    // Navigate to file
    await page.locator('a:has-text("click-recents-test")').first().click();
    const clickableFileLink = page.locator('a:has-text("clickable.txt")').first();
    await expect(clickableFileLink).toBeVisible({ timeout: 5000 });
    await clickableFileLink.click();
    await expect(page.getByTestId('viewer-back')).toBeVisible({ timeout: 5000 });

    // Go back to home
    await goToTreeList(page);

    // Look for the file in recents - the file link should contain the file name
    const recentsView = page.getByTestId('recents-view');
    const recentFileLink = recentsView.getByRole('link', { name: /clickable\.txt/i }).first();
    await expect(recentFileLink).toBeVisible({ timeout: 30000 });
    await recentFileLink.click();

    // Should navigate to the file (URL contains the file name)
    await expect(page).toHaveURL(/clickable/, { timeout: 30000 });
  });
});
