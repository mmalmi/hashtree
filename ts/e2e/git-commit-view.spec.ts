import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';
// Tests use isolated page contexts with disableOthersPool - safe for parallel execution

test.describe('Git commit view', () => {
  test.use({ viewport: { width: 1200, height: 800 } });
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('clicking commit message should navigate to commit view with details', { timeout: 90000 }, async ({ page }) => {
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder for our test repo
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('commit-view-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into the folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'commit-view-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/commit-view-test/, { timeout: 10000 });

    // Create initial files via the tree API
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      // Create README.md
      const readmeContent = new TextEncoder().encode('# Commit View Test\n\nTesting the commit view functionality.');
      const { cid: readmeCid, size: readmeSize } = await tree.putFile(readmeContent);
      rootCid = await tree.setEntry(rootCid, route.path, 'README.md', readmeCid, readmeSize, LinkType.Blob);

      // Create index.js
      const indexContent = new TextEncoder().encode('console.log("Hello from commit view test!");');
      const { cid: indexCid, size: indexSize } = await tree.putFile(indexContent);
      rootCid = await tree.setEntry(rootCid, route.path, 'index.js', indexCid, indexSize, LinkType.Blob);

      autosaveIfOwn(rootCid);
    });

    // Wait for files to appear
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'README.md' })).toBeVisible({ timeout: 15000 });

    // Git Init button should be visible
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });

    // Click Git Init
    await gitInitBtn.click();

    // Wait for initialization to complete
    await expect(page.getByRole('button', { name: 'Initializing...' })).toBeVisible({ timeout: 5000 });
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // Verify commits button appears
    const commitsBtn = page.getByRole('button', { name: /commits/i });
    await expect(commitsBtn).toBeVisible({ timeout: 20000 });
    await expect(commitsBtn).toContainText(/1/, { timeout: 20000 });

    // Wait for file table commit messages to load
    // The header row should show commit info
    const headerCommitLink = page.locator('thead a').filter({ hasText: /Initial commit|Added files/ });
    await expect(headerCommitLink).toBeVisible({ timeout: 15000 });

    // Click on the commit message link in the header row
    await headerCommitLink.click();

    // Should navigate to commit view URL with ?commit= param
    await page.waitForURL(/\?commit=/, { timeout: 10000 });

    // Commit view should display commit details
    // Check for commit hash display (full 40-char hash in code element)
    const commitHashCode = page.locator('code').filter({ hasText: /[a-f0-9]{40}/ });
    await expect(commitHashCode).toBeVisible({ timeout: 15000 });

    // Check for author info (Anonymous is default for wasm-git)
    await expect(page.locator('text=Anonymous').first()).toBeVisible({ timeout: 5000 });

    // Check for commit message
    await expect(page.locator('h1').filter({ hasText: /Initial commit|Added files/ })).toBeVisible({ timeout: 5000 });

    // Check for Browse files button
    const browseFilesBtn = page.locator('a').filter({ hasText: 'Browse files' });
    await expect(browseFilesBtn).toBeVisible({ timeout: 5000 });

    // Click Browse files to return to code view
    await browseFilesBtn.click();

    // Should navigate back to the repo (without ?commit= param)
    await expect(page).not.toHaveURL(/\?commit=/);

    // File table should be visible again
    await expect(page.locator('table tbody').first()).toBeVisible({ timeout: 10000 });
  });

  test('commit view shows commit details and browse files link', { timeout: 90000 }, async ({ page }) => {
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder for test
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('diff-view-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into the folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'diff-view-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/diff-view-test/, { timeout: 10000 });

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

      const content = new TextEncoder().encode('line 1\nline 2\nline 3\n');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'test.txt', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    // Wait for file
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'test.txt' })).toBeVisible({ timeout: 15000 });

    // Init git
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
    await gitInitBtn.click();
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // Wait for commits button
    await expect(page.getByRole('button', { name: /commits/i })).toBeVisible({ timeout: 20000 });

    // Navigate directly to commit view via URL
    const currentUrl = page.url();
    const commitViewUrl = currentUrl.includes('?')
      ? currentUrl.replace(/\?.*/, '?commit=HEAD')
      : currentUrl + '?commit=HEAD';

    await page.goto(commitViewUrl);

    // Wait for commit view to load - check for commit hash code element
    const commitHashCode = page.locator('code').filter({ hasText: /[a-f0-9]{40}/ });
    await expect(commitHashCode).toBeVisible({ timeout: 15000 });

    // Check for Browse files link
    const browseFilesLink = page.locator('a').filter({ hasText: 'Browse files' });
    await expect(browseFilesLink).toBeVisible({ timeout: 5000 });

    // Check for commit message header
    await expect(page.locator('h1').filter({ hasText: /Initial commit|Added files/ })).toBeVisible({ timeout: 5000 });
  });

  test('tab navigation shows on commit view', { timeout: 90000 }, async ({ page }) => {
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('tab-nav-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    // Navigate into folder
    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'tab-nav-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/tab-nav-test/, { timeout: 10000 });

    // Create file and init git
    await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();

      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content = new TextEncoder().encode('test');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'test.txt', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'test.txt' })).toBeVisible({ timeout: 15000 });

    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
    await gitInitBtn.click();
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    // Navigate to commit view
    const currentUrl = page.url();
    const commitViewUrl = currentUrl.includes('?')
      ? currentUrl.replace(/\?.*/, '?commit=HEAD')
      : currentUrl + '?commit=HEAD';

    await page.goto(commitViewUrl);

    // Tab navigation should be visible with Code, Pull Requests, Issues tabs
    const tabNav = page.locator('a').filter({ hasText: 'Code' });
    await expect(tabNav).toBeVisible({ timeout: 10000 });

    const pullsTab = page.locator('a').filter({ hasText: 'Pull Requests' });
    await expect(pullsTab).toBeVisible({ timeout: 5000 });

    const issuesTab = page.locator('a').filter({ hasText: 'Issues' });
    await expect(issuesTab).toBeVisible({ timeout: 5000 });

    // Code tab should be active (highlighted)
    await expect(tabNav).toHaveClass(/b-b-accent/);
  });
});
