import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, useLocalRelay } from './test-utils.js';

test.describe('Git commit status indicator', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await useLocalRelay(page);
  });

  test('shows clean after commit, uncommitted after edit', { timeout: 90000 }, async ({ page }) => {
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder for our test repo
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('status-indicator-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into the folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'status-indicator-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/status-indicator-test/, { timeout: 10000 });

    // Create initial file via tree API
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content = new TextEncoder().encode('# Status Test\n\nTesting status indicators.');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'README.md', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    // Wait for file to appear
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'README.md' })).toBeVisible({ timeout: 15000 });

    // Git Init
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
    await gitInitBtn.click();
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // After git init with initial commit, should show "clean"
    const cleanIndicator = page.locator('[title="No uncommitted changes"]');
    await expect(cleanIndicator).toBeVisible({ timeout: 30000 });

    // Add a new file to create uncommitted changes
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content = new TextEncoder().encode('export const VERSION = "1.0.0";');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'version.js', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    // Should show "uncommitted" indicator
    const uncommittedBtn = page.locator('button').filter({ hasText: /uncommitted/i });
    await expect(uncommittedBtn).toBeVisible({ timeout: 30000 });

    // Make a commit
    await uncommittedBtn.click();
    const commitModal = page.locator('.fixed.inset-0').filter({ hasText: 'Commit Changes' });
    await expect(commitModal).toBeVisible({ timeout: 5000 });

    // Enter commit message and commit
    const commitMessageInput = commitModal.locator('textarea[placeholder*="Describe"]');
    await commitMessageInput.fill('Add version.js');
    const commitBtn = commitModal.getByRole('button', { name: 'Commit' });
    await commitBtn.click();

    // Modal should close
    await expect(commitModal).not.toBeVisible({ timeout: 30000 });

    // Should show "clean" again after commit
    await expect(cleanIndicator).toBeVisible({ timeout: 30000 });
    await expect(uncommittedBtn).not.toBeVisible({ timeout: 5000 });
  });

  test('commit hash in history modal links to commit view', { timeout: 90000 }, async ({ page }) => {
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder for our test repo
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('commit-link-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into the folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'commit-link-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/commit-link-test/, { timeout: 10000 });

    // Create initial file
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content = new TextEncoder().encode('# Commit Link Test');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'README.md', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'README.md' })).toBeVisible({ timeout: 15000 });

    // Git Init
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
    await gitInitBtn.click();
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // Wait for commits button
    const commitsBtn = page.getByRole('button', { name: /commits/i });
    await expect(commitsBtn).toBeVisible({ timeout: 20000 });
    await expect(commitsBtn).toContainText(/1/, { timeout: 20000 });

    // Open history modal
    await commitsBtn.click();
    const historyModal = page.locator('.fixed.inset-0').filter({ hasText: 'Commit History' });
    await expect(historyModal).toBeVisible({ timeout: 5000 });

    // Find the commit hash link (should be a 7-character hash in monospace font)
    const hashLink = historyModal.locator('a.font-mono, a span.font-mono').first();
    await expect(hashLink).toBeVisible({ timeout: 5000 });

    // Get the hash text
    const hashText = await hashLink.textContent();
    expect(hashText).toMatch(/^[a-f0-9]{7}$/);

    // Click the hash to navigate to commit view
    await hashLink.click();

    // Modal should close
    await expect(historyModal).not.toBeVisible({ timeout: 5000 });

    // URL should now contain ?commit=<hash>
    await expect(page).toHaveURL(/\?commit=[a-f0-9]+/, { timeout: 10000 });

    // CommitView should be visible - check for "Browse files" link which is always shown
    await expect(page.locator('a').filter({ hasText: 'Browse files' })).toBeVisible({ timeout: 15000 });

    // Should show commit stats (file changed count)
    await expect(page.locator('text=/\\d+ files? changed/')).toBeVisible({ timeout: 10000 });
  });
});
