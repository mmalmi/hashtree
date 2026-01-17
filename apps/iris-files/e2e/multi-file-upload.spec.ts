import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, goToTreeList, disableOthersPool } from './test-utils.js';

test.describe('Multi-file upload', () => {
  test.setTimeout(60000);
  const testFiles = [
    { name: 'test-file-1.txt', content: 'Content of test file 1' },
    { name: 'test-file-2.txt', content: 'Content of test file 2' },
    { name: 'test-file-3.txt', content: 'Content of test file 3' },
  ];

  async function uploadTestFiles(page: any) {
    await page.evaluate(async (files: { name: string; content: string }[]) => {
      const { uploadFiles } = await import('/src/stores/upload.ts');
      const dataTransfer = new DataTransfer();
      for (const file of files) {
        dataTransfer.items.add(new File([file.content], file.name, { type: 'text/plain' }));
      }
      await uploadFiles(dataTransfer.files);
    }, testFiles);
  }

  test('should upload multiple files at once', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await navigateToPublicFolder(page);

    // Navigate to tree list first, then create a folder
    await goToTreeList(page);

    // Click New Folder to create a folder/tree
    await page.getByRole('button', { name: 'New Folder' }).click();

    // Enter folder name in modal
    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('test-upload-folder');
    await page.click('button:has-text("Create")');

    // Wait for modal to close (the fixed background overlay)
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for navigation to tree view - should show empty directory
    await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });

    await uploadTestFiles(page);

    // Wait for all files to appear in the file browser (use more specific selector to avoid upload progress indicator)
    await expect(page.locator('[data-testid="file-list"] a:has-text("test-file-1.txt")')).toBeVisible({ timeout: 15000 });
    await expect(page.locator('[data-testid="file-list"] a:has-text("test-file-2.txt")')).toBeVisible({ timeout: 15000 });
    await expect(page.locator('[data-testid="file-list"] a:has-text("test-file-3.txt")')).toBeVisible({ timeout: 15000 });
  });

  test('should not navigate to any file after multi-file upload', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await navigateToPublicFolder(page);

    // Navigate to tree list first, then create a folder
    await goToTreeList(page);

    // Click New Folder to create a folder/tree
    await page.getByRole('button', { name: 'New Folder' }).click();

    // Enter folder name in modal
    const input = page.locator('input[placeholder="Folder name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('test-upload-folder-2');
    await page.click('button:has-text("Create")');

    // Wait for modal to close
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for navigation to tree view - should show empty directory
    await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });

    await uploadTestFiles(page);

    // Wait for files to appear
    await expect(page.locator('text=test-file-1.txt')).toBeVisible({ timeout: 10000 });

    // URL should not include any of the test file names
    const currentUrl = page.url();
    expect(currentUrl).not.toContain('test-file-1.txt');
    expect(currentUrl).not.toContain('test-file-2.txt');
    expect(currentUrl).not.toContain('test-file-3.txt');
  });
});
