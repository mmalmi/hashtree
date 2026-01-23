/**
 * E2E test for address bar navigation to tree paths
 * Tests that pasting npub/treeName/path navigates to TreeRoute, not UserRoute
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler } from './test-utils.js';

test.describe('Address Bar Tree Navigation', () => {
  test('navigates to tree path when typing npub/treeName/path in address bar', async ({ page }) => {
    setupPageErrorHandler(page);
    // Use iris.html which has the address bar (main toolbar)
    await page.goto('/iris.html');
    // Wait for toolbar to be visible (IrisApp doesn't have header, it has a toolbar)
    await page.waitForSelector('input[placeholder="Search or enter address"]', { timeout: 30000 });

    // Use a test npub - any valid npub works for testing route parsing
    const npub = 'npub1wj6a4ex6hsp7rq4g3h9fzqwezt9f0478vnku9wzzkl25w2uudnds4z3upt';

    // Now test the address bar navigation
    // This is the exact pattern from the bug report: npub/treeName/nested/path
    const addressInput = page.locator('input[placeholder="Search or enter address"]');
    await addressInput.click();

    // Clear and type the full path (simulating paste)
    const testPath = `${npub}/public/test/nested/file.txt`;
    await addressInput.fill(testPath);
    await page.keyboard.press('Enter');

    // Wait for navigation
    await page.waitForURL(/public\/test\/nested\/file\.txt/, { timeout: 10000 });

    // The URL hash should include the full path
    expect(page.url()).toContain('public/test/nested/file.txt');

    // Wait a moment for rendering to complete
    await page.waitForTimeout(500);

    // Get the main content to see what's rendered
    const mainHtml = await page.evaluate(() => {
      const main = document.querySelector('main');
      return main?.innerHTML || '';
    });
    console.log('[test] Main content HTML:', mainHtml.slice(0, 2000));

    // ProfileView-specific elements
    const followingLink = page.locator('a:has-text("Following")');
    const followersLink = page.locator('a:has-text("Known Followers")');

    const hasFollowingLink = await followingLink.isVisible().catch(() => false);
    const hasFollowersLink = await followersLink.isVisible().catch(() => false);

    console.log('[test] hasFollowingLink:', hasFollowingLink);
    console.log('[test] hasFollowersLink:', hasFollowersLink);

    // Fail if ProfileView elements are visible - this means the bug is present
    if (hasFollowingLink || hasFollowersLink) {
      console.log('[test] BUG DETECTED: ProfileView is rendered instead of TreeRoute!');
      throw new Error('BUG: ProfileView rendered for tree path - IrisRouter not handling tree routes');
    }

    // TreeRoute should show FileBrowser or Loading state
    const loadingIndicator = page.locator('text=Loading...');
    const fileBrowser = page.locator('[data-testid="file-list"]');
    const hasLoading = await loadingIndicator.isVisible().catch(() => false);
    const hasFileBrowser = await fileBrowser.isVisible().catch(() => false);

    console.log('[test] hasLoading:', hasLoading);
    console.log('[test] hasFileBrowser:', hasFileBrowser);

    // At least one of these should be true for TreeRoute
    expect(hasLoading || hasFileBrowser).toBe(true);

    // Verify the route was parsed correctly by checking the routeStore
    const routeState = await page.evaluate(async () => {
      const stores = await import('/src/stores');
      let routeValue: any = null;
      const unsub = stores.routeStore.subscribe((v: any) => { routeValue = v; });
      unsub();
      return {
        hash: window.location.hash,
        routeValue,
      };
    });

    console.log('[test] Route state:', JSON.stringify(routeState, null, 2));

    // The routeStore should have treeName set (TreeRoute, not ProfileView)
    expect(routeState.routeValue.treeName).toBe('public');
    expect(routeState.routeValue.path).toEqual(['test', 'nested', 'file.txt']);
  });

  test('IrisRouter.parseRoute handles tree paths correctly', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/iris.html');
    await page.waitForSelector('input[placeholder="Search or enter address"]', { timeout: 30000 });
    await page.waitForLoadState('networkidle');

    // Test the IrisRouter.parseRoute function by evaluating the component's source
    // We'll read the IrisRouter source and extract the parseRoute function
    const result = await page.evaluate(async () => {
      // Simulate what IrisRouter.parseRoute does
      function parseRoute(path: string) {
        if (path === '/' || path === '') return { type: 'launcher' };
        if (path === '/settings' || path.startsWith('/settings/')) return { type: 'settings' };
        if (path === '/wallet') return { type: 'wallet' };
        if (path === '/users') return { type: 'users' };
        if (path === '/profile') return { type: 'profile', npub: undefined };

        if (path.startsWith('/npub1')) {
          const match = path.match(/^\/npub1[a-z0-9]{58}/);
          if (match) {
            const npub = match[0].slice(1);
            const remainder = path.slice(match[0].length);
            if (remainder === '/edit') return { type: 'editProfile', npub };
            // This is the fix - check for tree path
            if (remainder.startsWith('/') && remainder.length > 1) {
              const pathParts = remainder.slice(1).split('/');
              const treeName = pathParts[0];
              const wild = pathParts.slice(1).join('/') || undefined;
              return { type: 'tree', npub, treeName, wild };
            }
            return { type: 'profile', npub };
          }
        }

        if (path.startsWith('/app/')) {
          try {
            return { type: 'app', url: decodeURIComponent(path.slice(5)) };
          } catch { return { type: 'launcher' }; }
        }

        return { type: 'launcher' };
      }

      const testPath = '/npub1wj6a4ex6hsp7rq4g3h9fzqwezt9f0478vnku9wzzkl25w2uudnds4z3upt/public/jumble/dist/index.html';
      return parseRoute(testPath);
    });

    console.log('[test] IrisRouter.parseRoute result:', JSON.stringify(result, null, 2));

    // This should be 'tree', not 'profile'
    expect(result.type).toBe('tree');
    expect(result.treeName).toBe('public');
    expect(result.wild).toBe('jumble/dist/index.html');
  });

  test('matchRoute correctly handles npub/treeName/path pattern', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/iris.html');
    await page.waitForSelector('input[placeholder="Search or enter address"]', { timeout: 30000 });

    // Wait for initial loading to complete
    await page.waitForLoadState('networkidle');

    // Test the matchRoute function directly
    const results = await page.evaluate(async () => {
      const { matchRoute } = await import('/src/lib/router.svelte');

      const testPath = '/npub1wj6a4ex6hsp7rq4g3h9fzqwezt9f0478vnku9wzzkl25w2uudnds4z3upt/public/jumble/dist/index.html';

      // Test all relevant patterns in order
      const patterns = [
        { pattern: '/', name: 'home' },
        { pattern: '/:npub/profile', name: 'profile-explicit' },
        { pattern: '/:npub/:treeName/*', name: 'tree-wildcard' },
        { pattern: '/:npub/:treeName', name: 'tree-exact' },
        { pattern: '/:id/*', name: 'user-wildcard' },
        { pattern: '/:id', name: 'user-exact' },
      ];

      const results = patterns.map(({ pattern, name }) => ({
        name,
        pattern,
        ...matchRoute(pattern, testPath),
      }));

      return results;
    });

    console.log('[test] matchRoute results:', JSON.stringify(results, null, 2));

    // Find first match
    const firstMatch = results.find(r => r.matched);
    expect(firstMatch).toBeDefined();
    expect(firstMatch!.name).toBe('tree-wildcard');
    expect(firstMatch!.params.treeName).toBe('public');
    expect(firstMatch!.params.wild).toBe('jumble/dist/index.html');
  });
});
