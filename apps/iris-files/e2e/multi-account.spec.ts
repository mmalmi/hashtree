import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady, ensureLoggedIn, clearAllStorage, safeReload } from './test-utils.js';

// Helper to navigate to accounts page
async function navigateToAccountsPage(page: any) {
  const modalBackdrop = page.locator('[data-modal-backdrop], div.fixed.inset-0.bg-black\\/70').first();
  if (await modalBackdrop.isVisible().catch(() => false)) {
    await page.keyboard.press('Escape').catch(() => {});
    await modalBackdrop.waitFor({ state: 'hidden', timeout: 1000 }).catch(() => {});
    if (await modalBackdrop.isVisible().catch(() => false)) {
      await modalBackdrop.click({ position: { x: 5, y: 5 } }).catch(() => {});
      await modalBackdrop.waitFor({ state: 'hidden', timeout: 5000 }).catch(() => {});
    }
  }

  // Double-click on avatar to go to accounts
  const profileButton = page.locator('header button[title*="My Profile"]').first();
  await expect(profileButton).toBeVisible({ timeout: 10000 });
  await profileButton.dblclick({ timeout: 15000 });

  // Should be on accounts page
  await page.waitForURL(/#\/users/, { timeout: 30000 });
  await waitForAppReady(page, 30000);
  await page.evaluate(() => window.dispatchEvent(new HashChangeEvent('hashchange')));
  await expect(page.getByRole('heading', { name: 'Users' })).toBeVisible({ timeout: 30000 });
  await expect(page.getByRole('button', { name: 'Add with Secret Key' })).toBeVisible({ timeout: 30000 });
}

// Generate a test nsec for adding accounts
function generateTestNsec(): string {
  // This is a deterministic nsec for testing purposes only
  return 'nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5';
}

test.describe('Multi-Account Management', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);

    await page.goto('/');
    await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests

    await clearAllStorage(page);
    await safeReload(page, { waitUntil: 'load', timeoutMs: 60000 });
    await waitForAppReady(page, 60000); // Wait for page to load after reload
    await disableOthersPool(page); // Re-apply after reload
    await ensureLoggedIn(page, 30000);
  });

  test('should navigate to accounts page via double-click on avatar', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Verify accounts page elements
    await expect(page.getByRole('heading', { name: 'Users' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Add with Secret Key' })).toBeVisible();
  });

  test('should show initial auto-generated account', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Should show 1 account initially (auto-generated) with check mark
    await expect(page.locator('[data-testid="account-item"]').first()).toBeVisible({ timeout: 5000 });

    // Current account should be shown with check mark
    await expect(page.locator('span.i-lucide-check-circle').first()).toBeVisible();
  });

  test('should add account with nsec', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Click "Add with Secret Key" button
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();

    // Input should be visible
    const nsecInput = page.locator('input[placeholder="nsec1..."]');
    await expect(nsecInput).toBeVisible();

    // Enter a valid test nsec
    await nsecInput.fill(generateTestNsec());
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Should now show 2 accounts - look for 2 account rows (each has an avatar link)
    const accountRows = page.locator('.rounded-lg').filter({ has: page.locator('[class*="i-lucide-"]') });
    await expect(accountRows).toHaveCount(2, { timeout: 5000 });
  });

  test('should show error for invalid nsec', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Click "Add with Secret Key" button
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();

    // Enter invalid nsec
    const nsecInput = page.locator('input[placeholder="nsec1..."]');
    await nsecInput.fill('invalid-nsec');
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Should show error
    await expect(page.getByText('Invalid nsec')).toBeVisible();
  });

  test('should cancel adding nsec account', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Click "Add with Secret Key" button
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();

    // Input should be visible
    await expect(page.locator('input[placeholder="nsec1..."]')).toBeVisible();

    // Click Cancel
    await page.getByRole('button', { name: 'Cancel' }).click();

    // Input should be hidden
    await expect(page.locator('input[placeholder="nsec1..."]')).not.toBeVisible();
  });

  test('should switch between accounts', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Add a second account
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();
    await page.locator('input[placeholder="nsec1..."]').fill(generateTestNsec());
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Should have 2 accounts now - click on the non-active account row to switch
    // The non-active account row has bg-surface-1 (not bg-surface-2 like active)
    const nonActiveRow = page.locator('.rounded-lg.bg-surface-1').first();
    await expect(nonActiveRow).toBeVisible({ timeout: 5000 });
    await nonActiveRow.click();

    // After switching, the previously non-active row should now be active (have check-circle)
    // The row we clicked should now have bg-surface-2 (active styling)
    // And there should be a new non-active row (the previous account)
    const activeRows = page.locator('.rounded-lg.bg-surface-2');
    await expect(activeRows).toHaveCount(1, { timeout: 5000 });

    // The check-circle should be visible on the new active account
    await expect(page.locator('span.i-lucide-check-circle')).toBeVisible();
  });

  test('should not allow removing last account', async ({ page }) => {
    await navigateToAccountsPage(page);

    // With only 1 account, the remove button (trash icon) should not be visible
    await expect(page.locator('button[title="Remove account"]')).not.toBeVisible();
  });

  test('should remove account when multiple exist', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Add a second account first
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();
    await page.locator('input[placeholder="nsec1..."]').fill(generateTestNsec());
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Should have 2 accounts now - non-active row should be visible
    const nonActiveRow = page.locator('.rounded-lg.bg-surface-1').first();
    await expect(nonActiveRow).toBeVisible({ timeout: 5000 });

    // Find the remove button (trash icon) on non-active account
    const removeButton = page.locator('button[title="Remove account"]').first();
    await expect(removeButton).toBeVisible();

    // Click to show confirmation
    await removeButton.click();

    // Should show confirmation buttons - use exact: true to avoid matching "Remove account"
    const confirmRemoveButton = page.getByRole('button', { name: 'Remove', exact: true });
    await expect(confirmRemoveButton).toBeVisible({ timeout: 5000 });

    // Confirm removal
    await confirmRemoveButton.click();

    // Should now have only 1 account - no non-active row visible
    await expect(page.locator('.rounded-lg.bg-surface-1')).not.toBeVisible({ timeout: 5000 });
  });

  test('should cancel account removal', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Add a second account first
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();
    await page.locator('input[placeholder="nsec1..."]').fill(generateTestNsec());
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Find the remove button - use first() since both accounts have one
    const removeButton = page.locator('button[title="Remove account"]').first();
    await removeButton.click();

    // Click Cancel instead of Remove
    await page.getByRole('button', { name: 'Cancel' }).last().click();

    // Should still have 2 accounts - non-active row should be visible
    await expect(page.locator('.rounded-lg.bg-surface-1').first()).toBeVisible({ timeout: 5000 });
  });

  test('should go back from accounts page', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Click back button (chevron-left icon)
    await page.locator('button:has(span.i-lucide-chevron-left)').click();

    // Should be back on profile/tree list page
    await expect(page.getByTestId('file-list')).toBeVisible({ timeout: 10000 });
  });

  test('should persist accounts across page reload', async ({ page }) => {
    await navigateToAccountsPage(page);

    // Add a second account
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();
    await page.locator('input[placeholder="nsec1..."]').fill(generateTestNsec());
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Should have 2 accounts - non-active row should be visible
    await expect(page.locator('.rounded-lg.bg-surface-1').first()).toBeVisible({ timeout: 5000 });

    // Reload the page
    await safeReload(page, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(page, 60000);

    // Navigate back to accounts page
    await page.locator('header button[title*="My Profile"]').dblclick();

    // Should still have 2 accounts - non-active row should be visible
    await expect(page.locator('.rounded-lg.bg-surface-1').first()).toBeVisible({ timeout: 5000 });
  });

  test('should not add duplicate account', async ({ page }) => {
    await navigateToAccountsPage(page);

    const testNsec = generateTestNsec();

    // Add first account
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();
    await page.locator('input[placeholder="nsec1..."]').fill(testNsec);
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Should have 2 accounts - non-active row should be visible
    await expect(page.locator('.rounded-lg.bg-surface-1').first()).toBeVisible({ timeout: 5000 });

    // Try to add the same account again
    await page.getByRole('button', { name: 'Add with Secret Key' }).click();
    await page.locator('input[placeholder="nsec1..."]').fill(testNsec);
    await page.getByRole('button', { name: 'Add', exact: true }).click();

    // Should show error about duplicate
    await expect(page.getByText('Account already added')).toBeVisible();
  });

  // This feature was not implemented in the Svelte port - removing test
  // Account type indicator (nsec/extension) is not shown in the UI
});
