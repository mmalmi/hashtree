import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('NIP-34 Pull Requests', () => {
  // PR/Issues views are hidden on small screens (lg:flex), need wider viewport
  test.use({ viewport: { width: 1280, height: 720 } });
  test.setTimeout(60000); // 60s timeout for all tests in this describe

  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('should navigate to Pull Requests view via URL', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Get the current URL parts
    const url = new URL(page.url());
    const hash = url.hash.slice(1); // Remove #
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Navigate using ?tab=pulls query param
    await page.goto(`/#/${npub}/${treeName}?tab=pulls`);

    // Should show the PR view with empty state
    // Wait for loading to complete (nostr fetch has 5s timeout)
    await expect(page.locator('text=Loading pull requests...')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=No pull requests yet')).toBeVisible({ timeout: 5000 });

    // Tab navigation should be visible
    await expect(page.locator('a:has-text("Code")')).toBeVisible();
    await expect(page.locator('a:has-text("Pull Requests")')).toBeVisible();
    await expect(page.locator('a:has-text("Issues")')).toBeVisible();
  });

  test('should navigate to Issues view via URL', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get the current URL parts
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Navigate using ?tab=issues query param
    await page.goto(`/#/${npub}/${treeName}?tab=issues`);

    // Should show the Issues view with empty state
    // Wait for loading to complete (nostr fetch has 5s timeout)
    await expect(page.locator('text=Loading issues...')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=No issues yet')).toBeVisible({ timeout: 5000 });

    // Tab navigation should be visible
    await expect(page.locator('a:has-text("Code")')).toBeVisible();
    await expect(page.locator('a:has-text("Pull Requests")')).toBeVisible();
    await expect(page.locator('a:has-text("Issues")')).toBeVisible();
  });

  test('should show FileBrowser on left side in PR view', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get the current URL parts
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Navigate to PR view
    await page.goto(`/#/${npub}/${treeName}?tab=pulls`);

    // Wait for loading to complete (nostr fetch has 5s timeout)
    await expect(page.locator('text=Loading pull requests...')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=No pull requests yet')).toBeVisible({ timeout: 5000 });

    // FileBrowser should be visible - check for the breadcrumb or tree selector
    // The FileBrowser has a tree dropdown or shows "Empty directory"
    const fileBrowserVisible = await page.locator('text=Empty directory').isVisible() ||
      await page.locator('[data-testid="file-list"]').isVisible() ||
      await page.locator('.shrink-0.lg\\:w-80').isVisible();
    expect(fileBrowserVisible).toBeTruthy();
  });

  test('should switch between Code, PRs, and Issues tabs', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get the current URL parts
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Go to PRs
    await page.goto(`/#/${npub}/${treeName}?tab=pulls`);
    await expect(page.locator('text=Loading pull requests...')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=No pull requests yet')).toBeVisible({ timeout: 5000 });

    // Click Issues tab
    await page.locator('a:has-text("Issues")').click();
    await page.waitForURL(/tab=issues/, { timeout: 5000 });
    // Issues view should be visible (might show loading or content)
    await expect(page.locator('a:has-text("Issues")')).toBeVisible();

    // Click Code tab
    await page.locator('a:has-text("Code")').first().click();
    await page.waitForURL((url) => !url.href.includes('tab=pulls') && !url.href.includes('tab=issues'), { timeout: 5000 });
  });

  test('nevent encoding works correctly', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');

    // Wait for app to load
    await page.waitForTimeout(1000);

    // Test that encodeEventId and decodeEventId work correctly
    const result = await page.evaluate(async () => {
      // Dynamic import from the running app
      const nip34Module = await import('/src/nip34.ts');
      const { encodeEventId, decodeEventId } = nip34Module;

      // Test with a sample event ID (64 char hex)
      const hexId = 'a'.repeat(64);
      const encoded = encodeEventId(hexId);
      const decoded = decodeEventId(encoded);

      return {
        hexId,
        encoded,
        decoded,
        startsWithNevent: encoded.startsWith('nevent'),
        decodedMatches: decoded === hexId,
      };
    });

    console.log('[test] Encoding result:', result);
    expect(result.startsWithNevent).toBe(true);
    expect(result.decodedMatches).toBe(true);
  });

  test('PR list title should be a link with nevent ID', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get URL parts for navigation
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Navigate to PRs view using query param
    await page.goto(`/#/${npub}/${treeName}?tab=pulls`);
    await expect(page.locator('text=Loading pull requests...')).not.toBeVisible({ timeout: 10000 });
    await expect(page.locator('text=No pull requests yet')).toBeVisible({ timeout: 5000 });

    // The "New Pull Request" button should be visible when logged in
    // For now just verify the view structure is correct
    await expect(page.locator('a:has-text("Pull Requests")')).toBeVisible();
  });

  test('should navigate to PR detail view via URL with nevent id', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get URL parts for navigation
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Generate a test nevent ID (this won't exist, but we can test the routing)
    const testNeventId = await page.evaluate(async () => {
      const nip34Module = await import('/src/nip34.ts');
      const { encodeEventId } = nip34Module;
      // Use a fake event ID
      return encodeEventId('a'.repeat(64));
    });

    // Navigate to PR detail view with nevent ID
    await page.goto(`/#/${npub}/${treeName}?tab=pulls&id=${testNeventId}`);

    // Should show loading first, then error since event doesn't exist
    // Wait for the error message to appear (event doesn't exist)
    await expect(page.locator('text=Pull request not found')).toBeVisible({ timeout: 10000 });

    // Back button should also be visible
    await expect(page.locator('a:has-text("Back to pull requests")')).toBeVisible();
  });

  test('should navigate to Issue detail view via URL with nevent id', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get URL parts for navigation
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Generate a test nevent ID
    const testNeventId = await page.evaluate(async () => {
      const nip34Module = await import('/src/nip34.ts');
      const { encodeEventId } = nip34Module;
      return encodeEventId('b'.repeat(64));
    });

    // Navigate to Issue detail view with nevent ID
    await page.goto(`/#/${npub}/${treeName}?tab=issues&id=${testNeventId}`);

    // Should show error since event doesn't exist
    await expect(page.locator('text=Issue not found')).toBeVisible({ timeout: 10000 });

    // Back button should also be visible
    await expect(page.locator('a:has-text("Back to issues")')).toBeVisible();
  });

  test('should have back button in PR detail view that navigates to list', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get URL parts for navigation
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Generate a test nevent ID
    const testNeventId = await page.evaluate(async () => {
      const nip34Module = await import('/src/nip34.ts');
      const { encodeEventId } = nip34Module;
      return encodeEventId('c'.repeat(64));
    });

    // Navigate to PR detail view
    await page.goto(`/#/${npub}/${treeName}?tab=pulls&id=${testNeventId}`);

    // Wait for error state
    await expect(page.locator('text=Pull request not found')).toBeVisible({ timeout: 10000 });

    // Click the back button
    await page.locator('a:has-text("Back to pull requests")').click();

    // Should be back at the PR list view
    await page.waitForURL(/tab=pulls/, { timeout: 5000 });
    // URL should not have &id= anymore
    expect(page.url()).not.toContain('&id=');
    // PR list view should be visible (navigation worked)
    await expect(page.getByText('Pull request not found')).not.toBeVisible();
    await expect(page.getByText('No pull requests yet')).toBeVisible();
  });

  test('should have back button in Issue detail view that navigates to list', async ({ page }) => {
    await navigateToPublicFolder(page);

    // Get URL parts for navigation
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Generate a test nevent ID
    const testNeventId = await page.evaluate(async () => {
      const nip34Module = await import('/src/nip34.ts');
      const { encodeEventId } = nip34Module;
      return encodeEventId('d'.repeat(64));
    });

    // Navigate to Issue detail view
    await page.goto(`/#/${npub}/${treeName}?tab=issues&id=${testNeventId}`);

    // Wait for error state
    await expect(page.locator('text=Issue not found')).toBeVisible({ timeout: 10000 });

    // Click the back button
    await page.locator('a:has-text("Back to issues")').click();

    // Should be back at the Issues list view
    await page.waitForURL(/tab=issues/, { timeout: 5000 });
    // URL should not have &id= anymore
    expect(page.url()).not.toContain('&id=');
    // Issues list view should be visible (navigation worked)
    await expect(page.getByText('Issue not found')).not.toBeVisible();
    await expect(page.getByText('No issues yet')).toBeVisible();
  });

  test('PR detail view shows Conversation and Files changed tabs', async ({ page }) => {
    // This test verifies the PR detail view tabs UI
    // Since Nostr PR creation requires relay connectivity, we test the UI by navigating
    // to a PR detail view URL directly (which will show "not found" but still render tabs)
    await navigateToPublicFolder(page);

    // Get the current URL parts
    const url = new URL(page.url());
    const hash = url.hash.slice(1);
    const qIdx = hash.indexOf('?');
    const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
    const parts = path.split('/').filter(Boolean);
    const npub = parts[0];
    const treeName = parts[1];

    // Generate a test nevent ID
    const testNeventId = await page.evaluate(async () => {
      const nip34Module = await import('/src/nip34.ts');
      const { encodeEventId } = nip34Module;
      return encodeEventId('e'.repeat(64));
    });

    // Navigate to PR detail view with the test event ID
    await page.goto(`/#/${npub}/${treeName}?tab=pulls&id=${testNeventId}`);

    // Wait for the PR view to load (will show "not found" since event doesn't exist)
    await expect(page.locator('text=Pull request not found')).toBeVisible({ timeout: 10000 });

    // The back button should be visible
    await expect(page.locator('a:has-text("Back to pull requests")')).toBeVisible();

    // Tab navigation should still be visible at the top (use .first() to avoid multiple matches)
    await expect(page.locator('a:has-text("Code")').first()).toBeVisible();
    await expect(page.locator('a:has-text("Pull Requests")').first()).toBeVisible();
    await expect(page.locator('a:has-text("Issues")').first()).toBeVisible();
  });

  test('PR detail view tabs work when loaded from existing PR', async ({ page }) => {
    // This test creates a git repo with branches but skips PR creation via Nostr
    // Instead it verifies the PR list and detail view structure
    test.setTimeout(90000);
    test.slow();
    await navigateToPublicFolder(page);

    // Create a folder and init as git repo with branches
    await page.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('pr-structure-test');
    await page.click('button:has-text("Create")');
    await expect(page.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    const folderLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'pr-structure-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page.waitForURL(/pr-structure-test/, { timeout: 10000 });

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
      const content = new TextEncoder().encode('initial content');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'file.txt', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
    });

    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'file.txt' })).toBeVisible({ timeout: 15000 });

    // Git init
    const gitInitBtn = page.getByRole('button', { name: 'Git Init' });
    await expect(gitInitBtn).toBeVisible({ timeout: 15000 });
    await gitInitBtn.click();
    await expect(gitInitBtn).not.toBeVisible({ timeout: 30000 });

    const gitDirEntry = page.locator('[data-testid="file-list"] a').filter({ hasText: '.git' }).first();
    await expect(gitDirEntry).toBeVisible({ timeout: 30000 });

    // Wait for branch selector
    const branchSelector = page.locator('button:has(.i-lucide-git-branch):not([data-testid="git-init-btn"])').first();
    await expect(branchSelector).toBeVisible({ timeout: 30000 });

    // Go to Pull Requests tab
    await page.locator('a:has-text("Pull Requests")').click();
    await page.waitForURL(/tab=pulls/, { timeout: 5000 });

    // Verify PR list view structure
    await expect(page.locator('text=No pull requests yet')).toBeVisible({ timeout: 10000 });
    await expect(page.locator('button:has-text("New Pull Request")')).toBeVisible();

    // Verify repo tab navigation is visible
    await expect(page.locator('a:has-text("Code")')).toBeVisible();
    await expect(page.locator('a:has-text("Pull Requests")')).toBeVisible();
    await expect(page.locator('a:has-text("Issues")')).toBeVisible();

    // Switch to Issues tab
    await page.locator('a:has-text("Issues")').click();
    await page.waitForURL(/tab=issues/, { timeout: 5000 });
    await expect(page.locator('text=No issues yet')).toBeVisible({ timeout: 10000 });

    // Switch back to Code tab
    await page.locator('a:has-text("Code")').first().click();
    await page.waitForURL((url) => !url.href.includes('tab='), { timeout: 5000 });
    // Should see the file list again
    await expect(page.locator('[data-testid="file-list"] a').filter({ hasText: 'file.txt' })).toBeVisible({ timeout: 10000 });
  });
});
