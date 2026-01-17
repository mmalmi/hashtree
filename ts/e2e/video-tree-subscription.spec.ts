/**
 * Video Tree Subscription Test
 *
 * Tests that when viewing a video at /npub/videos/333/video.webm,
 * the MediaPlayer subscribes to tree updates for 'videos/333' (not just 'videos').
 *
 * This is critical for live streaming - the broadcaster publishes to 'videos/333'
 * and the viewer must subscribe to the same key to receive updates.
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler } from './test-utils';
// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

test.describe('Video Tree Subscription', () => {
  test('viewer subscribes to correct tree name for video files', async ({ page }) => {
    setupPageErrorHandler(page);

    // Go to app
    await page.goto('http://localhost:5173');
    // Page ready - navigateToPublicFolder handles waiting

    // Get user's npub
    const publicLink = page.getByRole('link', { name: 'public' }).first();
    await expect(publicLink).toBeVisible({ timeout: 15000 });
    await publicLink.click();
    await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });
    const url = page.url();
    const match = url.match(/npub1[a-z0-9]+/);
    const npub = match ? match[0] : '';
    expect(npub).toBeTruthy();

    const videoName = 'test_' + Date.now();

    // Navigate to the video file URL (non-encoded path - the common case from old share links)
    const videoUrl = `http://localhost:5173/#/${npub}/videos/${videoName}/video.webm`;
    console.log('Navigating to non-encoded URL:', videoUrl);
    await page.goto(videoUrl);

    // Wait for page to load
    await page.waitForTimeout(500);

    // Check what tree name Viewer.svelte ACTUALLY passes to MediaPlayer
    // The debug hook sets window.__viewerMediaPlayerTreeName
    const subscriptionInfo = await page.evaluate((videoName: string) => {
      const w = window as any;
      const actualTreeName = w.__viewerMediaPlayerTreeName;
      const urlPath = w.__viewerUrlPath || [];
      const expectedTreeName = `videos/${videoName}`;

      return {
        actualTreeName,
        urlPath,
        expectedTreeName,
        matches: actualTreeName === expectedTreeName,
      };
    }, videoName);

    console.log('Subscription info:', JSON.stringify(subscriptionInfo, null, 2));

    // The actual treeName passed to MediaPlayer MUST match the broadcaster's tree name
    // Without the fix: actualTreeName = 'videos' (WRONG)
    // With the fix: actualTreeName = 'videos/test_xxx' (CORRECT)
    expect(subscriptionInfo.actualTreeName).toBe(subscriptionInfo.expectedTreeName);
    expect(subscriptionInfo.matches).toBe(true);
  });

  test('encoded video URL gives correct tree name directly', async ({ page }) => {
    setupPageErrorHandler(page);

    await page.goto('http://localhost:5173');
    // Page ready - navigateToPublicFolder handles waiting

    // Get npub
    const publicLink = page.getByRole('link', { name: 'public' }).first();
    await expect(publicLink).toBeVisible({ timeout: 15000 });
    await publicLink.click();
    await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });
    const npub = page.url().match(/npub1[a-z0-9]+/)?.[0] || '';

    const videoName = 'encoded_test_' + Date.now();

    // Navigate to ENCODED video URL (proper format)
    const encodedTreeName = encodeURIComponent(`videos/${videoName}`);
    const videoUrl = `http://localhost:5173/#/${npub}/${encodedTreeName}/video.webm`;
    console.log('Navigating to encoded URL:', videoUrl);
    await page.goto(videoUrl);

    await page.waitForTimeout(500);

    // With encoded URL, route.treeName should already be correct
    const routeInfo = await page.evaluate(async () => {
      const { parseRoute } = await import('/src/utils/route.ts');
      const route = parseRoute();
      return {
        treeName: route.treeName,
        path: route.path,
      };
    });

    console.log('Route info for encoded URL:', JSON.stringify(routeInfo, null, 2));

    // Encoded URL should give treeName directly as 'videos/{videoName}'
    expect(routeInfo.treeName).toBe(`videos/${videoName}`);
    expect(routeInfo.path).toEqual(['video.webm']);
  });
});
