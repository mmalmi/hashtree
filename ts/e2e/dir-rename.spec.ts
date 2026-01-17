import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder } from './test-utils.js';

test.describe('Directory rename', () => {
  // Increase timeout for all tests since new user setup now creates 3 default folders
  test.setTimeout(60000);
  test('should rename a subdirectory', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await navigateToPublicFolder(page);

    // Now we're in the user's public folder, which starts empty
    // Wait for the Folder button to be available (may take a moment for UI to settle)
    await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 10000 });
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // Create a subdirectory - use the Folder button in the toolbar
    await page.getByRole('button', { name: 'New Folder' }).click();

    // Enter subdirectory name in modal
    const subInput = page.locator('input[placeholder="Folder name..."]');
    await subInput.waitFor({ timeout: 5000 });
    await subInput.fill('old-folder-name');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(1000); // Wait for folder to be created

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for subfolder creation to complete and appear in list
    // Wait longer as the file list may take time to update
    await page.waitForTimeout(2000);

    // Wait for the subdirectory to appear in the file list (look for the link)
    await expect(page.locator('[data-testid="file-list"] a:has-text("old-folder-name")')).toBeVisible({ timeout: 15000 });

    // Navigate into the subdirectory by clicking on it
    await page.click('[data-testid="file-list"] a:has-text("old-folder-name")');

    // Wait for navigation - should see empty directory inside the subfolder
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // URL should now include the folder name
    await expect(page).toHaveURL(/old-folder-name/);

    // Find and click the Rename button (should be visible for subdirectories)
    await page.click('button:has-text("Rename"):visible');

    // Wait for rename modal to appear with pre-filled input
    const renameInput = page.locator('input[placeholder="New name..."]');
    await renameInput.waitFor({ timeout: 5000 });

    // Verify the input is pre-filled with the current name
    await expect(renameInput).toHaveValue('old-folder-name');

    // Clear and enter new name
    await renameInput.fill('new-folder-name');
    // Click the Rename button inside the modal (btn-success class)
    await page.click('.fixed.inset-0 button.btn-success:has-text("Rename")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // URL should now include the new folder name
    await expect(page).toHaveURL(/new-folder-name/);
    expect(page.url()).not.toContain('old-folder-name');
  });

  test('should not show rename button for root directory', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await navigateToPublicFolder(page);

    // Now we're in the user's public folder (which is a root tree)
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // Should NOT see a Rename button for root directory (public folder)
    // The "Folder" button should exist (for creating subfolders) but not Rename
    await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible();
    await expect(page.locator('button:has-text("Rename")')).not.toBeVisible();
  });

  test('should delete a subdirectory', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await navigateToPublicFolder(page);

    // Now we're in the user's public folder, which starts empty
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // Create a subdirectory - use visible button with folder-plus icon
    await page.getByRole('button', { name: 'New Folder' }).click();
    const subInput = page.locator('input[placeholder="Folder name..."]');
    await subInput.waitFor({ timeout: 5000 });
    await subInput.fill('folder-to-delete');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for subfolder creation to complete
    await page.waitForTimeout(2000);

    // Wait for and navigate into the subdirectory
    await expect(page.locator('[data-testid="file-list"] a:has-text("folder-to-delete")')).toBeVisible({ timeout: 15000 });
    await page.click('[data-testid="file-list"] a:has-text("folder-to-delete")');
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // Set up dialog handler for the confirmation prompt
    page.on('dialog', async (dialog) => {
      await dialog.accept();
    });

    // Click Delete button
    await page.click('button:has-text("Delete"):visible');

    // Wait for navigation to complete
    await page.waitForURL(/\/public$/, { timeout: 10000 });

    // Should show empty directory in parent
    await expect(page.getByText(/Drop or click to add|Empty directory/).first()).toBeVisible({ timeout: 10000 });

    // URL should no longer include the deleted folder name
    expect(page.url()).not.toContain('folder-to-delete');
  });
});
