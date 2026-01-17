import { test, expect } from './fixtures';
import { setupPageErrorHandler, configureBlossomServers, navigateToPublicFolder, disableOthersPool, waitForAppReady } from './test-utils.js';

test.describe('Settings page', () => {
  test('can navigate to settings page', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await waitForAppReady(page);

    const settingsLink = page.locator('a[href="#/settings"]');
    await expect(settingsLink).toBeVisible({ timeout: 5000 });
    await settingsLink.click();

    await page.waitForURL(/#\/settings/, { timeout: 5000 });

    await expect(page.getByRole('button', { name: 'Servers' })).toBeVisible({ timeout: 5000 });
  });

  test('can add and remove blossom server', async ({ page }) => {
    test.setTimeout(60000);
    setupPageErrorHandler(page);
    await page.goto('/#/settings');

    await expect(page.locator('text=File Servers')).toBeVisible({ timeout: 10000 });

    const blossomSection = page.locator('div').filter({ hasText: /^File Servers/ }).first();
    const editBtn = blossomSection.locator('button', { hasText: 'Edit' });
    await expect(editBtn).toBeVisible({ timeout: 5000 });
    await editBtn.click();

    const urlInput = page.locator('input[placeholder="https://blossom.example.com"]');
    await expect(urlInput).toBeVisible({ timeout: 5000 });

    const testServerUrl = 'https://test-blossom.example.com';
    await urlInput.fill(testServerUrl);

    const addBtn = page.locator('button', { hasText: 'Add' });
    await expect(addBtn).toBeVisible({ timeout: 5000 });
    await addBtn.click({ timeout: 5000 });

    await expect(page.locator('text=test-blossom.example.com')).toBeVisible({ timeout: 5000 });

    const serverSpan = page.locator('span:has-text("test-blossom.example.com")');
    const serverRow = serverSpan.locator('xpath=..');
    const removeBtn = serverRow.getByRole('button', { name: 'Remove server' });
    await expect(removeBtn).toBeVisible({ timeout: 5000 });
    await removeBtn.click();

    await expect(page.locator('text=test-blossom.example.com')).not.toBeVisible({ timeout: 5000 });
  });

  test('can toggle blossom server read/write', async ({ page }) => {
    test.setTimeout(60000);
    setupPageErrorHandler(page);
    await page.goto('/#/settings');
    await waitForAppReady(page);
    await configureBlossomServers(page);

    await expect(page.locator('text=File Servers')).toBeVisible({ timeout: 10000 });
    
    const editBtn = page.locator('h3:has-text("File Servers")').locator('..').locator('button', { hasText: 'Edit' });
    await expect(editBtn).toBeVisible({ timeout: 5000 });
    await editBtn.click();

    const firstServerRow = page.locator('div').filter({ hasText: /upload\.iris\.to/ }).first();
    await expect(firstServerRow).toBeVisible({ timeout: 5000 });

    const readCheckbox = firstServerRow.locator('input[type="checkbox"]').first();
    await expect(readCheckbox).toBeVisible({ timeout: 5000 });

    const wasChecked = await readCheckbox.isChecked();
    await readCheckbox.click();

    const isNowChecked = await readCheckbox.isChecked();
    expect(isNowChecked).toBe(!wasChecked);

    await readCheckbox.click();
    expect(await readCheckbox.isChecked()).toBe(wasChecked);
  });

  test('settings page does not freeze with rapid interactions', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/#/settings');

    await expect(page.getByRole('button', { name: 'Servers' })).toBeVisible({ timeout: 10000 });

    for (let i = 0; i < 5; i++) {
      const editBtn = page.locator('button', { hasText: /Edit|Done/ }).first();
      await expect(editBtn).toBeVisible({ timeout: 2000 });
      const currentLabel = (await editBtn.textContent()) || '';
      await editBtn.click();
      const expectedLabel = currentLabel.includes('Edit') ? /Done/ : /Edit/;
      await expect(editBtn).toHaveText(expectedLabel, { timeout: 2000 });
    }

    await expect(page.getByRole('button', { name: 'Storage' })).toBeVisible({ timeout: 5000 });
  });

  test('displays storage stats with non-zero size after uploading', async ({ page }) => {
    test.slow();
    setupPageErrorHandler(page);

    await page.goto('/');
    await disableOthersPool(page);

    await navigateToPublicFolder(page);

    const testContent = 'Test file content for storage test - '.repeat(100);
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles({
      name: 'storage-test.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from(testContent),
    });

    const uploadedFileLink = page.locator('[data-testid="file-list"]').getByRole('link', { name: /storage-test\.txt/ }).first();
    await expect(uploadedFileLink).toBeVisible({ timeout: 10000 });

    const settingsLink = page.locator('a[href="#/settings"]');
    await expect(settingsLink).toBeVisible({ timeout: 5000 });
    await settingsLink.click();
    await page.waitForURL(/#\/settings/, { timeout: 5000 });

    await page.getByRole('button', { name: 'Storage' }).click();

    await expect(page.locator('text=Local Storage')).toBeVisible({ timeout: 5000 });

    const sizeElement = page.getByTestId('storage-size');
    await expect(sizeElement).toBeVisible({ timeout: 5000 });

    await page.waitForFunction(
      () => {
        const el = document.querySelector('[data-testid="storage-size"]');
        return el && el.textContent && el.textContent !== '0 B' && el.textContent !== '0';
      },
      { timeout: 15000 }
    );

    const sizeText = await sizeElement.textContent();
    expect(sizeText).not.toBe('0 B');

    const itemsElement = page.getByTestId('storage-items');
    const itemsText = await itemsElement.textContent();
    expect(parseInt(itemsText || '0')).toBeGreaterThan(0);

    const limitElement = page.getByTestId('storage-limit');
    const limitText = await limitElement.textContent();
    expect(limitText).toContain('GB');
  });

  test('can edit storage limit', async ({ page }) => {
    test.setTimeout(60000);
    setupPageErrorHandler(page);
    await page.goto('/#/settings');

    await page.getByRole('button', { name: 'Storage' }).click();

    await expect(page.locator('text=Local Storage')).toBeVisible({ timeout: 10000 });

    const storageSectionHeader = page.locator('h3:has-text("Local Storage")');
    await expect(storageSectionHeader).toBeVisible({ timeout: 5000 });

    const editBtn = storageSectionHeader.locator('..').getByRole('button', { name: 'Edit' });
    await expect(editBtn).toBeVisible({ timeout: 5000 });
    await editBtn.click();

    const limitInput = page.getByTestId('storage-limit-input');
    await expect(limitInput).toBeVisible({ timeout: 5000 });

    await limitInput.fill('500');
    await limitInput.blur();

    const doneBtn = storageSectionHeader.locator('..').getByRole('button', { name: 'Done' });
    await doneBtn.click();

    const limitElement = page.getByTestId('storage-limit');
    await expect(limitElement).toContainText('500');
  });
});
