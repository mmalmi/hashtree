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

// Helper to create a file and navigate back
async function createFile(page: any, name: string, content: string = '', treeName: string = '') {
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

  // Navigate back to the tree folder after file creation
  if (treeName) {
    await page.locator(`a:has-text("${treeName}")`).first().click();
    await page.waitForTimeout(500);
  }
}

test.describe('Viewer Actions', () => {
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

  test('should show back button that navigates to directory', async ({ page }) => {
    await createAndEnterTree(page, 'back-btn-test');
    await createFile(page, 'test.txt', 'Test content', 'back-btn-test');

    // Click on file to view it
    await page.locator('a:has-text("test.txt")').first().click();
    await page.waitForTimeout(500);

    // Verify we're viewing the file
    await expect(page.locator('pre')).toContainText('Test content', { timeout: 5000 });

    // Verify back button is visible
    const backButton = page.getByTestId('viewer-back');
    await expect(backButton).toBeVisible();

    // Click back button
    await backButton.click();
    await page.waitForTimeout(500);

    // Should be back in the directory (no file content visible, directory actions visible)
    await expect(page.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 5000 });
  });

  test('should show download button for files', async ({ page }) => {
    await createAndEnterTree(page, 'download-test');
    await createFile(page, 'download.txt', 'Download me', 'download-test');

    await page.locator('a:has-text("download.txt")').first().click();
    await page.waitForTimeout(500);

    // Verify download button exists
    const downloadButton = page.getByTestId('viewer-download');
    await expect(downloadButton).toBeVisible();
    await expect(downloadButton).toHaveText('Download');
  });

  test('should show permalink button for files', async ({ page }) => {
    await createAndEnterTree(page, 'permalink-test');
    await createFile(page, 'perma.txt', 'Permanent content', 'permalink-test');

    await page.locator('a:has-text("perma.txt")').first().click();
    await page.waitForTimeout(500);

    // Verify permalink button exists and is a link
    const permalinkButton = page.getByTestId('viewer-permalink');
    await expect(permalinkButton).toBeVisible();
    await expect(permalinkButton).toHaveText('Permalink');

    // Permalink should be a link (anchor tag)
    const href = await permalinkButton.getAttribute('href');
    expect(href).toBeTruthy();
    // Should contain nhash format
    expect(href).toMatch(/#\/nhash/);
  });

  test('should show share button that opens share modal', async ({ page }) => {
    await createAndEnterTree(page, 'share-test');
    await createFile(page, 'share.txt', 'Share this', 'share-test');

    await page.locator('a:has-text("share.txt")').first().click();
    await page.waitForTimeout(500);

    // Verify share button exists
    const shareButton = page.getByTestId('viewer-share');
    await expect(shareButton).toBeVisible();

    // Click share button
    await shareButton.click();

    // Share modal should open - look for modal or QR code
    await expect(page.getByTestId('share-modal')).toBeVisible({ timeout: 5000 });
  });

  test('should show rename button for own files', async ({ page }) => {
    await createAndEnterTree(page, 'rename-test');
    await createFile(page, 'rename.txt', 'Rename me', 'rename-test');

    await page.locator('a:has-text("rename.txt")').first().click();
    await page.waitForTimeout(500);

    // Verify rename button exists
    const renameButton = page.getByTestId('viewer-rename');
    await expect(renameButton).toBeVisible();
    await expect(renameButton).toHaveText('Rename');
  });

  test('should show edit button for text files', async ({ page }) => {
    await createAndEnterTree(page, 'edit-test');
    await createFile(page, 'editable.txt', 'Edit me', 'edit-test');

    await page.locator('a:has-text("editable.txt")').first().click();
    await page.waitForTimeout(500);

    // Verify edit button exists for text file
    const editButton = page.getByTestId('viewer-edit');
    await expect(editButton).toBeVisible();
    await expect(editButton).toHaveText('Edit');
  });

  test('should show delete button for own files', async ({ page }) => {
    await createAndEnterTree(page, 'delete-test');
    await createFile(page, 'deletable.txt', 'Delete me', 'delete-test');

    await page.locator('a:has-text("deletable.txt")').first().click();
    await page.waitForTimeout(500);

    // Verify delete button exists
    const deleteButton = page.getByTestId('viewer-delete');
    await expect(deleteButton).toBeVisible();
    await expect(deleteButton).toHaveText('Delete');
  });

  test('should show correct file icon for different file types', async ({ page }) => {
    await createAndEnterTree(page, 'icon-test');

    // Create different file types
    await createFile(page, 'code.js', 'console.log("test")', 'icon-test');

    await page.locator('a:has-text("code.js")').first().click();
    await page.waitForTimeout(500);

    // Viewer header should be visible with file icon
    const header = page.getByTestId('viewer-header');
    await expect(header).toBeVisible();

    // Should contain the file-code icon for .js files
    await expect(header.locator('.i-lucide-file-code')).toBeVisible();
  });

  test('viewer header should contain all action buttons', async ({ page }) => {
    await createAndEnterTree(page, 'all-actions-test');
    await createFile(page, 'actions.txt', 'All actions', 'all-actions-test');

    await page.locator('a:has-text("actions.txt")').first().click();
    await page.waitForTimeout(500);

    // All buttons should be visible for a text file owned by user
    await expect(page.getByTestId('viewer-back')).toBeVisible();
    await expect(page.getByTestId('viewer-download')).toBeVisible();
    await expect(page.getByTestId('viewer-permalink')).toBeVisible();
    await expect(page.getByTestId('viewer-share')).toBeVisible();
    await expect(page.getByTestId('viewer-rename')).toBeVisible();
    await expect(page.getByTestId('viewer-edit')).toBeVisible();
    await expect(page.getByTestId('viewer-delete')).toBeVisible();
  });
});
