import { test, expect } from './fixtures';
import path from 'path';
import { fileURLToPath } from 'url';
import {
  presetProductionRelaysInDB,
  waitForAppReady,
  setupPageErrorHandler,
  ensureLoggedIn,
  disableOthersPool,
  useLocalRelay,
  waitForRelayConnected,
} from './test-utils';

/**
 * Screenshot test to verify YouTube-style UI changes
 * Run with: pnpm run test:e2e -- e2e/youtube-style-verify.spec.ts --config=playwright.production.config.ts
 *
 * NOTE: In default test mode we seed local content to avoid relying on external relays.
 */
test.describe('YouTube Style Verification', () => {
  test.describe.configure({ mode: 'serial' });

  const __dirname = path.dirname(fileURLToPath(import.meta.url));
  const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
  const USE_PRODUCTION = process.env.PLAYWRIGHT_PRODUCTION === 'true';

  const useProductionRelays = async (page: any) => {
    await page.goto('/');
    await presetProductionRelaysInDB(page);
    await page.reload();
    await waitForAppReady(page);
  };

  const dismissVideoOverlays = async (page: any) => {
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
  };

  const openUploadModal = async (page: any) => {
    const createBtn = page.locator('button:has-text("Create")');
    await expect(createBtn).toBeVisible({ timeout: 15000 });
    await createBtn.click();

    const uploadOption = page.locator('button:has-text("Upload Video")').first();
    await expect(uploadOption).toBeVisible({ timeout: 5000 });
    await uploadOption.click();

    await expect(page.getByRole('heading', { name: 'Upload Video' })).toBeVisible({ timeout: 10000 });
  };

  const seedLocalVideo = async (page: any) => {
    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await waitForAppReady(page);
    await ensureLoggedIn(page);
    await useLocalRelay(page);
    await waitForRelayConnected(page);

    await dismissVideoOverlays(page);
    await openUploadModal(page);

    const fileInput = page.locator('input[type="file"][accept="video/*"]');
    await fileInput.setInputFiles(TEST_VIDEO);

    const titleInput = page.locator('input[placeholder="Video title"]');
    await expect(titleInput).toBeVisible({ timeout: 10000 });
    const videoTitle = `Style Test ${Date.now()}`;
    await titleInput.fill(videoTitle);

    await page.locator('.fixed button:has-text("Upload")').first().click();
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    const videoUrl = page.url();
    const match = videoUrl.match(/npub1[a-z0-9]+/);
    return { videoUrl, npub: match ? match[0] : '', videoTitle };
  };

  test('screenshot home page with real content', async ({ page }) => {
    test.slow();
    setupPageErrorHandler(page);

    if (USE_PRODUCTION) {
      await useProductionRelays(page);
      await page.goto('/video.html#/');
    } else {
      await seedLocalVideo(page);
      await page.goto('/video.html#/');
    }

    await expect(page.locator('a[href*="videos"]').first()).toBeVisible({ timeout: 30000 });
    await page.screenshot({ path: 'e2e/screenshots/youtube-style-home.png', fullPage: false });
  });

  test('screenshot video page', async ({ page }) => {
    test.slow();
    setupPageErrorHandler(page);

    if (USE_PRODUCTION) {
      await useProductionRelays(page);
      await page.goto('/video.html#/npub1avdpgsnd2864qfe7jkevcrxqryhywfrx8ydmja2u0s0u0qpth9jqtr4cka/videos%2FDonkey%20Kong%20Country%20Soundtrack%20Full%20OST');
    } else {
      const seeded = await seedLocalVideo(page);
      await page.goto(seeded.videoUrl);
    }

    await expect(page.locator('h1')).toBeVisible({ timeout: 30000 });
    await page.locator('video').waitFor({ state: 'visible', timeout: 30000 });
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      return video && video.readyState >= 2;
    }, { timeout: 30000 });
    await page.screenshot({ path: 'e2e/screenshots/youtube-style-video-page.png', fullPage: false });
  });

  test('screenshot profile with videos', async ({ page }) => {
    test.slow();
    setupPageErrorHandler(page);

    if (USE_PRODUCTION) {
      await useProductionRelays(page);
      await page.goto('/video.html#/npub1avdpgsnd2864qfe7jkevcrxqryhywfrx8ydmja2u0s0u0qpth9jqtr4cka');
    } else {
      const seeded = await seedLocalVideo(page);
      await page.goto(`/video.html#/${seeded.npub}`);
    }

    await expect(page.locator('text=videos').first()).toBeVisible({ timeout: 30000 });
    await page.screenshot({ path: 'e2e/screenshots/youtube-style-profile.png', fullPage: false });
  });
});
