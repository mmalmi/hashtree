import { test, expect } from './fixtures';
import { disableOthersPool, setupPageErrorHandler } from './test-utils.js';

test.describe('Wallet', () => {
  test('clicking wallet icon navigates to wallet page', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);

    // Click the wallet icon in header
    await page.click('a[href="#/wallet"]');

    // Wait for wallet page to load
    await expect(page.locator('h1:has-text("Wallet")')).toBeVisible();

    // Verify tabs are present
    await expect(page.getByRole('button', { name: 'Balance' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Receive' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Redeem' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Mints' })).toBeVisible();

    // Take screenshot of wallet page
    await page.screenshot({ path: 'test-results/wallet-page.png', fullPage: true });
  });

  test('wallet page accessible via direct URL', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/#/wallet');
    await disableOthersPool(page);

    // Wait for wallet page to load
    await expect(page.locator('h1:has-text("Wallet")')).toBeVisible();

    // Balance tab should be active (has accent color)
    const balanceTab = page.getByRole('button', { name: 'Balance' });
    await expect(balanceTab).toHaveClass(/text-accent/);

    // Should show one of: "Total Balance", "Initializing", "Wallet unavailable", or "Wallet not initialized"
    const hasBalance = await page.getByText('Total Balance').isVisible().catch(() => false);
    const hasInit = await page.getByText('Initializing wallet...').isVisible().catch(() => false);
    const hasUnavailable = await page.getByText('Wallet unavailable').isVisible().catch(() => false);
    const hasNotInit = await page.getByText('Wallet not initialized').isVisible().catch(() => false);
    expect(hasBalance || hasInit || hasUnavailable || hasNotInit).toBe(true);
  });

  test('wallet page tabs are clickable', async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/#/wallet');
    await disableOthersPool(page);

    // Wait for wallet page to load
    await expect(page.locator('h1:has-text("Wallet")')).toBeVisible();

    // Click each tab - just verify they're clickable and the page doesn't error
    await page.getByRole('button', { name: 'Receive' }).click();
    await page.waitForTimeout(100);

    await page.getByRole('button', { name: 'Redeem' }).click();
    await page.waitForTimeout(100);

    await page.getByRole('button', { name: 'Mints' }).click();
    await page.waitForTimeout(100);

    await page.getByRole('button', { name: 'Balance' }).click();
    // After clicking Balance, verify Total Balance is visible (wallet initialized)
    await expect(page.getByText('Total Balance')).toBeVisible();
  });

  test('wallet receive generates invoice', async ({ page }) => {
    test.slow();
    setupPageErrorHandler(page);
    await page.goto('/#/wallet');
    await disableOthersPool(page);

    await expect(page.locator('h1:has-text("Wallet")')).toBeVisible();

    // Wait for wallet to initialize
    await expect(page.getByText('Total Balance')).toBeVisible({ timeout: 10000 });

    // Click receive tab
    await page.getByRole('button', { name: 'Receive' }).click();

    // Enter amount and generate invoice
    const amountInput = page.locator('input[type="number"]');
    await expect(amountInput).toBeVisible();
    await amountInput.fill('10');
    await page.getByRole('button', { name: 'Generate Invoice' }).click();

    // Wait for QR code to appear
    await expect(page.locator('img[alt="Lightning Invoice"]')).toBeVisible({ timeout: 15000 });
  });
});
