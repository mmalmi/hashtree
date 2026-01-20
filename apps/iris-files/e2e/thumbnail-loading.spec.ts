import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady, ensureLoggedIn, configureBlossomServers } from './test-utils';
import path from 'path';
import { fileURLToPath } from 'url';
// Tests use isolated page contexts with disableOthersPool - safe for parallel execution

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const TEST_VIDEO_PATH = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');

/**
 * Thumbnail loading tests for video feed
 *
 * Ensures thumbnails load properly for new users, including:
 * - Single video thumbnails at root level
 * - Playlist thumbnails (first video's thumbnail)
 * - Error recovery showing placeholder icon
 */

test.describe('Thumbnail Loading', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  async function openUploadModal(page) {
    const createBtn = page.locator('button:has-text("Create")');
    await expect(createBtn).toBeVisible({ timeout: 15000 });
    await createBtn.click();

    const uploadOption = page.locator('button:has-text("Upload Video")').first();
    await expect(uploadOption).toBeVisible({ timeout: 5000 });
    await uploadOption.click();

    await expect(page.getByRole('heading', { name: 'Upload Video' })).toBeVisible({ timeout: 10000 });
  }

  async function uploadFallbackVideo(page) {
    await page.keyboard.press('Escape');
    await openUploadModal(page);

    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(TEST_VIDEO_PATH);
    const titleInput = page.locator('input[placeholder="Video title"]');
    await expect(titleInput).toBeVisible({ timeout: 5000 });
    await titleInput.fill(`Thumbnail Test ${Date.now()}`);
    await page.locator('.fixed button:has-text("Upload")').click();

    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    await page.locator('a[href="#/"]').first().click();
    await page.waitForURL(/\/video\.html#\/$/, { timeout: 10000 });
  }

  async function ensureFeedHasCards(page) {
    const hasCards = await page.waitForFunction(
      () => document.querySelectorAll('[href*="npub"]').length > 0,
      undefined,
      { timeout: 60000 }
    ).then(() => true).catch(() => false);

    if (hasCards) return;

    await uploadFallbackVideo(page);
    await page.waitForFunction(
      () => document.querySelectorAll('[href*="npub"]').length > 0,
      undefined,
      { timeout: 90000 }
    );
  }

  test('thumbnails load or show placeholder for new user', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await waitForAppReady(page);
    await configureBlossomServers(page);
    await ensureLoggedIn(page);

    await ensureFeedHasCards(page);

    const getVisibleThumbnailStats = () => page.evaluate(() => {
      const cards = Array.from(document.querySelectorAll('[href*="npub"]'));
      let loaded = 0;
      let placeholder = 0;
      let loading = 0;
      let error = 0;
      let total = 0;

      cards.forEach(card => {
        const thumbnailArea = card.querySelector('.aspect-video');
        if (!thumbnailArea) return;

        const rect = thumbnailArea.getBoundingClientRect();
        const isVisible = rect.bottom > 0 && rect.top < window.innerHeight && rect.width > 0 && rect.height > 0;
        if (!isVisible) return;

        total++;

        const img = thumbnailArea.querySelector('img') as HTMLImageElement | null;
        const icon = thumbnailArea.querySelector('.i-lucide-video');

        if (img && !icon) {
          if (img.complete && img.naturalWidth > 0) {
            loaded++;
          } else if (!img.complete) {
            loading++;
          } else {
            error++;
          }
        } else if (icon && !img) {
          placeholder++;
        } else if (!icon && !img) {
          error++;
        } else {
          error++;
        }
      });

      return { total, loaded, placeholder, loading, error };
    });

    await expect.poll(async () => {
      const stats = await getVisibleThumbnailStats();
      const resolved = stats.loaded + stats.placeholder;
      return stats.total > 0 && stats.error === 0 && resolved > 0;
    }, { timeout: 120000 }).toBe(true);

    // Check ONLY the thumbnail area (.aspect-video) for visible cards
    // This avoids counting Avatar images as thumbnails
    const thumbnailStats = await getVisibleThumbnailStats();

    console.log('Thumbnail Stats:', thumbnailStats);
    console.log(`Success rate: ${Math.round((thumbnailStats.loaded + thumbnailStats.placeholder) / thumbnailStats.total * 100)}%`);

    // Verify SW is active
    const swStatus = await page.evaluate(async () => {
      if (!('serviceWorker' in navigator)) return false;
      const registration = await navigator.serviceWorker.getRegistration();
      return !!registration?.active;
    });
    expect(swStatus).toBe(true);

    // Visible cards should avoid error state
    expect(thumbnailStats.error).toBe(0);

    // At least some visible cards should resolve (image or placeholder)
    if (thumbnailStats.total > 0) {
      expect(thumbnailStats.loaded + thumbnailStats.placeholder).toBeGreaterThan(0);
    }
  });

  test('playlist thumbnails update when detected', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await waitForAppReady(page);
    await configureBlossomServers(page);
    await ensureLoggedIn(page);

    await ensureFeedHasCards(page);

    // Count loaded thumbnails for visible cards (checking only .aspect-video area)
    const getLoadedCount = () => page.evaluate(() => {
      const cards = Array.from(document.querySelectorAll('[href*="npub"]'));
      let loaded = 0;
      cards.forEach(card => {
        const thumbnailArea = card.querySelector('.aspect-video');
        if (!thumbnailArea) return;

        const rect = thumbnailArea.getBoundingClientRect();
        const isVisible = rect.bottom > 0 && rect.top < window.innerHeight && rect.width > 0 && rect.height > 0;
        if (!isVisible) return;

        const img = thumbnailArea.querySelector('img') as HTMLImageElement | null;
        const icon = thumbnailArea.querySelector('.i-lucide-video');
        if (img && !icon && img.complete && img.naturalWidth > 0) loaded++;
      });
      return loaded;
    });

    const initialLoaded = await getLoadedCount();

    // Wait for thumbnail loading to stabilize (playlist detection updates can change counts)
    await page.waitForFunction(() => {
      const cards = Array.from(document.querySelectorAll('[href*="npub"]'));
      let loaded = 0;

      cards.forEach(card => {
        const thumbnailArea = card.querySelector('.aspect-video');
        if (!thumbnailArea) return;

        const rect = thumbnailArea.getBoundingClientRect();
        const isVisible = rect.bottom > 0 && rect.top < window.innerHeight && rect.width > 0 && rect.height > 0;
        if (!isVisible) return;

        const img = thumbnailArea.querySelector('img') as HTMLImageElement | null;
        const icon = thumbnailArea.querySelector('.i-lucide-video');
        if (img && !icon && img.complete && img.naturalWidth > 0) loaded++;
      });

      const state = (window as any).__thumbStableState;
      const now = performance.now();
      if (!state || state.count !== loaded) {
        (window as any).__thumbStableState = { count: loaded, since: now };
        return false;
      }

      return now - state.since > 5000;
    }, undefined, { timeout: 60000 });

    const finalLoaded = await getLoadedCount();

    console.log('Initial loaded:', initialLoaded, 'Final loaded:', finalLoaded);

    // Should have same or more loaded after playlist detection
    expect(finalLoaded).toBeGreaterThanOrEqual(initialLoaded);
  });
});
