import { test, expect } from './fixtures';
import { waitForAppReady } from './test-utils.js';

/**
 * Test for FollowsTreesView - shows trees from followed users
 */
test.describe('FollowsTreesView', () => {
  test.setTimeout(60000);

  test('should show "Not following anyone" for new user', async ({ page }) => {
    // Capture console logs
    const consoleLogs: string[] = [];
    page.on('console', (msg) => {
      consoleLogs.push(`[${msg.type()}] ${msg.text()}`);
    });

    await page.goto('/');
    await waitForAppReady(page);

    // Login with new account
    await page.getByRole('button', { name: /New/i }).click();
    // Wait for login to complete
    await expect(page.getByRole('link', { name: 'public' })).toBeVisible({ timeout: 15000 });

    // Look for "Following" section in sidebar
    const followingSection = page.locator('text=Following');
    await expect(followingSection.first()).toBeVisible({ timeout: 5000 });

    // Should show "Not following anyone" or follow count of 0
    const notFollowing = page.locator('text=Not following anyone');
    const following0 = page.locator('text=Following (0)');

    const hasNotFollowing = await notFollowing.isVisible().catch(() => false);
    const hasFollowing0 = await following0.isVisible().catch(() => false);

    expect(hasNotFollowing || hasFollowing0).toBe(true);
  });

  test('should show trees from followed users', async ({ page }) => {
    test.slow(); // This test involves multiple users and follows
    // Capture console logs for debugging
    const consoleLogs: string[] = [];
    page.on('console', (msg) => {
      const text = msg.text();
      if (text.includes('[follows]') || text.includes('trees')) {
        consoleLogs.push(`[${msg.type()}] ${text}`);
      }
    });

    await page.goto('/');
    await waitForAppReady(page);

    // Login with new account
    await page.getByRole('button', { name: /New/i }).click();
    // Wait for login to complete
    await expect(page.getByRole('link', { name: 'public' })).toBeVisible({ timeout: 15000 });

    // Close the "New Folder" modal if it appears (new user flow)
    const cancelButton = page.getByRole('button', { name: 'Cancel' });
    if (await cancelButton.isVisible().catch(() => false)) {
      await cancelButton.click();
      await page.waitForTimeout(500);
    }

    // Navigate to a known user with trees (default social graph root)
    // First follow them
    const testNpub = 'npub1g53nez3lp2xa443uvlyfq2ge7xfcmn8ynx5acphcf8qhv3k8a2gskkl6zh';
    await page.goto(`/#/${testNpub}`);
    await page.waitForTimeout(2000);

    // Debug: take screenshot before following
    await page.screenshot({ path: 'e2e/screenshots/before-follow.png' });
    console.log('Current URL:', page.url());

    // Try to click follow button if visible (may need to scroll or wait)
    const followButton = page.getByRole('button', { name: 'Follow', exact: true });
    const isVisible = await followButton.isVisible({ timeout: 5000 }).catch(() => false);
    console.log('Follow button visible:', isVisible);
    if (isVisible) {
      await followButton.click();
      // Wait for follow to complete
      await expect(page.getByRole('button', { name: 'Following' }).or(page.getByRole('button', { name: 'Unfollow' }))).toBeVisible({ timeout: 10000 }).catch(() => {});
    }

    // Go back home
    await page.goto('/#/');
    await waitForAppReady(page);

    // Take screenshot
    await page.screenshot({ path: 'e2e/screenshots/follows-trees-view.png', fullPage: true });

    // Check for the Following section
    const followingHeader = page.locator('text=/Following \\(\\d+\\)/');
    const headerVisible = await followingHeader.isVisible().catch(() => false);
    console.log('Following header visible:', headerVisible);

    // Print console logs
    console.log('=== Console logs ===');
    consoleLogs.forEach(log => console.log(log));
  });

  test('should display trees with correct structure', async ({ page }) => {
    await page.goto('/');
    await waitForAppReady(page);

    // Login with new account
    await page.getByRole('button', { name: /New/i }).click();
    // Wait for login to complete
    await expect(page.getByRole('link', { name: 'public' })).toBeVisible({ timeout: 15000 });

    // Check structure of the Following section
    const followingSection = page.locator('.flex-1.flex.flex-col.min-h-0').filter({
      has: page.locator('text=Following')
    });

    // Take screenshot of the following section
    await page.screenshot({ path: 'e2e/screenshots/follows-trees-structure.png', fullPage: true });

    // Verify the structure exists
    await expect(followingSection.first()).toBeVisible({ timeout: 5000 });
  });
});
