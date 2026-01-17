import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForRelayConnected } from './test-utils';
import path from 'path';
import { fileURLToPath } from 'url';
import { nip19 } from 'nostr-tools';
// Run tests in this file serially - they share video upload state
test.describe.configure({ mode: 'serial' });

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/**
 * Helper to ensure user is logged in
 */
async function ensureLoggedIn(page: any) {
  // Check if already logged in (Create button visible)
  const createBtn = page.locator('button:has-text("Create")');
  const isVisible = await createBtn.isVisible().catch(() => false);

  if (!isVisible) {
    // Need to login - try clicking New button
    const newBtn = page.getByRole('button', { name: /New/i });
    if (await newBtn.isVisible().catch(() => false)) {
      await newBtn.click();
      await expect(createBtn).toBeVisible({ timeout: 15000 });
    }
  }
}

/**
 * Helper to open the video upload modal
 * The Create button opens a dropdown with options - click "Upload Video" to open modal
 */
async function openUploadModal(page: any) {
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
}

/**
 * Dismiss any open upload modal or dropdown before starting a new action.
 */
async function dismissVideoOverlays(page: any) {
  await page.keyboard.press('Escape');
  await expect(page.getByRole('heading', { name: 'Upload Video' })).toBeHidden({ timeout: 10000 });
  await expect(page.locator('button:has-text("Upload Video")')).toBeHidden({ timeout: 5000 });
}

/**
 * Tests for video.iris.to (Iris Video app)
 * Tests video upload, playback, and navigation
 */
test.describe('Iris Video App', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('shows Iris Video header and home page', async ({ page }) => {
    await page.goto('/video.html#/');

    // Should show the Iris Video header
    await expect(page.locator('text=Iris Video')).toBeVisible({ timeout: 30000 });

    // Take screenshot of home page
    await page.screenshot({ path: 'e2e/screenshots/video-home.png' });
  });

  test('shows feed content for new user after uploading video', async ({ page }) => {
    test.slow(); // Video upload takes time

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Close any modal and open upload modal
    await dismissVideoOverlays(page);
    await openUploadModal(page);

    // Upload the test video file
    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    const fileInput = page.locator('input[type="file"][accept="video/*"]');
    await fileInput.setInputFiles(testVideoPath);

    // Title should be pre-filled from filename
    const titleInput = page.locator('input[placeholder="Video title"]');
    await expect(titleInput).toHaveValue('Big_Buck_Bunny_360_10s_1MB', { timeout: 5000 });

    // Change title to something unique
    const videoTitle = `Feed Test Video ${Date.now()}`;
    await titleInput.fill(videoTitle);

    // Click Upload button
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for upload to complete
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Navigate back to home
    await page.locator('a[href="#/"]').first().click();
    await page.waitForURL(/\/video\.html#\/$/, { timeout: 10000 });

    // Wait for video card to appear in feed (no "Feed" heading in current UI)
    await expect(page.locator('a[href*="videos"]').first()).toBeVisible({ timeout: 30000 });

    // Take screenshot of feed content
    await page.screenshot({ path: 'e2e/screenshots/video-feed-content.png' });
  });

  test('can open upload modal', async ({ page }) => {
    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Close any open modal first (press Escape)
    await dismissVideoOverlays(page);

    // Open upload modal via dropdown
    await openUploadModal(page);

    // Should have file selection prompt
    await expect(page.locator('text=Click to select a video file')).toBeVisible();

    // Take screenshot of upload modal
    await page.screenshot({ path: 'e2e/screenshots/video-upload-modal.png' });
  });

  test('can upload video and navigate to video page', async ({ page }) => {
    test.slow(); // Video processing can take time

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Close any modal and open upload modal
    await dismissVideoOverlays(page);
    await openUploadModal(page);

    // Upload the test video file
    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    const fileInput = page.locator('input[type="file"][accept="video/*"]');
    await fileInput.setInputFiles(testVideoPath);

    // Should show file info and title field
    await expect(page.locator('text=Big_Buck_Bunny_360_10s_1MB.mp4')).toBeVisible({ timeout: 10000 });

    // Title should be pre-filled from filename
    const titleInput = page.locator('input[placeholder="Video title"]');
    await expect(titleInput).toHaveValue('Big_Buck_Bunny_360_10s_1MB', { timeout: 5000 });

    // Take screenshot of upload modal with file selected
    await page.screenshot({ path: 'e2e/screenshots/video-upload-file-selected.png' });

    // Change title to something unique
    const videoTitle = `Test Video ${Date.now()}`;
    await titleInput.fill(videoTitle);

    // Click Upload button in modal
    await page.locator('.fixed button:has-text("Upload")').click();

    // Should show progress
    await expect(page.locator('text=Processing...').or(page.locator('text=Preparing')).first()).toBeVisible({ timeout: 5000 });

    // Wait for upload to complete and navigate to video page
    // URL should contain videos%2F (encoded slash since treeName includes 'videos/')
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Modal should auto-close after navigation
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Take screenshot of video player page
    await page.screenshot({ path: 'e2e/screenshots/video-player-page.png' });

    // Verify video title is shown (this confirms we're on the right page)
    await expect(page.locator(`text=${videoTitle}`)).toBeVisible({ timeout: 10000 });

    // Wait for video to load (may take time for tree root to sync)
    const videoLocator = page.locator('video');
    await expect(videoLocator).toBeVisible({ timeout: 60000 });

    // Wait for video to actually load metadata and have duration
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      return video && video.readyState >= 1 && video.duration > 0;
    }, { timeout: 30000 });

    // Verify video properties are valid
    const videoProps = await page.evaluate(() => {
      const video = document.querySelector('video');
      if (!video) return null;
      return {
        duration: video.duration,
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        readyState: video.readyState,
        src: video.src?.substring(0, 100),
        error: video.error?.message
      };
    });

    // Video should have valid properties (Big Buck Bunny is ~10 seconds)
    expect(videoProps).not.toBeNull();
    expect(videoProps!.duration).toBeGreaterThan(5);
    expect(videoProps!.videoWidth).toBeGreaterThan(0);
    expect(videoProps!.videoHeight).toBeGreaterThan(0);
    expect(videoProps!.error).toBeUndefined();

    // Take final screenshot
    await page.screenshot({ path: 'e2e/screenshots/video-player-loaded.png' });
  });

  test('can delete uploaded video', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Upload a video first
    await dismissVideoOverlays(page);
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    const fileInput = page.locator('input[type="file"][accept="video/*"]');
    await fileInput.setInputFiles(testVideoPath);

    const videoTitle = `Delete Test ${Date.now()}`;
    const titleInput = page.locator('input[placeholder="Video title"]');
    await titleInput.fill(videoTitle);

    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for navigation to video page
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Verify we're on the video page
    await expect(page.locator(`text=${videoTitle}`)).toBeVisible({ timeout: 10000 });

    // Click delete button
    page.on('dialog', dialog => dialog.accept()); // Accept the confirm dialog
    const deleteBtn = page.locator('button[title="Delete video"]');
    await expect(deleteBtn).toBeVisible({ timeout: 5000 });
    await deleteBtn.click();

    // Should navigate back to home
    await page.waitForURL('/video.html#/', { timeout: 10000 });

    // Force page reload to clear any cached feed data
    await page.reload();
    await disableOthersPool(page);

    // Wait for feed to load or empty state to appear
    const feedReady = page.locator('a[href*="videos"]').first()
      .or(page.getByText('No videos found', { exact: false }));
    await expect(feedReady).toBeVisible({ timeout: 30000 });

    // Video should no longer appear in feed - check specifically in the feed area
    // Use a longer timeout and check that the video card doesn't appear
    const videoCard = page.locator(`a[href*="videos"]`).filter({ hasText: videoTitle });
    await expect(videoCard).not.toBeVisible({ timeout: 15000 });
  });

  test('profile page shows uploaded videos', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Get the user's npub (wait for store to be initialized)
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

    // Upload a video first
    await dismissVideoOverlays(page);
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);

    const videoTitle = `Profile Test ${Date.now()}`;
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for video page
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Modal should auto-close
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    await expect(page.locator('video')).toBeVisible({ timeout: 30000 });

    // Navigate to profile page
    await page.evaluate((n) => window.location.hash = `#/${n}`, npub);

    // Wait for profile to load - look for "video" count text
    await expect(page.locator('text=video').first()).toBeVisible({ timeout: 30000 });

    // Take screenshot of profile page
    await page.screenshot({ path: 'e2e/screenshots/video-profile-page.png' });

    // Video should appear on profile
    await expect(page.locator(`text=${videoTitle}`)).toBeVisible({ timeout: 30000 });
  });

  test('can post a comment on a video', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Upload a video first
    await dismissVideoOverlays(page);
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);

    const videoTitle = `Comment Test ${Date.now()}`;
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for video page
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Modal should auto-close
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Wait for Comments heading to be visible
    await expect(page.getByRole('heading', { name: 'Comments' })).toBeVisible({ timeout: 30000 });

    // Find comment textarea and type a comment
    const commentBox = page.locator('textarea[placeholder="Add a comment..."]');
    await expect(commentBox).toBeVisible({ timeout: 10000 });

    const commentText = `Test comment ${Date.now()}`;
    await commentBox.fill(commentText);

    // Click Comment button
    await page.getByRole('button', { name: 'Comment' }).click();

    // Comment should appear in the list
    await expect(page.locator(`text=${commentText}`)).toBeVisible({ timeout: 30000 });

    // Take screenshot of video with comment
    await page.screenshot({ path: 'e2e/screenshots/video-with-comment.png' });
  });

  test('can like a video', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Upload a video first
    await dismissVideoOverlays(page);
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);

    const videoTitle = `Like Test ${Date.now()}`;
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for video page
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Modal should auto-close
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Wait for video title in the main content area (h1 heading)
    await expect(page.locator('h1', { hasText: videoTitle })).toBeVisible({ timeout: 30000 });

    // Find and click the like button (heart icon)
    const likeBtn = page.locator('button[title="Like"]');
    await expect(likeBtn).toBeVisible({ timeout: 10000 });

    // Click like button
    await likeBtn.click();

    // Wait for like to register - button should change to "Liked" and show count
    await expect(page.locator('button[title="Liked"]')).toBeVisible({ timeout: 10000 });

    // Like count should show "1"
    await expect(page.locator('button[title="Liked"]').locator('text=1')).toBeVisible({ timeout: 5000 });

    // Take screenshot of liked video
    await page.screenshot({ path: 'e2e/screenshots/video-liked.png' });
  });

  test('permalink navigates to nhash URL and shows video', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await waitForRelayConnected(page, 30000);

    // Upload a video first
    await dismissVideoOverlays(page);
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);

    const videoTitle = `Permalink Test ${Date.now()}`;
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for video page (npub route)
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });

    // Modal should auto-close
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Wait for video to load
    const videoLocator = page.locator('video');
    await expect(videoLocator).toBeVisible({ timeout: 60000 });

    // Wait for video to actually load metadata
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      return video && video.readyState >= 1 && video.duration > 0;
    }, { timeout: 30000 });

    // Find the Permalink button and click it
    const permalinkBtn = page.locator('button[title="Permalink (content-addressed)"]');
    await expect(permalinkBtn).toBeVisible({ timeout: 10000 });
    await permalinkBtn.click();

    // URL should now contain nhash (content-addressed permalink)
    await page.waitForURL(/\/video\.html#\/nhash1/, { timeout: 10000 });

    // Take screenshot of permalink page
    await page.screenshot({ path: 'e2e/screenshots/video-permalink-page.png' });

    // Video should still be visible on permalink page
    await expect(page.locator('video')).toBeVisible({ timeout: 30000 });

    // Wait for video to load on permalink page
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      return video && video.readyState >= 1 && video.duration > 0;
    }, { timeout: 30000 });

    // Verify video properties are valid
    const videoProps = await page.evaluate(() => {
      const video = document.querySelector('video');
      if (!video) return null;
      return {
        duration: video.duration,
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        readyState: video.readyState,
        src: video.src?.substring(0, 100),
        error: video.error?.message
      };
    });

    expect(videoProps).not.toBeNull();
    expect(videoProps!.duration).toBeGreaterThan(5);
    expect(videoProps!.videoWidth).toBeGreaterThan(0);
    expect(videoProps!.videoHeight).toBeGreaterThan(0);
    expect(videoProps!.error).toBeUndefined();

    // Should show the permalink info box
    await expect(page.locator('text=content-addressed permalink')).toBeVisible({ timeout: 5000 });

    // Take final screenshot
    await page.screenshot({ path: 'e2e/screenshots/video-permalink-loaded.png' });
  });

  test('liked video appears in follower feed', async ({ browser }) => {
    test.slow(); // Multi-browser test with Nostr sync

    // Create two browser contexts
    const context1 = await browser.newContext();
    const context2 = await browser.newContext();

    const page1 = await context1.newPage();
    const page2 = await context2.newPage();

    setupPageErrorHandler(page1);
    setupPageErrorHandler(page2);

    // === Setup page1 (uploader who will like their own video) ===
    await page1.goto('/video.html#/');
    await disableOthersPool(page1);

    // Login page1
    const newBtn1 = page1.getByRole('button', { name: /New/i });
    if (await newBtn1.isVisible().catch(() => false)) {
      await newBtn1.click();
      await expect(page1.locator('button:has-text("Create")')).toBeVisible({ timeout: 15000 });
    }
    await waitForRelayConnected(page1, 30000);

    // Upload a video on page1 (this will navigate to video page with npub in URL)
    await dismissVideoOverlays(page1);
    await openUploadModal(page1);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page1.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);

    const videoTitle = `Social Feed Test ${Date.now()}`;
    await page1.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page1.locator('.fixed button:has-text("Upload")').click();

    // Wait for video page (this URL will contain the npub)
    await page1.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });
    await expect(page1.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Get page1's npub from the current URL
    const page1Url = page1.url();
    const page1NpubMatch = page1Url.match(/npub1[a-z0-9]+/);
    expect(page1NpubMatch).toBeTruthy();
    const page1Npub = page1NpubMatch![0];
    const page1Pubkey = (() => {
      try {
        return nip19.decode(page1Npub).data as string;
      } catch {
        return null;
      }
    })();
    expect(page1Pubkey).toBeTruthy();
    console.log(`Page1 npub: ${page1Npub.slice(0, 20)}...`);

    // Like the video
    const likeBtn = page1.locator('button[title="Like"]');
    await expect(likeBtn).toBeVisible({ timeout: 10000 });
    await likeBtn.click();
    await expect(page1.locator('button[title="Liked"]')).toBeVisible({ timeout: 10000 });
    console.log('Video uploaded and liked');

    // === Setup page2 (follower) ===
    await page2.goto('/video.html#/');
    await disableOthersPool(page2);

    // Login page2
    const newBtn2 = page2.getByRole('button', { name: /New/i });
    if (await newBtn2.isVisible().catch(() => false)) {
      await newBtn2.click();
      await expect(page2.locator('button:has-text("Create")')).toBeVisible({ timeout: 15000 });
    }
    await waitForRelayConnected(page2, 30000);

    // Page2 follows page1
    await page2.goto(`/video.html#/${page1Npub}`);
    const followBtn = page2.getByRole('button', { name: 'Follow', exact: true });
    await expect(followBtn).toBeVisible({ timeout: 30000 });
    await followBtn.click();
    console.log('Page2 now follows page1');

    await page2.waitForFunction(async (targetPubkey: string) => {
      const { getFollowsSync } = await import('/src/stores/follows');
      const store = (window as any).__nostrStore;
      const me = store?.getState?.().pubkey;
      if (!me) return false;
      const follows = getFollowsSync(me);
      return !!follows?.follows?.includes(targetPubkey);
    }, page1Pubkey, { timeout: 30000 });

    // Go to home page and check feed
    await page2.goto('/video.html#/');

    // Wait for video cards to appear in feed
    await expect(page2.locator('a[href*="videos"]').first()).toBeVisible({ timeout: 60000 });

    // The liked video should appear in page2's feed
    await expect(page2.locator(`text=${videoTitle}`)).toBeVisible({ timeout: 60000 });
    console.log('Liked video appears in follower feed!');

    // Take screenshot
    await page2.screenshot({ path: 'e2e/screenshots/video-social-feed.png' });

    // Cleanup
    await context1.close();
    await context2.close();
  });
});
