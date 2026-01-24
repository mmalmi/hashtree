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
  const doneButton = page.getByRole('button', { name: 'Done' });
  await expect(doneButton).toBeVisible({ timeout: 5000 });
  if (content) {
    await page.locator('textarea').fill(content);
    const saveButton = page.getByRole('button', { name: /Save|Saved|Saving/ }).first();
    if (await saveButton.isEnabled().catch(() => false)) {
      await saveButton.click();
    }
    await expect(saveButton).toBeDisabled({ timeout: 10000 });
  }
  await doneButton.click();
  await expect(page.locator('textarea')).not.toBeVisible({ timeout: 10000 });
}

test.describe('README Panel', () => {
  test.setTimeout(120000);

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

  test('should display README.md content in directory view', async ({ page }) => {
    // Create tree with README
    await createAndEnterTree(page, 'readme-test');
    await createFile(page, 'README.md', '# Hello World\n\nThis is a test readme.');

    // Navigate back to tree to see the readme panel
    await goToTreeList(page);
    await page.locator('a:has-text("readme-test")').first().click();

    // Check that README panel is visible with rendered content
    // The panel has a header with book-open icon and "README.md" text
    await expect(page.locator('.i-lucide-book-open')).toBeVisible({ timeout: 30000 });
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

    // Check edit button exists in README panel
    await expect(page.locator('.i-lucide-book-open')).toBeVisible({ timeout: 30000 });
    // Edit button should be in the README panel header
    await expect(page.getByRole('button', { name: 'Edit' })).toBeVisible();
  });

  test('should navigate relative links within the tree', async ({ page }) => {
    // Create tree with a subdirectory and README linking to it
    await createAndEnterTree(page, 'link-test');

    // Create a subdir with its own README
    await page.getByRole('button', { name: 'Folder' }).click();
    await page.locator('input[placeholder="Folder name..."]').fill('subdir');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 10000 });

    // Create README in subdir
    await createFile(page, 'README.md', '# Subdir Docs\n\nThis is the subdir readme.');

    // Go back to parent
    await page.locator('a[href*="link-test"]').filter({ hasText: 'link-test' }).first().click();
    await expect(page.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 10000 });

    // Create root README with relative link
    await createFile(page, 'README.md', '# Main\n\nSee [subdir docs](subdir/README.md) for more.');

    // Navigate back to tree root to see the readme panel
    await goToTreeList(page);
    await page.locator('a:has-text("link-test")').first().click();
    await expect(page.locator('.i-lucide-book-open')).toBeVisible({ timeout: 30000 });

    // Click the relative link in the README
    await page.locator('.prose a:has-text("subdir docs")').click();

    // Should navigate to the subdir README file
    await expect(page).toHaveURL(/#.*link-test.*subdir.*README\.md/);
  });
});
