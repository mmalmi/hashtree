import { test, expect } from './fixtures';

test.describe('Users Page', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    // Wait for page to load
    await page.waitForTimeout(500);
  });

  // Helper to close any open modals
  async function closeModals(page) {
    const cancelButton = page.getByRole('button', { name: 'Cancel' });
    if (await cancelButton.isVisible({ timeout: 500 }).catch(() => false)) {
      await cancelButton.click();
      await page.waitForTimeout(200);
    }
  }

  test('should show Generate New Account button on users page', async ({ page }) => {
    // First login to be able to access users page
    await page.getByRole('button', { name: /New/i }).click();
    await page.waitForTimeout(1000);

    // Navigate to users page via double-click or URL
    await page.goto('/#/users');
    await page.waitForTimeout(500);

    // Should see the Generate New Account button
    await expect(page.getByTestId('generate-new-account')).toBeVisible();
    await expect(page.getByTestId('generate-new-account')).toHaveText(/Generate New Account/);
  });

  test('should show Add with Secret Key button on users page', async ({ page }) => {
    // First login
    await page.getByRole('button', { name: /New/i }).click();
    await page.waitForTimeout(1000);

    // Navigate to users page
    await page.goto('/#/users');
    await page.waitForTimeout(500);

    // Should see the Add with Secret Key button
    await expect(page.getByTestId('add-with-nsec')).toBeVisible();
    await expect(page.getByTestId('add-with-nsec')).toHaveText(/Add with Secret Key/);
  });

  test('should generate new account when clicking Generate New Account', async ({ page }) => {
    // First login
    await page.getByRole('button', { name: /New/i }).click();
    await page.waitForTimeout(1500);
    await closeModals(page);

    // Navigate to users page
    await page.goto('/#/users');
    await page.waitForTimeout(500);
    await closeModals(page);

    // Click Generate New Account
    await page.getByTestId('generate-new-account').click();
    await page.waitForTimeout(1000);

    // Should stay on users page (not navigate away)
    expect(page.url()).toContain('users');

    // Should see at least 2 accounts (original + generated)
    const accountItems = page.getByTestId('account-item');
    await expect(accountItems).toHaveCount(2);
  });

  test('should show account list with avatar and name', async ({ page }) => {
    // First login
    await page.getByRole('button', { name: /New/i }).click();
    await page.waitForTimeout(1500);
    await closeModals(page);

    // Navigate to users page
    await page.goto('/#/users');
    await page.waitForTimeout(500);

    // Should see account cards
    const accountCard = page.getByTestId('account-item').first();
    await expect(accountCard).toBeVisible();

    // Should have avatar (img or svg)
    const avatar = accountCard.locator('img, svg').first();
    await expect(avatar).toBeVisible();
  });

  test('should allow switching between accounts', async ({ page }) => {
    // First login
    await page.getByRole('button', { name: /New/i }).click();
    await page.waitForTimeout(1500);
    await closeModals(page);

    // Navigate to users page
    await page.goto('/#/users');
    await page.waitForTimeout(500);
    await closeModals(page);

    // Generate another account
    await page.getByTestId('generate-new-account').click();
    await page.waitForTimeout(1000);

    // Should have 2 accounts now
    const accountItems = page.getByTestId('account-item');
    await expect(accountItems).toHaveCount(2);

    // Click on second account to switch
    await accountItems.nth(1).click();
    await page.waitForTimeout(500);

    // Verify we still have 2 accounts after switching
    await expect(page.getByTestId('account-item')).toHaveCount(2);
  });

  test('should show secret key input when clicking Add with Secret Key', async ({ page }) => {
    // First login
    await page.getByRole('button', { name: /New/i }).click();
    await page.waitForTimeout(1500);
    await closeModals(page);

    // Navigate to users page
    await page.goto('/#/users');
    await page.waitForTimeout(500);
    await closeModals(page);

    // Click Add with Secret Key
    await page.getByTestId('add-with-nsec').click();
    await page.waitForTimeout(300);

    // Should show input field
    await expect(page.locator('input[placeholder="nsec1..."]')).toBeVisible();

    // Should show Add and Cancel buttons
    await expect(page.getByRole('button', { name: 'Add', exact: true })).toBeVisible();
  });

  test('should navigate back to home when clicking Back button', async ({ page }) => {
    // First login
    await page.getByRole('button', { name: /New/i }).click();
    await page.waitForTimeout(1500);
    await closeModals(page);

    // Navigate to users page
    await page.goto('/#/users');
    await page.waitForTimeout(500);
    await closeModals(page);

    // Click Back button (chevron-left icon)
    await page.locator('button:has(span.i-lucide-chevron-left)').click();
    await page.waitForTimeout(500);

    // Should be back at home
    expect(page.url()).toContain('#/');
    expect(page.url()).not.toContain('users');
  });
});
