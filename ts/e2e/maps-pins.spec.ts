import { test, expect } from './fixtures';
import { disableOthersPool, setupPageErrorHandler, useLocalRelay } from './test-utils';

async function ensureLoggedIn(page: any) {
  const profileBtn = page.locator('button[title="My Profile (double-click for users)"]');
  await page.waitForSelector('button[title="My Profile (double-click for users)"], button:has-text("New")', { timeout: 15000 });
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

  test('persists map pins to hashtree and reloads them', async ({ page }) => {
    test.slow();

    await page.goto('/maps.html#/');
    await disableOthersPool(page);
    await useLocalRelay(page);
    await ensureLoggedIn(page);

    const addPlaceButton = page.locator('button[title="Add Place"]');
    await expect(addPlaceButton).toBeVisible({ timeout: 10000 });
    await addPlaceButton.click();

    const map = page.locator('.leaflet-container');
    await expect(map).toBeVisible({ timeout: 30000 });
    const mapBox = await map.boundingBox();
    if (!mapBox) {
      throw new Error('Map bounds not available');
    }
    await page.mouse.click(mapBox.x + mapBox.width - 60, mapBox.y + 120);

    const locationText = page.locator('text=Location:');
    await expect(locationText).toBeVisible({ timeout: 10000 });

    const placeName = `Test Pin ${Date.now()}`;
    await page.locator('#place-name').fill(placeName);
    await page.getByRole('button', { name: 'Save' }).click();

    const modal = page.getByRole('dialog', { name: 'Add Place' });
    await expect(modal).toBeHidden({ timeout: 10000 });

    const markers = page.locator('.leaflet-marker-icon');
    await expect(markers).toHaveCount(1, { timeout: 10000 });

    await page.reload();
    await disableOthersPool(page);
    await useLocalRelay(page);

    await expect(map).toBeVisible({ timeout: 30000 });
    await expect(markers).toHaveCount(1, { timeout: 20000 });
  });
});
