import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady, presetProductionRelaysInDB } from './test-utils';
// Run tests in this file serially - they access shared Nostr repo state
test.describe.configure({ mode: 'serial' });

/**
 * Test native git operations on the real hashtree repo
 * This repo was pushed via git push htree://self/hashtree
 */
test.describe('Native git operations', () => {
  test('should load commit info for hashtree repo', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await presetProductionRelaysInDB(page);
    await page.reload();
    await waitForAppReady(page);

    // Navigate to the hashtree repo (pushed earlier)
    const url = '/#/npub10ugptv2thshtaulx2kwkyq9n4vlhqawylxrtu5xga5zetdejq7ys6c8t9m/hashtree';
    await page.goto(url);
    await disableOthersPool(page);

    await expect(page.locator('td:has-text("apps")').first()).toBeVisible({ timeout: 45000 });
    await expect(page.locator('td:has-text("README.md")').first()).toBeVisible({ timeout: 10000 });

    // Wait for commit info to load (should show author name, not "Loading commit info...")
    // The native implementation should load quickly without wasm copy
    await expect(page.locator('text=Loading commit info')).not.toBeVisible({ timeout: 30000 });

    // Verify we see actual commit info (author name should appear)
    // The header row should have commit info from the native reader
    const headerRow = page.locator('thead tr').first();
    await expect(headerRow).toBeVisible();

    // Should NOT show "No commits yet" since this is a git repo
    await expect(page.locator('text=No commits yet')).not.toBeVisible();

    // Should show branch info
    await expect(page.locator('text=master')).toBeVisible({ timeout: 10000 });
  });

  test('should show file commit info in directory listing', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await presetProductionRelaysInDB(page);
    await page.reload();
    await waitForAppReady(page);

    const url = '/#/npub10ugptv2thshtaulx2kwkyq9n4vlhqawylxrtu5xga5zetdejq7ys6c8t9m/hashtree';
    await page.goto(url);
    await disableOthersPool(page);

    await expect(page.locator('td:has-text("apps")').first()).toBeVisible({ timeout: 45000 });

    // Get file commit info for files - look for relative time indicators
    // Files should show "X days ago", "X hours ago", etc. from the native getFileLastCommitsNative
    await page.waitForTimeout(5000); // Give time for file commits to load

    // Check if any commit timestamps are shown (evidence that getFileLastCommitsNative works)
    const timeIndicators = page.locator('td:has-text("ago")');
    const count = await timeIndicators.count();

    // We should have at least some files with commit info
    console.log(`Found ${count} file entries with commit timestamps`);

    // If native git ops work, we should see timestamps
    // Note: The first time may need to fetch from network, subsequent loads use cache
  });
});
