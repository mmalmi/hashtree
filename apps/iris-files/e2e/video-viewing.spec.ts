/**
 * Video viewing regression test
 *
 * Ensures that a viewer can load thumbnails and play a public video
 * from a production relay/blossom-backed tree.
 */
import { test, expect } from './fixtures';
import {
  setupPageErrorHandler,
  disableOthersPool,
  waitForAppReady,
  waitForRelayConnected,
  presetProductionRelaysInDB,
  configureBlossomServers,
} from './test-utils';

const BASE_URL = 'http://localhost:5173';

const VIDEO_URL = `${BASE_URL}/video.html#/npub1l66ntjjz0aatw4g6mlzlesu3x2cse0q3eevyruz43p5qa985uudqqfaykt/videos%2FTaistelukentt%C3%A4%202020%20%EF%BD%9C%20Slagf%C3%A4lt%202020%20%EF%BD%9C%20Battlefield%202020`;

test('viewer loads thumbnails and plays a public video', async ({ page }) => {
  test.slow();

  setupPageErrorHandler(page);

  await page.goto(`${BASE_URL}/video.html#/`);
  await presetProductionRelaysInDB(page);
  await page.reload();
  await waitForAppReady(page, 60000);
  await disableOthersPool(page);
  await configureBlossomServers(page);
  await waitForRelayConnected(page, 60000);

  await page.goto(VIDEO_URL);
  await waitForAppReady(page, 60000);
  await waitForRelayConnected(page, 60000);

  await page.waitForFunction(() => {
    const images = Array.from(document.querySelectorAll('.aspect-video img')) as HTMLImageElement[];
    return images.some((img) => img.complete && img.naturalWidth > 0);
  }, undefined, { timeout: 60000 });

  await page.screenshot({ path: 'e2e/screenshots/video-view-thumbnails.png', fullPage: true });

  const videoEl = page.locator('video');
  await expect(videoEl).toBeVisible({ timeout: 60000 });
  await videoEl.click({ force: true });

  await page.evaluate(() => {
    const video = document.querySelector('video') as HTMLVideoElement | null;
    if (!video) return;
    video.muted = true;
    void video.play().catch(() => {});
  });

  await page.waitForFunction(() => {
    const video = document.querySelector('video') as HTMLVideoElement | null;
    return !!(video && video.currentTime > 0.2 && video.readyState >= 3);
  }, undefined, { timeout: 60000 });

  await page.screenshot({ path: 'e2e/screenshots/video-view-playing.png', fullPage: true });
});
