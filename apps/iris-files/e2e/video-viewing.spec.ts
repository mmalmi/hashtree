/**
 * Video viewing regression test
 *
 * Ensures that a viewer can load thumbnails and play a public video
 * from a production relay/blossom-backed tree.
 */
import { test, expect, type Page } from './fixtures';
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
const PLAYLIST_URL = `${BASE_URL}/video.html#/npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk/videos%2FAngel%20Sword/60m84vRAsZw`;

async function prepareVideoSession(page: Page) {
  setupPageErrorHandler(page);

  await page.goto(`${BASE_URL}/`);
  await waitForAppReady(page, 60000);
  await disableOthersPool(page);
  await presetProductionRelaysInDB(page);
}

async function openVideoApp(page: Page, url: string) {
  await page.goto(url);
  await waitForAppReady(page, 60000);
  await configureBlossomServers(page);
  await waitForRelayConnected(page, 60000);
}

test('viewer plays a public video', async ({ page }) => {
  test.slow();

  await prepareVideoSession(page);
  await openVideoApp(page, VIDEO_URL);

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

test('viewer loads playlist sidebar thumbnails', async ({ page }) => {
  test.slow();

  await prepareVideoSession(page);
  await openVideoApp(page, PLAYLIST_URL);

  await page.waitForFunction(async () => {
    const { currentPlaylist } = await import('/src/stores/playlist');
    let playlist: { items?: unknown[] } | null = null;
    currentPlaylist.subscribe((value) => {
      playlist = value as typeof playlist;
    })();
    return !!(playlist && playlist.items && playlist.items.length > 1);
  }, undefined, { timeout: 60000 });

  const sidebar = page.locator('[data-testid="playlist-sidebar"]:visible');
  await expect(sidebar).toBeVisible({ timeout: 60000 });
  await expect.poll(() => sidebar.locator('button').count(), { timeout: 60000 }).toBeGreaterThan(2);
  await expect.poll(async () => {
    return sidebar.locator('img').evaluateAll((images) =>
      images.some((img) => img.complete && img.naturalWidth > 0)
    );
  }, { timeout: 60000 }).toBe(true);

  await page.screenshot({ path: 'e2e/screenshots/video-playlist-sidebar.png', fullPage: true });
});
