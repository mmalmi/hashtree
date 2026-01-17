import { test, expect } from './fixtures';
import { setupPageErrorHandler, waitForAppReady, disableOthersPool } from './test-utils.js';

test.describe('Mobile Search', () => {
  test.setTimeout(30000);

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    // Set mobile viewport
    await page.setViewportSize({ width: 375, height: 667 });
    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page);
  });

  test('should show search icon on mobile', async ({ page }) => {
    // On mobile, the search icon should be visible
    await expect(page.getByRole('button', { name: 'Search' })).toBeVisible();
  });

  test('should expand search when icon is clicked', async ({ page }) => {
    // Click the search icon
    await page.getByRole('button', { name: 'Search' }).click();

    // Search input should now be visible and expanded (use :visible to get the mobile search, not the hidden desktop one)
    await expect(page.locator('header input[placeholder="Search"]:visible')).toBeVisible();
  });

  test('should collapse search when clicking close button', async ({ page }) => {
    // Click the search icon to expand
    await page.getByRole('button', { name: 'Search' }).click();
    await expect(page.locator('header input[placeholder="Search"]:visible')).toBeVisible();

    // Click the close button (X icon)
    await page.getByRole('button', { name: 'Close search' }).click();

    // Search should collapse back to icon
    await expect(page.getByRole('button', { name: 'Search' })).toBeVisible();
  });

  test('should hide search on desktop', async ({ page }) => {
    // Desktop viewport
    await page.setViewportSize({ width: 1200, height: 800 });

    // Regular search should be visible on desktop
    await expect(page.locator('header input[placeholder="Search"]')).toBeVisible();

    // Mobile search icon should be hidden on desktop
    await expect(page.getByRole('button', { name: 'Search' })).toBeHidden();
  });
});
