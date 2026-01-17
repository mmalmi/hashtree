/**
 * E2E tests for video source loading
 *
 * Verifies that video elements have proper /htree/ src attributes
 * and that videos actually load and play correctly.
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool } from './test-utils';
import path from 'path';
import { fileURLToPath } from 'url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Run tests serially to avoid resource conflicts
test.describe.configure({ mode: 'serial' });

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
 * Helper to open the video upload modal
 */
async function openUploadModal(page: any) {
  const createBtn = page.locator('button:has-text("Create")');
  await expect(createBtn).toBeVisible({ timeout: 15000 });
  await createBtn.click();

  const uploadOption = page.locator('button:has-text("Upload Video")').first();
  await expect(uploadOption).toBeVisible({ timeout: 5000 });
  await uploadOption.click();

  await expect(page.getByRole('heading', { name: 'Upload Video' })).toBeVisible({ timeout: 10000 });
}

test.describe('Video Source Loading', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('uploaded video has /htree/ src and actually plays', async ({ page }) => {
    test.slow(); // Video processing takes time

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Close any modal and open upload modal
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
    await openUploadModal(page);

    // Upload test video
    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    const fileInput = page.locator('input[type="file"][accept="video/*"]');
    await fileInput.setInputFiles(testVideoPath);

    // Set unique title
    const videoTitle = `Source Test ${Date.now()}`;
    const titleInput = page.locator('input[placeholder="Video title"]');
    await expect(titleInput).toBeVisible({ timeout: 5000 });
    await titleInput.fill(videoTitle);

    // Click upload
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for navigation to video page
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Modal should auto-close
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Wait for video element to be visible
    const videoLocator = page.locator('video');
    await expect(videoLocator).toBeVisible({ timeout: 30000 });

    // Critical: Verify video has /htree/ src (not blob: or empty)
    const videoSrc = await page.evaluate(() => {
      const video = document.querySelector('video');
      return video?.src || null;
    });

    expect(videoSrc).not.toBeNull();
    expect(videoSrc).toContain('/htree/');
    expect(videoSrc).not.toContain('blob:');

    console.log('Video src:', videoSrc);

    // Wait for video to load metadata (readyState >= 1 = HAVE_METADATA)
    await page.waitForFunction(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      return v && v.readyState >= 1 && v.duration > 0;
    }, { timeout: 30000 });

    // Verify video properties after metadata load
    const videoProps = await page.evaluate(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      if (!v) return null;
      return {
        src: v.src,
        duration: v.duration,
        videoWidth: v.videoWidth,
        videoHeight: v.videoHeight,
        readyState: v.readyState,
        networkState: v.networkState,
        error: v.error?.message || null,
      };
    });

    expect(videoProps).not.toBeNull();
    expect(videoProps!.duration).toBeGreaterThan(5);
    expect(videoProps!.videoWidth).toBeGreaterThan(0);
    expect(videoProps!.videoHeight).toBeGreaterThan(0);
    expect(videoProps!.error).toBeNull();

    console.log('Video metadata loaded:', videoProps);

    // Actually play the video and verify it progresses
    await page.evaluate(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      v.muted = true; // Mute to allow autoplay
      v.play();
    });

    // Wait for video to actually play (currentTime > 0)
    await page.waitForFunction(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      return v && v.currentTime > 0.5 && !v.paused;
    }, { timeout: 15000 });

    // Verify playback state
    const playbackState = await page.evaluate(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      if (!v) return null;
      return {
        currentTime: v.currentTime,
        paused: v.paused,
        ended: v.ended,
        readyState: v.readyState,
        error: v.error?.message || null,
      };
    });

    expect(playbackState).not.toBeNull();
    expect(playbackState!.currentTime).toBeGreaterThan(0);
    expect(playbackState!.paused).toBe(false);
    expect(playbackState!.error).toBeNull();

    console.log('Video playing successfully:', playbackState);
  });

  test('video in feed has correct thumbnail and plays on click', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Upload a video first
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);

    const videoTitle = `Feed Video ${Date.now()}`;
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for video page
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Take screenshot of video page
    await page.screenshot({ path: 'test-results/video-page-after-upload.png' });

    // Navigate back to home
    await page.locator('a[href="#/"]').first().click();
    await page.waitForURL(/\/video\.html#\/$/, { timeout: 10000 });

    // Wait for page to load and take screenshot
    await page.waitForTimeout(3000);
    await page.screenshot({ path: 'test-results/home-page-feed.png' });

    // Look for our video in the page (may be in different sections)
    const videoLink = page.locator(`a:has-text("${videoTitle}")`).first();

    // Wait for video to appear somewhere on page
    await expect(videoLink).toBeVisible({ timeout: 30000 });

    // Check that thumbnail image exists (may use /htree/ path)
    const thumbnailImg = videoLink.locator('img').first();
    const hasThumbnail = await thumbnailImg.isVisible().catch(() => false);
    if (hasThumbnail) {
      const thumbnailSrc = await thumbnailImg.getAttribute('src');
      console.log('Thumbnail src:', thumbnailSrc);
      // Thumbnail should use /htree/ path
      if (thumbnailSrc && !thumbnailSrc.startsWith('data:')) {
        expect(thumbnailSrc).toContain('/htree/');
      }
    }

    // Click on video to navigate
    await videoLink.click();

    // Should navigate to video page
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 30000 });

    // Wait for video element
    await expect(page.locator('video')).toBeVisible({ timeout: 30000 });

    // Take screenshot showing video player
    await page.screenshot({ path: 'test-results/video-playing-from-feed.png' });

    // Verify video has correct src
    const videoSrc = await page.evaluate(() => {
      const v = document.querySelector('video');
      return v?.src || null;
    });

    console.log('Video src from feed:', videoSrc);
    expect(videoSrc).toContain('/htree/');

    // Verify video actually loads
    await page.waitForFunction(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      return v && v.readyState >= 1 && v.duration > 0;
    }, { timeout: 30000 });

    const videoState = await page.evaluate(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      return {
        src: v?.src,
        duration: v?.duration,
        readyState: v?.readyState,
        error: v?.error?.message || null,
      };
    });

    console.log('Video state:', videoState);
    expect(videoState.duration).toBeGreaterThan(0);
    expect(videoState.error).toBeNull();
  });

  test('window.htree API is available after init', async ({ page }) => {
    await page.goto('/video.html#/');
    await disableOthersPool(page);

    // Wait for app to initialize
    await expect(page.locator('text=Iris Video')).toBeVisible({ timeout: 30000 });

    // Wait a bit for htreeApi to initialize
    await page.waitForTimeout(2000);

    // Check window.htree exists
    const htreeApi = await page.evaluate(() => {
      const htree = (window as any).htree;
      if (!htree) return null;
      return {
        version: htree.version,
        isTauri: htree.isTauri,
        htreeBaseUrl: htree.htreeBaseUrl,
        hasNostr: !!htree.nostr,
        hasDetectLocalRelay: typeof htree.detectLocalRelay === 'function',
      };
    });

    expect(htreeApi).not.toBeNull();
    expect(htreeApi!.version).toBe('1.0.0');
    expect(htreeApi!.isTauri).toBe(false); // Running in browser for tests
    expect(htreeApi!.htreeBaseUrl).toBe(''); // Empty for SW mode
    expect(htreeApi!.hasDetectLocalRelay).toBe(true);

    console.log('window.htree API:', htreeApi);
  });

  test('service worker is active and intercepts /htree/ requests', async ({ page }) => {
    await page.goto('/video.html#/');
    await disableOthersPool(page);

    // Wait for app to initialize
    await expect(page.locator('text=Iris Video')).toBeVisible({ timeout: 30000 });

    // Check service worker status
    const swStatus = await page.evaluate(async () => {
      if (!('serviceWorker' in navigator)) {
        return { supported: false };
      }

      const registration = await navigator.serviceWorker.ready;
      return {
        supported: true,
        active: !!registration.active,
        controlling: !!navigator.serviceWorker.controller,
        scope: registration.scope,
      };
    });

    console.log('Service worker status:', swStatus);

    expect(swStatus.supported).toBe(true);
    expect(swStatus.active).toBe(true);
    expect(swStatus.controlling).toBe(true);
  });

  test('video src URL structure is correct', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Get user's npub
    const npub = await page.evaluate(async () => {
      for (let i = 0; i < 30; i++) {
        const store = (window as any).__nostrStore;
        if (store?.getState()?.npub) {
          return store.getState().npub;
        }
        await new Promise(r => setTimeout(r, 100));
      }
      return null;
    });
    expect(npub).toBeTruthy();

    // Upload a video
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);

    const videoTitle = `URL Test ${Date.now()}`;
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Wait for video element to be visible and have a src
    const videoLocator = page.locator('video');
    await expect(videoLocator).toBeVisible({ timeout: 30000 });

    // Wait for video to load metadata
    await page.waitForFunction(() => {
      const v = document.querySelector('video') as HTMLVideoElement;
      return v && v.src && v.readyState >= 1;
    }, { timeout: 30000 });

    // Verify video src follows expected structure
    const videoSrc = await page.evaluate(() => {
      const v = document.querySelector('video');
      return v?.src || null;
    });

    expect(videoSrc).not.toBeNull();

    // Should match pattern: /htree/{npub}/videos%2F{title}/video.{ext}
    // or with Tauri prefix: http://127.0.0.1:PORT/htree/...
    const srcUrl = new URL(videoSrc!, 'http://localhost');
    const pathname = srcUrl.pathname;

    expect(pathname).toMatch(/^\/htree\/npub1[a-z0-9]+\/videos%2F.+\/video\.(webm|mp4|mov)$/);

    console.log('Video URL pathname:', pathname);

    // Verify the npub in URL matches logged-in user
    expect(pathname).toContain(npub);
  });
});
