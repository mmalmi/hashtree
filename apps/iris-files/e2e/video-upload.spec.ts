import { test, expect, Page } from '@playwright/test';
import fs from 'fs';
import https from 'https';

const TEST_VIDEO_URL = 'https://sample-videos.com/video321/mp4/240/big_buck_bunny_240p_1mb.mp4';
const TEST_VIDEO_PATH = '/tmp/big_buck_bunny_test.mp4';

// Download test video if not exists
async function ensureTestVideo(): Promise<string> {
  if (fs.existsSync(TEST_VIDEO_PATH)) {
    return TEST_VIDEO_PATH;
  }

  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(TEST_VIDEO_PATH);
    https.get(TEST_VIDEO_URL, (response) => {
      if (response.statusCode === 301 || response.statusCode === 302) {
        // Follow redirect
        https.get(response.headers.location!, (res) => {
          res.pipe(file);
          file.on('finish', () => {
            file.close();
            resolve(TEST_VIDEO_PATH);
          });
        }).on('error', reject);
      } else {
        response.pipe(file);
        file.on('finish', () => {
          file.close();
          resolve(TEST_VIDEO_PATH);
        });
      }
    }).on('error', reject);
  });
}

// Login as test user and wait for login to complete
async function loginAsTestUser(page: Page) {
  await page.evaluate(() => {
    // Use a fixed test nsec for reproducibility
    const testNsec = 'nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5';
    localStorage.setItem('nostr-login', JSON.stringify({ nsec: testNsec }));
  });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => !!navigator.serviceWorker?.controller, { timeout: 30000 });
  // Wait for login to complete - Create button appears when logged in
  await page.locator('button:has-text("Create"), a:has-text("Create")').first().waitFor({ state: 'visible', timeout: 15000 });
}

async function waitForVideoData(page: Page, timeoutMs = 120000) {
  await expect.poll(async () => {
    return page.evaluate(async () => {
      try {
        const hash = window.location.hash;
        const match = hash.match(/#\/(npub1[a-z0-9]+)\/([^/?]+)/);
        if (!match) return false;
        const npub = match[1];
        const treeName = decodeURIComponent(match[2]);
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const root = getTreeRootSync(npub, treeName);
        if (!root) return false;
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        if (!adapter?.readFile && !adapter?.readFileRange) return false;
        await adapter.sendHello?.();
        if (typeof adapter.get === 'function') {
          await Promise.race([
            adapter.get(root.hash).catch(() => null),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), 5000)),
          ]);
        }
        const tree = getTree();
        const candidates = ['video.mp4', 'video.webm', 'video.mov', 'video.mkv'];
        for (const name of candidates) {
          const entry = await Promise.race([
            tree.resolvePath(root, name),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), 5000)),
          ]);
          if (!entry?.cid) continue;
          const read = () => {
            if (typeof adapter.readFileRange === 'function') {
              return adapter.readFileRange(entry.cid, 0, 1024);
            }
            return adapter.readFile(entry.cid);
          };
          const data = await Promise.race([
            read(),
            new Promise<Uint8Array | null>((resolve) => setTimeout(() => resolve(null), 5000)),
          ]);
          if (data && data.length > 0) return true;
        }
        return false;
      } catch {
        return false;
      }
    });
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000, 5000] }).toBe(true);
}

test.describe('Video Upload with Visibility', () => {
  test.beforeAll(async () => {
    await ensureTestVideo();
  });

  test('should upload link-visible video and show correct icon after refresh', async ({ page, browser }) => {
    test.slow(); // This test involves video upload which can be slow

    // Login
    await page.goto('/video.html');
    await loginAsTestUser(page);

    // Navigate to create page
    await page.goto('/video.html#/create');

    // Upload video file (input is hidden, use setInputFiles directly)
    const fileInput = page.locator('input[type="file"]');
    await fileInput.setInputFiles(TEST_VIDEO_PATH);

    // Wait for title input to appear (shows after file is selected and processed)
    const titleInput = page.getByPlaceholder(/title/i);
    await titleInput.waitFor({ state: 'visible', timeout: 10000 });

    // Set title
    const testTitle = `Test Link-Visible ${Date.now()}`;
    await titleInput.fill(testTitle);

    // Set visibility to link-visible (click the Link button)
    const linkVisibleButton = page.getByRole('button', { name: /link/i }).first();
    await linkVisibleButton.click();

    // Click upload button (use the one that says "Upload Video")
    const uploadButton = page.getByRole('button', { name: 'Upload Video' });
    await uploadButton.click();

    // Wait for upload to complete - URL changes to video page
    await expect(page).toHaveURL(/videos(%2F|\/)/i, { timeout: 90000 });

    // Get the video URL
    const videoUrl = page.url();
    console.log('Video URL:', videoUrl);

    // Verify k param is in URL (link-visible uses encryption key in URL)
    expect(videoUrl).toContain('k=');

    // Wait for page to stabilize after navigation
    await page.waitForLoadState('domcontentloaded');
    await page.waitForFunction(() => !!navigator.serviceWorker?.controller, { timeout: 30000 });

    // Wait for video page to load - h1 title appears
    const titleElement = page.locator('h1');
    await expect(titleElement).toBeVisible({ timeout: 15000 });
    await expect(titleElement).toContainText('Test Link-Visible');

    // CRITICAL: Verify video actually loads with content
    const videoElement = page.locator('video');
    await expect(videoElement).toBeVisible({ timeout: 15000 });

    // Verify video has loaded content (not empty element)
    const mediaSetup = await page.evaluate(async () => {
      const { ensureMediaStreamingReady, isMediaStreamingSetup } = await import('/src/lib/mediaStreamingSetup.ts');
      const result = await ensureMediaStreamingReady(5, 1000);
      return { result, isSetup: isMediaStreamingSetup() };
    });
    console.log('Media streaming setup:', mediaSetup);
    let attemptedReload = false;
    await expect.poll(async () => {
      const videoState = await page.evaluate(() => {
        const v = document.querySelector('video') as HTMLVideoElement | null;
        if (!v) {
          return { hasSrc: false, reason: 'no-video', readyState: 0 };
        }
        if (v.readyState === 0 && v.src) {
          v.load();
        }
        const src = v.currentSrc || v.src;
        return { hasSrc: !!src, reason: src ? 'has-src' : 'no-src', readyState: v.readyState };
      });
      if (!videoState.hasSrc && videoState.reason === 'no-video') {
        const failedVisible = await page.getByText('Video failed to load').isVisible().catch(() => false);
        if (failedVisible && !attemptedReload) {
          attemptedReload = true;
          await page.reload({ waitUntil: 'domcontentloaded' });
          await page.waitForFunction(() => !!navigator.serviceWorker?.controller, { timeout: 30000 });
          await expect(page.locator('h1')).toBeVisible({ timeout: 30000 });
        }
      }
      console.log('Video state:', videoState);
      return videoState.hasSrc;
    }, { timeout: 60000, intervals: [1000, 2000, 3000, 5000] }).toBe(true);

    await waitForVideoData(page, 120000);

    // Screenshot to verify video loaded
    await page.screenshot({ path: 'test-results/link-visible-upload.png' });

    // Check for visibility icon (link icon for link-visible)
    const visibilityIcon = page.getByTitle('Link-visible (link only)');
    await expect(visibilityIcon).toBeVisible({ timeout: 5000 });

    // CRITICAL: Open in fresh browser context to verify nostr persistence (no local cache)
    // Fresh context verifies tree root is published to nostr and can be resolved
    // Note: Video DATA may not load in fresh context without blossom servers
    const freshContext = await browser.newContext();
    const freshPage = await freshContext.newPage();
    await freshPage.goto(videoUrl);

    // Verify the page loads - title appears (not "Video not found" error)
    // This proves the tree root was resolved from nostr
    const freshTitleElement = freshPage.locator('h1');
    await expect(freshTitleElement).toBeVisible({ timeout: 30000 });
    await expect(freshTitleElement).toContainText('Test Link-Visible');

    // Verify visibility icon in fresh browser (proves metadata loaded from nostr)
    const freshVisibilityIcon = freshPage.getByTitle('Link-visible (link only)');
    await expect(freshVisibilityIcon).toBeVisible({ timeout: 5000 });

    // Screenshot from fresh browser
    await freshPage.screenshot({ path: 'test-results/link-visible-fresh-browser.png' });

    await freshContext.close();
  });

  test('should upload private video and show correct icon after refresh', async ({ page }) => {
    // Login
    await page.goto('/video.html');
    await loginAsTestUser(page);

    // Navigate to create page
    await page.goto('/video.html#/create');

    // Upload video file
    const fileInput = page.locator('input[type="file"]');
    await fileInput.setInputFiles(TEST_VIDEO_PATH);

    // Wait for title input
    const titleInput = page.getByPlaceholder(/title/i);
    await titleInput.waitFor({ state: 'visible', timeout: 10000 });

    // Set title
    const testTitle = `Test Private ${Date.now()}`;
    await titleInput.fill(testTitle);

    // Set visibility to private (click the Private button)
    const privateButton = page.getByRole('button', { name: /private/i });
    await privateButton.click();

    // Click upload button
    const uploadButton = page.getByRole('button', { name: 'Upload Video' });
    await uploadButton.click();

    // Wait for upload to complete
    await expect(page).toHaveURL(/videos(%2F|\/)/i, { timeout: 90000 });

    // Get the video URL
    const videoUrl = page.url();
    console.log('Video URL:', videoUrl);

    // Wait for video page to load
    const titleElement = page.locator('h1');
    await expect(titleElement).toBeVisible({ timeout: 15000 });
    await expect(titleElement).toContainText('Test Private');

    // CRITICAL: Verify video actually loads
    const videoElement = page.locator('video');
    await expect(videoElement).toBeVisible({ timeout: 30000 });

    // Check for visibility icon (lock icon for private)
    const visibilityIcon = page.locator('[title*="Private"]');
    await expect(visibilityIcon).toBeVisible({ timeout: 5000 });

    // Refresh page to test persistence from nostr
    await page.reload();

    // CRITICAL: Verify video loads after refresh
    await expect(videoElement).toBeVisible({ timeout: 30000 });
    await expect(visibilityIcon).toBeVisible({ timeout: 5000 });
  });

  test('should auto-add k= param when owner navigates to link-visible without it', async ({ page }) => {
    test.slow(); // This test involves video upload which can be slow

    // Login
    await page.goto('/video.html');
    await loginAsTestUser(page);

    // Navigate to create page
    await page.goto('/video.html#/create');

    // Upload video file
    const fileInput = page.locator('input[type="file"]');
    await fileInput.setInputFiles(TEST_VIDEO_PATH);

    // Wait for title input
    const titleInput = page.getByPlaceholder(/title/i);
    await titleInput.waitFor({ state: 'visible', timeout: 10000 });

    // Set title
    const testTitle = `Test Owner Recovery ${Date.now()}`;
    await titleInput.fill(testTitle);

    // Set visibility to link-visible
    const linkVisibleButton = page.getByRole('button', { name: /link/i }).first();
    await linkVisibleButton.click();

    // Click upload button
    const uploadButton = page.getByRole('button', { name: 'Upload Video' });
    await uploadButton.click();

    // Wait for upload to complete - URL changes to video page with k= param
    await expect(page).toHaveURL(/videos(%2F|\/)/i, { timeout: 90000 });

    // Get the full video URL with k= param
    const fullUrl = page.url();
    console.log('Full URL:', fullUrl);
    expect(fullUrl).toContain('k=');

    // Extract the base URL without k= param
    const urlWithoutK = fullUrl.replace(/[?&]k=[a-f0-9]+/i, '');
    console.log('URL without k:', urlWithoutK);

    // Listen to console for debug messages
    const consoleLogs: string[] = [];
    page.on('console', msg => {
      consoleLogs.push(`[${msg.type()}] ${msg.text()}`);
    });

    // Navigate to the URL WITHOUT the k= param
    await page.goto(urlWithoutK);

    // Print debug logs
    const treeRootLogs = consoleLogs.filter(l => l.includes('treeRoot'));
    console.log('treeRoot logs:', treeRootLogs.length ? treeRootLogs.join('\n') : 'NONE');

    // Check for resolver logs
    const resolverLogs = consoleLogs.filter(l => l.includes('Resolver'));
    console.log('Resolver logs:', resolverLogs.length ? resolverLogs.join('\n') : 'NONE');

    // Wait for page to load and k= to be auto-added
    // The URL should update to include k= via history.replaceState
    await expect(async () => {
      const currentUrl = page.url();
      console.log('Current URL after navigation:', currentUrl);
      expect(currentUrl).toContain('k=');
    }).toPass({ timeout: 15000 });

    // Wait for page to stabilize
    await page.waitForLoadState('domcontentloaded');

    // Verify the video title loads correctly
    const titleElement = page.locator('h1');
    await expect(titleElement).toBeVisible({ timeout: 15000 });
    await expect(titleElement).toContainText('Test Owner Recovery');

    // Verify the video element loads
    const videoElement = page.locator('video');
    await expect(videoElement).toBeVisible({ timeout: 15000 });
  });

  test('should upload public video and NOT show visibility icon', async ({ page }) => {
    // Login
    await page.goto('/video.html');
    await loginAsTestUser(page);

    // Navigate to create page
    await page.goto('/video.html#/create');

    // Upload video file
    const fileInput = page.locator('input[type="file"]');
    await fileInput.setInputFiles(TEST_VIDEO_PATH);

    // Wait for title input
    const titleInput = page.getByPlaceholder(/title/i);
    await titleInput.waitFor({ state: 'visible', timeout: 10000 });

    // Set title
    const testTitle = `Test Public ${Date.now()}`;
    await titleInput.fill(testTitle);

    // Keep visibility as public (default - click Public button to ensure)
    const publicButton = page.getByRole('button', { name: /public/i });
    await publicButton.click();

    // Click upload button
    const uploadButton = page.getByRole('button', { name: 'Upload Video' });
    await uploadButton.click();

    // Wait for upload to complete
    await expect(page).toHaveURL(/videos(%2F|\/)/i, { timeout: 90000 });

    // Get the video URL
    const videoUrl = page.url();
    console.log('Video URL:', videoUrl);

    // Verify NO k param in URL (public doesn't need encryption key)
    expect(videoUrl).not.toContain('k=');

    // Wait for video page to load
    const titleElement = page.locator('h1');
    await expect(titleElement).toBeVisible({ timeout: 15000 });
    await expect(titleElement).toContainText('Test Public');

    // CRITICAL: Verify video actually loads
    const videoElement = page.locator('video');
    await expect(videoElement).toBeVisible({ timeout: 30000 });

    // Verify NO visibility icon for public videos
    const linkVisibleIcon = page.locator('[title*="Link-visible"]');
    const privateIcon = page.locator('[title*="Private"]');
    await expect(linkVisibleIcon).not.toBeVisible();
    await expect(privateIcon).not.toBeVisible();

    // Refresh page to test persistence from nostr
    await page.reload();

    // CRITICAL: Verify video loads after refresh
    await expect(videoElement).toBeVisible({ timeout: 30000 });
  });
});
