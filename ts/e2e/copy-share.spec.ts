import { test, expect } from './fixtures';
import { disableOthersPool, ensureLoggedIn, waitForAppReady } from './test-utils.js';

test.describe('Copy and Share functionality', () => {
  // Grant clipboard permissions
  test.use({
    permissions: ['clipboard-read', 'clipboard-write'],
  });

  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests
    await ensureLoggedIn(page, 30000);
  });

  // Helper to close any open modals
  async function closeModals(page) {
    // Try multiple times to close modals (some might open after others close)
    for (let i = 0; i < 3; i++) {
      const backdrop = page.locator('.fixed.inset-0');
      if (await backdrop.first().isVisible({ timeout: 300 }).catch(() => false)) {
        // Click the backdrop at edge to close
        await backdrop.first().click({ position: { x: 5, y: 5 }, force: true });
        await backdrop.first().waitFor({ state: 'hidden', timeout: 1000 }).catch(() => {});
      } else {
        break;
      }
    }
  }

  // Helper to navigate to own profile by clicking the avatar in NostrLogin
  async function goToProfile(page) {
    // Close any modals first
    await closeModals(page);

    // Click on the user avatar/button in the header to go to profile
    const avatarLink = page.locator('header a[href*="profile"]');
    if (await avatarLink.isVisible({ timeout: 1000 }).catch(() => false)) {
      await avatarLink.click();
    } else {
      // Fallback: get npub from window and navigate directly
      const npub = await page.evaluate(() => (window as any).__nostrStore?.getState()?.npub);
      await page.goto(`/#/${npub}/profile`);
    }
    await page.waitForURL(/\/profile/, { timeout: 5000 }).catch(() => {});
    // Close any modals that may have opened
    await closeModals(page);
  }

  test.describe('CopyText in ProfileView', () => {
    test('should show npub with copy button in profile', async ({ page }) => {
      await goToProfile(page);

      // Should see the copy button for npub (use first() for desktop layout with sidebar)
      const copyButton = page.getByTestId('copy-npub').first();
      await expect(copyButton).toBeVisible();

      // Should show truncated npub (first 8 + ... + last 4)
      const buttonText = await copyButton.textContent();
      expect(buttonText).toContain('npub');
      expect(buttonText).toContain('...');
    });

    test('should show check icon after clicking copy', async ({ page }) => {
      test.slow(); // Timeout-based wait needs more time
      await goToProfile(page);

      // Click copy button (use first() for desktop layout with sidebar)
      const copyButton = page.getByTestId('copy-npub').first();
      await copyButton.click();

      // Should show check icon
      await expect(copyButton.locator('.i-lucide-check')).toBeVisible();

      // After 2 seconds, should show copy icon again
      await expect(copyButton.locator('.i-lucide-copy')).toBeVisible();
    });
  });

  test.describe('ShareModal', () => {
    test('should open share modal when clicking share button', async ({ page }) => {
      await goToProfile(page);

      // Click share button (use first() for desktop layout)
      await page.getByRole('button', { name: 'Share' }).first().click();
      await expect(page.getByTestId('share-modal')).toBeVisible();
    });

    test('should display QR code in share modal', async ({ page }) => {
      await goToProfile(page);

      await page.getByRole('button', { name: 'Share' }).first().click();
      await expect(page.getByTestId('share-modal')).toBeVisible();

      const qrCode = page.getByTestId('share-qr-code');
      await expect(qrCode).toBeVisible();

      // QR code should have a valid src
      const src = await qrCode.getAttribute('src');
      expect(src).toContain('data:image/png');
    });

    test('should show URL with copy button in share modal', async ({ page }) => {
      await goToProfile(page);

      await page.getByRole('button', { name: 'Share' }).first().click();
      await expect(page.getByTestId('share-modal')).toBeVisible();

      const copyUrl = page.getByTestId('share-copy-url');
      await expect(copyUrl).toBeVisible();

      // Should contain npub in the URL
      const text = await copyUrl.textContent();
      expect(text).toContain('npub');
    });

    test('should copy URL when clicking copy in share modal', async ({ page }) => {
      await goToProfile(page);

      await page.getByRole('button', { name: 'Share' }).first().click();
      await expect(page.getByTestId('share-modal')).toBeVisible();

      const copyUrl = page.getByTestId('share-copy-url');
      await copyUrl.click();

      // Should show check icon
      await expect(copyUrl.locator('.i-lucide-check')).toBeVisible();
    });

    test('should close share modal when clicking backdrop', async ({ page }) => {
      await goToProfile(page);

      await page.getByRole('button', { name: 'Share' }).first().click();
      await expect(page.getByTestId('share-modal')).toBeVisible();

      await page.getByTestId('share-modal-backdrop').click({ position: { x: 10, y: 10 } });

      // Modal should be closed
      await expect(page.getByTestId('share-modal')).toBeHidden();
    });

    test('should close share modal when clicking QR code', async ({ page }) => {
      await goToProfile(page);

      await page.getByRole('button', { name: 'Share' }).first().click();
      await expect(page.getByTestId('share-modal')).toBeVisible();

      await page.getByTestId('share-qr-code').click();

      // Modal should be closed
      await expect(page.getByTestId('share-modal')).toBeHidden();
    });

    test('should close share modal with Escape key', async ({ page }) => {
      await goToProfile(page);

      await page.getByRole('button', { name: 'Share' }).first().click();
      await expect(page.getByTestId('share-modal')).toBeVisible();

      await page.keyboard.press('Escape');

      // Modal should be closed
      await expect(page.getByTestId('share-modal')).toBeHidden();
    });
  });
});
