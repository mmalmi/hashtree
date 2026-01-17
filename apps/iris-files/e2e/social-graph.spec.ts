import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady, ensureLoggedIn } from './test-utils';

test.describe('Social graph features', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await ensureLoggedIn(page);
  });

  // Helper to close any open modals
  async function closeModals(page) {
    for (let i = 0; i < 3; i++) {
      const backdrop = page.locator('.fixed.inset-0').first();
      if (await backdrop.isVisible({ timeout: 300 }).catch(() => false)) {
        const cancelButton = backdrop.getByRole('button', { name: /Cancel/i });
        if (await cancelButton.isVisible().catch(() => false)) {
          await cancelButton.click();
        } else {
          await page.keyboard.press('Escape');
          if (await backdrop.isVisible({ timeout: 300 }).catch(() => false)) {
            await backdrop.click({ position: { x: 5, y: 5 }, force: true });
          }
        }
        await expect(backdrop).toBeHidden({ timeout: 5000 });
      } else {
        break;
      }
    }
  }

  // Helper to navigate to own profile
  async function goToOwnProfile(page) {
    await closeModals(page);
    // Click on the avatar button in header (title: "My Profile")
    const avatarButton = page.getByTitle('My Profile (double-click for users)');
    if (await avatarButton.isVisible({ timeout: 1000 }).catch(() => false)) {
      await avatarButton.click();
    }
    await page.waitForURL(/npub1/, { timeout: 15000 });
    await expect(page.locator('h1').first()).toBeVisible({ timeout: 15000 });
    await closeModals(page);
  }

  test.describe('ProfileView badges', () => {
    test('should show "You" badge on own profile', async ({ page }) => {
      await goToOwnProfile(page);

      // Should show "You" badge
      const youBadge = page.locator('text=You').first();
      await expect(youBadge).toBeVisible();
    });

    test('should show following count', async ({ page }) => {
      await goToOwnProfile(page);

      // Should show following count (may be 0 or ...) - it's a link, not a button
      const followingLink = page.getByRole('link', { name: /Following/i });
      await expect(followingLink).toBeVisible();
    });
  });

  test.describe('Follow/unfollow', () => {
    // Note: Testing follow/unfollow requires another user's profile
    // For now we just verify the UI structure on own profile
    test('should not show follow/unfollow button on own profile', async ({ page }) => {
      await goToOwnProfile(page);

      // Should NOT have a follow or unfollow button on own profile (exact match)
      const followButton = page.getByRole('button', { name: 'Follow', exact: true });
      const unfollowButton = page.getByRole('button', { name: 'Unfollow', exact: true });
      await expect(followButton).not.toBeVisible();
      await expect(unfollowButton).not.toBeVisible();
    });

    test('should show Edit Profile button on own profile', async ({ page }) => {
      await goToOwnProfile(page);

      // Should show Edit Profile button
      const editButton = page.getByRole('button', { name: 'Edit Profile' });
      await expect(editButton).toBeVisible();
    });
  });
});
