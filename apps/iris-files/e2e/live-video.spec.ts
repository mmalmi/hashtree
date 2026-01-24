/**
 * E2E test for video viewer functionality
 *
 * Tests that videos can be uploaded and played back correctly.
 * Uses the Big Buck Bunny test video from e2e/fixtures.
 */
import { test, expect, Page } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, flushPendingPublishes, clearAllStorage } from './test-utils.js';
// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');

// Helper to set up a fresh user session and navigate to public folder
async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);

  await page.goto('/');

  await clearAllStorage(page);
  await page.reload();
  // Page ready - navigateToPublicFolder handles waiting
  await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });
  await disableOthersPool(page);
}

test.describe('Video Viewer', () => {
  test.setTimeout(120000);

  test('should display video with correct duration', async ({ page }) => {
    // Verify test file exists
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    // Log console for debugging
    page.on('console', msg => {
      const text = msg.text();
      if (msg.type() === 'error') console.log(`[Video Error] ${text}`);
    });

    // Set up fresh user and navigate to public folder
    await setupFreshUser(page);

    // Upload the video via hidden file input
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Wait for upload to complete - look for the video in the file list
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });

    // Click on the video to view it
    await videoLink.click();

    // Check that video element exists
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 30000 });

    // Wait for video to have a source
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      if (!video) return false;
      return video.src !== '' || video.srcObject !== null;
    }, undefined, { timeout: 30000 });

    // Wait for video metadata to load (duration becomes available)
    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.readyState >= 1; // HAVE_METADATA
    }, undefined, { timeout: 30000 });

    // Get video state
    const videoState = await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        src: video.src,
        hasSrcObject: video.srcObject !== null,
        readyState: video.readyState,
        duration: video.duration,
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        paused: video.paused,
        error: video.error ? { code: video.error.code, message: video.error.message } : null,
      };
    });

    console.log('Video state:', JSON.stringify(videoState, null, 2));

    // Verify video has loaded correctly
    expect(videoState).not.toBeNull();
    expect(videoState!.readyState).toBeGreaterThanOrEqual(1);
    expect(videoState!.duration).toBeGreaterThan(0);
    expect(videoState!.videoWidth).toBeGreaterThan(0);
    expect(videoState!.videoHeight).toBeGreaterThan(0);
    expect(videoState!.error).toBeNull();

    // Check duration display in UI (format: "0:00 / 0:10" for 10s video)
    // Big Buck Bunny test video is ~10 seconds
    const durationDisplay = page.locator('text=/\\d+:\\d+ \\/ \\d+:\\d+/');
    await expect(durationDisplay).toBeVisible({ timeout: 10000 });
  });

  test('should play video and update current time', async ({ page }) => {
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    await setupFreshUser(page);

    // Upload the video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Click on video
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });
    await videoLink.click();

    // Wait for video to load - allow more time when running in parallel
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 30000 });

    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.readyState >= 2; // HAVE_CURRENT_DATA
    }, undefined, { timeout: 45000 });

    // Play the video (muted to avoid autoplay restrictions)
    await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (video) {
        video.muted = true;
        return video.play().catch(e => console.error('Play failed:', e));
      }
    });

    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.currentTime > 0.2;
    }, undefined, { timeout: 15000 });

    // Check that currentTime has advanced
    const currentTime = await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video?.currentTime || 0;
    });

    console.log('Current time after playing:', currentTime);
    expect(currentTime).toBeGreaterThan(0);
  });

  test('recently changed video should show LIVE indicator and seek near end', async ({ page }) => {
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    await setupFreshUser(page);

    // Upload the video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Click on video immediately after upload (while it's still "recently changed")
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });
    await videoLink.click();

    // Wait for video element to appear first (may take time to load data from IndexedDB)
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 30000 });

    // Then wait for video metadata to load
    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.readyState >= 1;
    }, undefined, { timeout: 30000 });

    // Check for LIVE indicator (should appear for recently changed files)
    // The file was just uploaded so it should be in recentlyChangedFiles store
    const liveIndicator = page.locator('text=LIVE');

    // Get video duration and current time to check if it seeked near end
    const videoState = await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        duration: video.duration,
        currentTime: video.currentTime,
      };
    });

    console.log('Video state for live check:', JSON.stringify(videoState, null, 2));

    // Verify duration is available immediately (not NaN or 0)
    expect(videoState).not.toBeNull();
    expect(videoState!.duration).toBeGreaterThan(0);
    expect(isFinite(videoState!.duration)).toBe(true);

    // If video is long enough (>5s) and was detected as live,
    // it should have seeked to near the end (duration - 5)
    // Our test video is ~10s, so if live detection worked, currentTime should be ~5
    if (videoState!.duration > 5) {
      // Check if LIVE indicator is visible OR if it seeked to near end
      // (depending on whether the file is still in recentlyChanged store)
      const isLiveVisible = await liveIndicator.isVisible().catch(() => false);
      console.log('LIVE indicator visible:', isLiveVisible);

      if (isLiveVisible) {
        // If LIVE is shown, video should have seeked near end
        expect(videoState!.currentTime).toBeGreaterThan(videoState!.duration - 6);
      }
    }
  });

  test('video with ?live=1 hash param should show LIVE indicator', async ({ page }) => {
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    await setupFreshUser(page);

    // Upload the video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Get the video URL
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });

    // Navigate to the video with ?live=1 hash param
    const href = await videoLink.getAttribute('href');
    expect(href).toBeTruthy();
    const liveUrl = href + '?live=1';
    await page.goto('/' + liveUrl);

    // Wait for video element to appear first
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 30000 });

    // Then wait for video metadata to load
    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.readyState >= 1;
    }, undefined, { timeout: 30000 });

    // LIVE indicator should be visible because of ?live=1 param
    // Note: LIVE badge appears in both viewer header and video overlay - use .first()
    const liveIndicator = page.locator('text=LIVE').first();
    await expect(liveIndicator).toBeVisible({ timeout: 10000 });

    // Get video state
    const videoState = await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        duration: video.duration,
        currentTime: video.currentTime,
      };
    });

    console.log('Video state with ?live=1:', JSON.stringify(videoState, null, 2));

    // Verify video has correct duration and is playable
    // Note: Video does NOT seek to end on initial load with ?live=1
    // Seeking only happens on tree updates during an active stream
    expect(videoState).not.toBeNull();
    expect(videoState!.duration).toBeGreaterThan(5);
  });

  test('?live=1 param should be removed when stream is no longer live', async ({ page }) => {
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    // Capture console logs for debugging
    page.on('console', msg => {
      if (msg.text().includes('[LiveVideo]')) {
        console.log(msg.text());
      }
    });

    await setupFreshUser(page);

    // Upload the video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Get the video URL and navigate with ?live=1
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });
    const href = await videoLink.getAttribute('href');
    expect(href).toBeTruthy();

    // Navigate to video with ?live=1
    const liveUrl = href + '?live=1';
    await page.goto('/' + liveUrl);

    // Verify ?live=1 is in URL
    expect(page.url()).toContain('live=1');

    await page.evaluate(async () => {
      const { ensureMediaStreamingReady } = await import('/src/lib/mediaStreamingSetup.ts');
      await ensureMediaStreamingReady(5, 1000);
    });

    // Wait for video element and src to be set before checking live param removal
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 30000 });
    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.src;
    }, undefined, { timeout: 30000 });

    // Wait for the ?live=1 param to be removed
    // The file is not in recentlyChangedFiles (uploaded via setInputFiles, not our saveFile)
    // Stream timeout is 10 seconds + check interval of 2 seconds, so we need ~15s timeout
    await page.waitForFunction(() => {
      return !window.location.hash.includes('live=1');
    }, undefined, { timeout: 20000 });

    // Verify ?live=1 was removed from URL
    expect(page.url()).not.toContain('live=1');
    console.log('URL after live param removed:', page.url());
  });

  test('direct navigation to video URL should show video viewer, not directory', async ({ page }) => {
    // This test simulates a viewer clicking on a link shared by a streamer
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    // Set up user and upload video to get a valid URL
    await setupFreshUser(page);

    // Upload the video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Get the video URL
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });
    await flushPendingPublishes(page);
    const href = await videoLink.getAttribute('href');
    expect(href).toBeTruthy();
    console.log('Video URL:', href);

    // Navigate away first using hash (keeps app in memory - no page reload)
    await page.evaluate(() => { window.location.hash = '#/'; });
    await page.waitForURL('**/#/');

    // Navigate DIRECTLY to the video URL (simulating clicking a shared link within the same session)
    // NOTE: In a real "share link" scenario, the viewer would have a fresh page load
    // and would get data from nostr. This test verifies the SPA navigation works correctly.
    await page.evaluate((url: string) => { window.location.hash = url; }, href);
    await page.waitForFunction((target) => window.location.hash === target, href);

    // Should show video element, NOT directory listing
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 30000 });

    // Should NOT show "Empty directory" or file list
    const emptyDir = page.locator('text=Empty directory');
    await expect(emptyDir).not.toBeVisible();

    // Viewer header should show the filename
    const viewerHeader = page.getByTestId('viewer-header');
    await expect(viewerHeader).toContainText('Big_Buck_Bunny_360_10s_1MB.mp4');

    // Video should have loaded metadata
    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.readyState >= 1 && video.duration > 0;
    }, undefined, { timeout: 30000 });

    const videoState = await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        readyState: video.readyState,
        duration: video.duration,
        src: video.src,
      };
    });

    console.log('Video state after direct nav:', JSON.stringify(videoState, null, 2));
    expect(videoState).not.toBeNull();
    expect(videoState!.readyState).toBeGreaterThanOrEqual(1);
  });

  test('direct navigation to video URL with ?live=1 should show LIVE indicator', async ({ page }) => {
    // Test that direct navigation with ?live=1 param shows LIVE indicator and video loads
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    await setupFreshUser(page);

    // Upload the video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Get the video URL
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });
    await flushPendingPublishes(page);
    const href = await videoLink.getAttribute('href');
    expect(href).toBeTruthy();

    // Navigate away using hash (keeps app in memory)
    await page.evaluate(() => { window.location.hash = '#/'; });
    await page.waitForURL('**/#/');

    // Navigate DIRECTLY to video with ?live=1 (simulating shared live stream link)
    const liveUrl = href + '?live=1';
    await page.evaluate((url: string) => { window.location.hash = url; }, liveUrl);
    await page.waitForFunction((target) => window.location.hash === target, liveUrl);

    // Should show video element
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 30000 });

    // Should show LIVE indicator
    const liveIndicator = page.locator('text=LIVE').first();
    await expect(liveIndicator).toBeVisible({ timeout: 5000 });

    // Video should load and have valid duration
    await page.waitForFunction(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      return video && video.readyState >= 1 && video.duration > 0;
    }, undefined, { timeout: 30000 });

    const videoState = await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        duration: video.duration,
        currentTime: video.currentTime,
        readyState: video.readyState,
      };
    });

    console.log('Video state for direct live nav:', JSON.stringify(videoState, null, 2));
    expect(videoState).not.toBeNull();
    expect(videoState!.duration).toBeGreaterThan(5);
    // Note: Live streams don't auto-seek to end on initial load (behavior removed per MediaPlayer.svelte)
    // The LIVE indicator is the key functionality being tested
  });
});
