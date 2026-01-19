import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('Releases', () => {
  test.use({ viewport: { width: 1280, height: 720 } });
  test.describe.configure({ timeout: 90000 });

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('create, edit, and delete a release', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000 });

    const route = await page.evaluate(() => {
      const hash = window.location.hash.slice(1);
      const qIdx = hash.indexOf('?');
      const path = qIdx !== -1 ? hash.slice(0, qIdx) : hash;
      const parts = path.split('/').filter(Boolean);
      return { npub: parts[0], treeName: parts[1] };
    });

    await page.goto(`/#/${route.npub}/${route.treeName}?tab=releases`);
    await expect(page.locator('text=Loading releases...')).not.toBeVisible({ timeout: 20000 });
    await expect(page.locator('text=No releases yet')).toBeVisible({ timeout: 20000 });

    await page.getByRole('button', { name: 'New Release' }).click();
    await page.locator('#release-title').fill('Iris Files v0.1');
    await page.locator('#release-tag').fill('v0.1.0');
    await page.locator('#release-commit').fill('deadbeefdeadbeefdeadbeefdeadbeefdeadbeef');
    await page.locator('#release-notes').fill('## Changes\n- initial release');

    const assetInput = page.locator('#release-assets');
    await assetInput.setInputFiles({
      name: 'artifact.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('artifact content'),
    });

    await page.getByRole('button', { name: 'Create Release' }).click();
    await expect(page.locator('a:has-text("Iris Files v0.1")')).toBeVisible({ timeout: 20000 });

    await page.locator('a:has-text("Iris Files v0.1")').click();
    await expect(page.locator('h1:has-text("Iris Files v0.1")')).toBeVisible({ timeout: 20000 });
    await expect(page.locator('text=initial release')).toBeVisible({ timeout: 20000 });
    await expect(page.locator('a:has-text("artifact.txt")')).toBeVisible({ timeout: 20000 });

    await page.getByRole('button', { name: 'Edit' }).click();
    await page.locator('#release-title').fill('Iris Files v0.1.1');
    await page.locator('#release-notes').fill('## Changes\n- updated release');
    await page.getByRole('button', { name: 'Save Release' }).click();

    await expect(page.locator('h1:has-text("Iris Files v0.1.1")')).toBeVisible({ timeout: 20000 });
    await expect(page.locator('text=updated release')).toBeVisible({ timeout: 20000 });
    await expect(page.locator('a:has-text("artifact.txt")')).toBeVisible({ timeout: 20000 });

    page.once('dialog', dialog => dialog.accept());
    await page.getByRole('button', { name: 'Delete' }).click();

    await page.waitForURL(/tab=releases/, { timeout: 20000 });
    await expect(page.locator('text=No releases yet')).toBeVisible({ timeout: 20000 });
  });
});
