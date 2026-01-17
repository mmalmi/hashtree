import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('Git init features', () => {
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('git init button should initialize a git repo in a directory', { timeout: 60000 }, async ({ page }) => {
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder with some files
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('git-init-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Wait for folder to appear and click into it
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'git-init-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/git-init-test/, { timeout: 10000 });

    // Create a file in the folder via the tree API (simpler than UI)
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      const rootCid = getCurrentRootCid();
      if (!rootCid) return;

      // Create a README.md file
      const content = new TextEncoder().encode('# Test Repo\n\nThis is a test.');
      const { cid: fileCid, size } = await tree.putFile(content);

      // Add to current directory
      const newRootCid = await tree.setEntry(rootCid, route.path, 'README.md', fileCid, size, LinkType.Blob);
      autosaveIfOwn(newRootCid);
    });

    // Wait for file to appear - check for the file in the list
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'README.md' })).toBeVisible({ timeout: 10000 });

    // Git Init button should be visible (not a git repo yet)
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });

    // Click Git Init
    await gitInitBtn.click();

    // Wait for initialization to complete (button text changes to "Initializing...")
    await expect(page.getByRole('button', { name: 'Initializing...' })).toBeVisible({ timeout: 5000 });
    // Then button should disappear since it's now a git repo
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // Verify .git directory was created
    const gitDir = page.locator('[data-testid="file-list"] a').filter({ hasText: '.git' }).first();
    await expect(gitDir).toBeVisible({ timeout: 10000 });

    // Verify it's detected as a git repo by checking if the Commits button appears
    const commitsBtn = page.getByRole('button', { name: /commits/i });
    await expect(commitsBtn).toBeVisible({ timeout: 10000 });
  });

});
