import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, goToTreeList, disableOthersPool, waitForAppReady } from './test-utils.js';

// Run keyboard tests serially - they depend on focus state which is unreliable under parallel execution
test.describe.configure({ mode: 'serial' });

// Helper to create tree and navigate into it
async function createAndEnterTree(page: any, name: string) {
  await goToTreeList(page);
  await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 10000 });
  await page.getByRole('button', { name: 'New Folder' }).click();
  await page.locator('input[placeholder="Folder name..."]').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 10000 });
}

// Helper to create a file and return to parent folder
async function createFile(page: any, name: string, content: string = '', treeName: string = '') {
  await page.getByRole('button', { name: /File/ }).first().click();
  await page.locator('input[placeholder="File name..."]').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
  if (content) {
    await page.locator('textarea').fill(content);
  }
  await page.getByRole('button', { name: 'Done' }).click();
  const unsavedModal = page.locator('[data-modal-backdrop]');
  if (await unsavedModal.isVisible()) {
    await unsavedModal.getByRole('button', { name: 'Save', exact: true }).click();
  }
  await expect(page.getByRole('button', { name: 'Done' })).not.toBeVisible({ timeout: 10000 });
  await expect(unsavedModal).toHaveCount(0);

  // Navigate back to the tree folder after file creation
  if (treeName) {
    await page.locator(`a:has-text("${treeName}")`).first().click();
    await expect(page.getByTestId('file-list')).toBeVisible({ timeout: 5000 });
  }
}

test.describe('Keyboard Navigation', () => {
  test.setTimeout(60000);

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests

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
    await waitForAppReady(page); // Wait for page to load after reload
    await disableOthersPool(page); // Re-apply after reload
    await navigateToPublicFolder(page);
  });

  test('should navigate between files with arrow keys', async ({ page }) => {
    // Create tree with multiple files
    await createAndEnterTree(page, 'keyboard-test');
    await createFile(page, 'file1.txt', 'Content 1', 'keyboard-test');
    await createFile(page, 'file2.txt', 'Content 2', 'keyboard-test');
    await createFile(page, 'file3.txt', 'Content 3', 'keyboard-test');

    // Start from a deterministic selection
    const file1Link = page.locator('[data-testid="file-list"] a').filter({ hasText: 'file1.txt' }).first();
    await expect(file1Link).toBeVisible({ timeout: 10000 });
    await file1Link.click();
    await expect(page.locator('pre')).toContainText('Content 1', { timeout: 10000 });

    // Focus the file list and navigate to next file
    const fileList = page.getByTestId('file-list');
    await fileList.click();
    await fileList.focus();
    await page.keyboard.press('ArrowDown');

    // Should now show file2.txt
    await expect(page.locator('pre')).toContainText('Content 2', { timeout: 10000 });
  });

  test('should navigate with vim keys (j/k)', async ({ page }) => {
    await createAndEnterTree(page, 'vim-keys-test');
    await createFile(page, 'alpha.txt', 'Alpha content', 'vim-keys-test');
    await createFile(page, 'beta.txt', 'Beta content', 'vim-keys-test');

    const fileList = page.getByTestId('file-list');
    await expect(fileList).toBeVisible({ timeout: 10000 });
    await fileList.click(); // Click to ensure focus
    await fileList.focus();

    const reachContent = async (needle: string, key: string) => {
      for (let i = 0; i < 6; i++) {
        await page.keyboard.press(key);
        const text = await page.locator('pre').textContent().catch(() => '');
        if (text && text.includes(needle)) {
          return true;
        }
        await page.waitForTimeout(200);
      }
      return false;
    };

    // Navigate down with j to alpha
    expect(await reachContent('Alpha content', 'j')).toBe(true);

    // Navigate down with j to beta
    expect(await reachContent('Beta content', 'j')).toBe(true);

    // Navigate back up with k to alpha
    expect(await reachContent('Alpha content', 'k')).toBe(true);
  });

  test('should navigate into directories with Enter key', async ({ page }) => {
    await createAndEnterTree(page, 'enter-nav-test');

    // Create a subdirectory
    await page.getByRole('button', { name: /Folder/ }).first().click();
    await page.locator('input[placeholder="Folder name..."]').fill('subdir');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // Navigate back to parent
    await page.locator('a:has-text("enter-nav-test")').first().click();
    await expect(page.getByTestId('file-list')).toBeVisible({ timeout: 5000 });

    const fileList = page.getByTestId('file-list');
    await fileList.focus();

    // Navigate to subdir (past '..' and '.')
    await page.keyboard.press('ArrowDown'); // '..'
    await page.keyboard.press('ArrowDown'); // '.'
    await page.keyboard.press('ArrowDown'); // 'subdir'

    // Press Enter to navigate into directory
    await page.keyboard.press('Enter');

    // Should now be in subdir - verify by checking URL or empty directory message
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 5000 });
  });

  test('should show focus ring on focused item', async ({ page }) => {
    await createAndEnterTree(page, 'focus-ring-test');
    await createFile(page, 'test.txt', 'Test content', 'focus-ring-test');

    const fileList = page.getByTestId('file-list');
    await fileList.focus();

    // Navigate down to focus '..'
    await page.keyboard.press('ArrowDown');

    // Check that '..' row has focus ring class
    const parentRow = page.locator('a:has-text("..")');
    await expect(parentRow).toHaveClass(/ring-accent/);
  });

  test('should navigate tree list with arrow keys', async ({ page }) => {
    // Create multiple trees
    await goToTreeList(page);
    await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 10000 });

    // Create first tree
    await page.getByRole('button', { name: 'New Folder' }).click();
    await page.locator('input[placeholder="Folder name..."]').fill('tree-a');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // Go back and create second tree
    await page.locator('a:has-text("..")').first().click();
    await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 10000 });
    await page.getByRole('button', { name: 'New Folder' }).click();
    await page.locator('input[placeholder="Folder name..."]').fill('tree-b');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // Go back to tree list
    await page.locator('a:has-text("..")').first().click();
    await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 10000 });

    // Use first() since profile page may have multiple file-list elements
    const fileList = page.getByTestId('file-list').first();
    await fileList.click(); // Click to ensure focus is on the element
    await fileList.focus();

    // Navigate down through tree items - retry to handle reactivity timing
    await expect(async () => {
      await page.keyboard.press('ArrowDown');
      // Check focus ring is visible on a tree item
      const focusedTree = page.locator('[data-testid="file-list"] a.ring-accent');
      await expect(focusedTree).toBeVisible({ timeout: 1000 });
    }).toPass({ timeout: 10000, intervals: [500, 1000, 2000] });
  });
});
