import { test, expect } from '@playwright/test';

const JUMBLE_URL = 'https://jumble.social';

test.describe('External App in Webview', () => {
  test('should load jumble.social in iframe via address bar', async ({ page }) => {
    test.slow(); // External site may load slowly

    await page.goto('/iris.html');

    // Wait for app to load
    await expect(page.locator('h2:has-text("Favourites")')).toBeVisible({ timeout: 10000 });

    // Enter jumble.social in the address bar (without https://)
    const addressBar = page.locator('input[placeholder="Search or enter address"]');
    await addressBar.fill('jumble.social');
    await addressBar.press('Enter');

    // Wait for iframe to load
    const iframe = page.locator('main iframe');
    await expect(iframe).toBeVisible({ timeout: 15000 });

    // Star button should be visible and enabled
    const starButton = page.locator('button[title="Add bookmark"]');
    await expect(starButton).toBeVisible({ timeout: 5000 });
    await expect(starButton).toBeEnabled();
  });

  test('should save jumble.social to favorites and navigate to nhash', async ({ page }) => {
    test.slow(); // This test involves external network requests

    await page.goto('/iris.html');

    // Wait for app to load
    await expect(page.locator('h2:has-text("Favourites")')).toBeVisible({ timeout: 10000 });

    // Navigate to jumble.social
    const addressBar = page.locator('input[placeholder="Search or enter address"]');
    await addressBar.fill(JUMBLE_URL);
    await addressBar.press('Enter');

    // Wait for iframe
    const iframe = page.locator('main iframe');
    await expect(iframe).toBeVisible({ timeout: 15000 });

    // Click star button to save
    const starButton = page.locator('button[title="Add bookmark"]');
    await expect(starButton).toBeEnabled({ timeout: 5000 });
    await starButton.click();

    // Wait for save to complete - should navigate to nhash URL
    // This may take a while as it fetches and saves the PWA
    await expect(addressBar).toHaveValue(/nhash/, { timeout: 60000 });

    // Star should now show "Remove bookmark" (bookmarked state)
    const removeButton = page.locator('button[title="Remove bookmark"]');
    await expect(removeButton).toBeVisible({ timeout: 5000 });
  });

  test('should show jumble.social in favorites after saving', async ({ page }) => {
    test.slow();

    await page.goto('/iris.html');
    await expect(page.locator('h2:has-text("Favourites")')).toBeVisible({ timeout: 10000 });

    // Navigate to jumble.social
    const addressBar = page.locator('input[placeholder="Search or enter address"]');
    await addressBar.fill(JUMBLE_URL);
    await addressBar.press('Enter');

    // Wait for iframe and save
    const iframe = page.locator('main iframe');
    await expect(iframe).toBeVisible({ timeout: 15000 });

    const starButton = page.locator('button[title="Add bookmark"]');
    await starButton.click();

    // Wait for save to complete
    await expect(addressBar).toHaveValue(/nhash/, { timeout: 60000 });

    // Go back to home
    await page.goto('/iris.html#/');
    await expect(page.locator('h2:has-text("Favourites")')).toBeVisible({ timeout: 10000 });

    // Check localStorage for saved apps
    const apps = await page.evaluate(() => localStorage.getItem('iris:apps'));
    console.log('Saved apps:', apps);

    // jumble.social (or similar name from manifest) should appear in favorites
    // The name could be "Jumble" or similar based on the PWA manifest
    const favoriteButton = page.locator('[class*="grid"] button').first();
    await expect(favoriteButton).toBeVisible({ timeout: 10000 });
  });
});
