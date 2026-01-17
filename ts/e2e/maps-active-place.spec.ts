import { test, expect } from './fixtures';
import { disableOthersPool, setupPageErrorHandler } from './test-utils';

async function ensureLoggedIn(page: any) {
  await page.waitForSelector('button[title="My Profile (double-click for users)"], button:has-text("New")', { timeout: 15000 });
  const profileBtn = page.locator('button[title="My Profile (double-click for users)"]');
  if (await profileBtn.isVisible().catch(() => false)) {
    return;
  }
  const newButton = page.getByRole('button', { name: 'New' });
  await newButton.click();
  await expect(profileBtn).toBeVisible({ timeout: 15000 });
}

test.describe('Iris Maps', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('tracks active search place in hash and panel', async ({ page }) => {
    test.slow();

    await page.goto('/maps.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    const input = page.getByTestId('search-input');
    await expect(input).toBeVisible({ timeout: 10000 });
    await input.fill('Helsinki');

    const results = page.getByTestId('search-results');
    await expect(results).toBeVisible({ timeout: 20000 });

    const firstResult = results.locator('button').first();
    await expect(firstResult).toBeVisible({ timeout: 20000 });
    const name = (await firstResult.locator('div >> nth=0').textContent())?.trim() || 'Helsinki';
    await firstResult.click();

    const panel = page.locator('aside');
    await expect(panel).toBeVisible({ timeout: 10000 });
    await expect(panel).toContainText(name);

    await page.waitForFunction(() => window.location.hash.includes('place='));

    await page.reload();
    await disableOthersPool(page);
    await expect(panel).toBeVisible({ timeout: 10000 });
    await expect(panel).toContainText(name);
  });
});
