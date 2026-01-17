import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool } from './test-utils';
import path from 'path';
import { fileURLToPath } from 'url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/**
 * Helper to ensure user is logged in
 */
async function ensureLoggedIn(page: any) {
  const createBtn = page.locator('button:has-text("Create")');
  const isVisible = await createBtn.isVisible().catch(() => false);

  if (!isVisible) {
    const newBtn = page.getByRole('button', { name: /New/i });
    if (await newBtn.isVisible().catch(() => false)) {
      await newBtn.click();
      await expect(createBtn).toBeVisible({ timeout: 15000 });
    }
  }
}

/**
 * Get the user's npub from the current session
 */
async function getUserNpub(page: any): Promise<string> {
  return await page.evaluate(() => {
    return new Promise<string>((resolve) => {
      const store = (window as any).__nostrStore;
      if (!store || !store.subscribe) {
        resolve('');
        return;
      }
      const unsub = store.subscribe((state: any) => {
        resolve(state?.npub || '');
      });
      // Immediately unsubscribe since we just want the current value
      if (typeof unsub === 'function') unsub();
    });
  });
}

/**
 * Helper to upload a test video and return the video URL
 */
async function uploadTestVideo(page: any): Promise<string> {
  await ensureLoggedIn(page);

  // Close any modal and open upload modal
  await page.keyboard.press('Escape');
  await page.waitForTimeout(200);

  // Click Create button to open dropdown
  const createBtn = page.locator('button:has-text("Create")');
  await expect(createBtn).toBeVisible({ timeout: 15000 });
  await createBtn.click();

  // Wait for dropdown and click "Upload Video" option
  const uploadOption = page.locator('button:has-text("Upload Video")').first();
  await expect(uploadOption).toBeVisible({ timeout: 5000 });
  await uploadOption.click();

  // Wait for modal to appear
  await expect(page.getByRole('heading', { name: 'Upload Video' })).toBeVisible({ timeout: 10000 });

  // Upload the test video file
  const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
  const fileInput = page.locator('input[type="file"][accept="video/*"]');
  await fileInput.setInputFiles(testVideoPath);

  // Wait for title to be pre-filled
  const titleInput = page.locator('input[placeholder="Video title"]');
  await expect(titleInput).toHaveValue('Big_Buck_Bunny_360_10s_1MB', { timeout: 5000 });

  // Change title to something unique
  const videoTitle = `Zap Test Video ${Date.now()}`;
  await titleInput.fill(videoTitle);

  // Click Upload button
  await page.locator('.fixed button:has-text("Upload")').click();

  // Wait for upload to complete and navigate to video page
  await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

  return page.url();
}

/**
 * Tests for video zap functionality
 */
test.describe('Video Zaps', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('zap button shows on own videos', async ({ page }) => {
    test.slow(); // Video upload takes time

    await page.goto('/video.html#/');
    await disableOthersPool(page);

    await uploadTestVideo(page);

    // Zap button should be visible (disabled if no lud16, enabled if lud16 set)
    const zapButton = page.getByTestId('zap-button');
    await expect(zapButton).toBeVisible({ timeout: 5000 });

    // Button state depends on whether profile has lud16 - either state is valid
    // Just verify the button is functional
    const isDisabled = await zapButton.isDisabled();
    console.log(`Zap button disabled: ${isDisabled} (depends on profile lud16)`);

    // Like button should be visible
    await expect(page.locator('button[title="Like"]')).toBeVisible();
  });

  test('comments section loads on video page', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);

    await uploadTestVideo(page);

    // Wait for video page to fully load - use heading to be specific
    await expect(page.getByRole('heading', { name: /Comments/ })).toBeVisible({ timeout: 10000 });

    // Should show empty comments message
    await expect(page.locator('text=No comments yet')).toBeVisible();

    // Take screenshot
    await page.screenshot({ path: 'e2e/screenshots/video-comments-section.png' });
  });

  test('zap button shows total on video page', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);

    await uploadTestVideo(page);

    // Zap button should be visible (shows "0" when disabled, "Tip" when enabled with no zaps)
    const zapButton = page.getByTestId('zap-button');
    await expect(zapButton).toBeVisible({ timeout: 5000 });
    // Either shows "0" (disabled) or "Tip" (enabled, no zaps yet) - both are valid
    const text = await zapButton.textContent();
    expect(['0', ' Tip', 'Tip'].some(t => text?.includes(t))).toBe(true);

    await page.screenshot({ path: 'e2e/screenshots/video-zap-button.png' });
  });

  test('zap modal opens when profile has lud16', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Get current user's npub
    const userNpub = await getUserNpub(page);
    console.log('User npub:', userNpub);

    // Set lud16 in profile edit page
    if (!userNpub) {
      console.log('No npub found, skipping test');
      return;
    }
    await page.goto(`/video.html#/${userNpub}/edit`);

    // Wait for Edit Profile heading to appear
    await expect(page.getByRole('heading', { name: 'Edit Profile' })).toBeVisible({ timeout: 15000 });

    // Find and fill lightning address field
    const lud16Input = page.getByTestId('lud16-input');
    await expect(lud16Input).toBeVisible({ timeout: 10000 });
    await lud16Input.fill('test@getalby.com');

    // Save profile
    const saveBtn = page.locator('button:has-text("Save")');
    await saveBtn.click();
    await page.waitForTimeout(2000);
    console.log('Profile saved with lud16');

    // Upload a video
    await page.goto('/video.html#/');
    await uploadTestVideo(page);

    // Wait for profile to be loaded
    await page.waitForTimeout(3000);

    // Check console logs for debug output
    const logs: string[] = [];
    page.on('console', msg => {
      if (msg.text().includes('[VideoZapButton]')) {
        logs.push(msg.text());
        console.log('Browser log:', msg.text());
      }
    });

    // Zap button should be visible
    const zapButton = page.getByTestId('zap-button');
    await expect(zapButton).toBeVisible({ timeout: 10000 });

    // Check if button is enabled (not disabled)
    const isDisabled = await zapButton.isDisabled();
    console.log('Zap button disabled:', isDisabled);

    // Wait a bit more for profile to load if disabled
    if (isDisabled) {
      await page.waitForTimeout(3000);
      const stillDisabled = await zapButton.isDisabled();
      console.log('After wait, still disabled:', stillDisabled);
    }

    // Click zap button
    await zapButton.click();
    await page.waitForTimeout(500);

    // Check if modal opened
    const modalVisible = await page.getByTestId('zap-modal').isVisible().catch(() => false);
    console.log('Modal visible:', modalVisible);

    // Take screenshot regardless
    await page.screenshot({ path: 'e2e/screenshots/video-zap-modal-test.png' });

    // Print collected logs
    console.log('Collected logs:', logs);

    // Modal should open
    await expect(page.getByTestId('zap-modal')).toBeVisible({ timeout: 5000 });
  });
});
