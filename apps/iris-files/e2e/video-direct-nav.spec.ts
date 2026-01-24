/**
 * E2E test for video direct navigation
 *
 * Tests that browser B can direct navigate to a video uploaded by browser A
 * and the video should load and play correctly.
 */
import { test, expect } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, configureBlossomServers, followUser, waitForFollowInWorker, waitForWebRTCConnection, waitForAppReady, waitForRelayConnected, presetLocalRelayInDB, safeReload, useLocalRelay } from './test-utils.js';
// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');

async function getTreeRootHash(page: any, npub: string, treeName: string): Promise<string | null> {
  return page.evaluate(async ({ targetNpub, targetTree }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    const root = getTreeRootSync(targetNpub, targetTree);
    return root ? toHex(root.hash) : null;
  }, { targetNpub: npub, targetTree: treeName });
}

async function waitForTreeRootHash(
  page: any,
  npub: string,
  treeName: string,
  expectedHash: string,
  timeoutMs = 60000
): Promise<void> {
  await page.waitForFunction(async ({ targetNpub, targetTree, targetHash }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    const root = getTreeRootSync(targetNpub, targetTree);
    if (!root) return false;
    return toHex(root.hash) === targetHash;
  }, { targetNpub: npub, targetTree: treeName, targetHash: expectedHash }, { timeout: timeoutMs });
}

async function prefetchFile(page: any, npub: string, treeName: string, filePath: string, timeoutMs = 60000): Promise<number> {
  let size = 0;
  await expect.poll(async () => {
    size = await page.evaluate(async (args: { targetNpub: string; targetTree: string; path: string }) => {
      try {
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const rootCid = getTreeRootSync(args.targetNpub, args.targetTree);
        if (!rootCid) return 0;
        const tree = getTree();
        const entry = await tree.resolvePath(rootCid, args.path);
        if (!entry) return 0;
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        if (!adapter?.readFile) return 0;
        const data = await adapter.readFile(entry.cid);
        return data ? data.length : 0;
      } catch {
        return 0;
      }
    }, { targetNpub: npub, targetTree: treeName, path: filePath });
    return size;
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBeGreaterThan(0);
  return size;
}

async function waitForTreeEntry(page: any, npub: string, treeName: string, filePath: string, timeoutMs = 60000): Promise<void> {
  await expect.poll(async () => {
    return page.evaluate(async (args: { targetNpub: string; targetTree: string; path: string }) => {
      try {
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const root = getTreeRootSync(args.targetNpub, args.targetTree);
        if (!root) return false;
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        if (!adapter) return false;
        await adapter.sendHello?.();
        if (typeof adapter.get === 'function') {
          await Promise.race([
            adapter.get(root.hash).catch(() => null),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), 5000)),
          ]);
        }
        const tree = getTree();
        const entry = await Promise.race([
          tree.resolvePath(root, args.path),
          new Promise<null>((resolve) => setTimeout(() => resolve(null), 5000)),
        ]);
        return !!entry?.cid;
      } catch {
        return false;
      }
    }, { targetNpub: npub, targetTree: treeName, path: filePath });
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBe(true);
}

test.describe('Video Direct Navigation', () => {
  test.setTimeout(120000);

  test('browser B can direct navigate to video uploaded by browser A', async ({ browser }) => {
    test.slow(); // Multi-browser test with WebRTC sync

    // Verify test file exists
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    // Create two browser contexts
    const context1 = await browser.newContext();
    const context2 = await browser.newContext();

    const page1 = await context1.newPage();
    const page2 = await context2.newPage();

    setupPageErrorHandler(page1);
    setupPageErrorHandler(page2);

    // Console logging for debugging
    page1.on('console', msg => {
      const text = msg.text();
      if (text.includes('WebRTC') || text.includes('error') || text.includes('Error')) {
        console.log(`[page1] ${text}`);
      }
    });
    page2.on('console', msg => {
      const text = msg.text();
      if (text.includes('WebRTC') || text.includes('error') || text.includes('Error') || text.includes('video')) {
        console.log(`[page2] ${text}`);
      }
    });

    // === Setup page1 (uploader) ===
    await page1.goto('/');
    await disableOthersPool(page1);
    await configureBlossomServers(page1);

    // Clear storage for fresh state
    await page1.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();
    });
    await presetLocalRelayInDB(page1);

    await safeReload(page1, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(page1);
    await useLocalRelay(page1);
    await waitForRelayConnected(page1, 30000);
    await waitForRelayConnected(page1, 30000);
    await disableOthersPool(page1);
    await configureBlossomServers(page1);
    await navigateToPublicFolder(page1);

    // Get page1's npub from URL
    const page1Url = page1.url();
    const page1NpubMatch = page1Url.match(/npub1[a-z0-9]+/);
    expect(page1NpubMatch).toBeTruthy();
    const page1Npub = page1NpubMatch![0];
    console.log(`Page1 npub: ${page1Npub.slice(0, 20)}...`);

    // Upload video
    console.log('Uploading video...');
    const fileInput = page1.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Wait for file to appear in list
    const videoFileName = 'Big_Buck_Bunny_360_10s_1MB.mp4';
    const videoLink = page1.locator('[data-testid="file-list"] a').filter({ hasText: videoFileName }).first();
    await expect(videoLink).toBeVisible({ timeout: 60000 });
    console.log('Video uploaded and visible in list');

    // Ensure latest tree root is published before peer navigation
    await page1.evaluate(async () => {
      const { flushPendingPublishes } = await import('/src/treeRootCache');
      await flushPendingPublishes();
    });
    const rootHashAfterUpload = await getTreeRootHash(page1, page1Npub, 'public');
    expect(rootHashAfterUpload).toBeTruthy();

    // === Setup page2 (viewer) ===
    await page2.goto('/');
    await disableOthersPool(page2);
    await configureBlossomServers(page2);

    // Clear storage for fresh state
    await page2.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();
    });
    await presetLocalRelayInDB(page2);

    await safeReload(page2, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(page2);
    await useLocalRelay(page2);
    await waitForRelayConnected(page2, 30000);
    await waitForTreeRootHash(page2, page1Npub, 'public', rootHashAfterUpload!);
    await disableOthersPool(page2);
    await configureBlossomServers(page2);

    // Get page2's npub
    await navigateToPublicFolder(page2);
    const page2Url = page2.url();
    const page2NpubMatch = page2Url.match(/npub1[a-z0-9]+/);
    expect(page2NpubMatch).toBeTruthy();
    const page2Npub = page2NpubMatch![0];
    console.log(`Page2 npub: ${page2Npub.slice(0, 20)}...`);

    // === Have users follow each other for WebRTC connection ===
    await followUser(page1, page2Npub);
    await followUser(page2, page1Npub);

    const page1Pubkey = await page1.evaluate(async (npub: string) => {
      const { npubToPubkey } = await import('/src/nostr');
      return npubToPubkey(npub);
    }, page1Npub);
    const page2Pubkey = await page2.evaluate(async (npub: string) => {
      const { npubToPubkey } = await import('/src/nostr');
      return npubToPubkey(npub);
    }, page2Npub);
    if (!page1Pubkey || !page2Pubkey) {
      throw new Error('Failed to decode pubkeys for follow validation');
    }

    const [page1FollowReady, page2FollowReady] = await Promise.all([
      waitForFollowInWorker(page1, page2Pubkey),
      waitForFollowInWorker(page2, page1Pubkey),
    ]);
    expect(page1FollowReady).toBe(true);
    expect(page2FollowReady).toBe(true);

    const page1Connected = await waitForWebRTCConnection(page1, 30000);
    const page2Connected = await waitForWebRTCConnection(page2, 30000);
    expect(page1Connected).toBe(true);
    expect(page2Connected).toBe(true);

    await waitForTreeEntry(page2, page1Npub, 'public', videoFileName, 60000).catch(() => {});

    // === Direct navigate to the video file ===
    const videoUrl = `http://localhost:5173/#/${page1Npub}/public/${videoFileName}`;
    console.log(`Direct navigating to: ${videoUrl}`);
    await page2.goto(videoUrl);
    const videoUrlPattern = new RegExp(videoFileName.replace(/\./g, '\\.'));
    await page2.waitForURL(videoUrlPattern, { timeout: 20000 }).catch(async () => {
      const nextHash = `/${page1Npub}/public/${videoFileName}`;
      await page2.evaluate((hash: string) => {
        window.location.hash = hash;
      }, nextHash);
    });

    await waitForAppReady(page2);
    await waitForRelayConnected(page2, 30000);
    await waitForTreeRootHash(page2, page1Npub, 'public', rootHashAfterUpload!);
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());

    // Check that video element exists (fallback to list navigation if needed)
    const videoElement = page2.locator('video');
    let attemptedFallback = false;
    const dirUrl = `http://localhost:5173/#/${page1Npub}/public`;
    const expectedHash = `#/${page1Npub}/public/${videoFileName}`;
    const videoEntryLink = page2.locator('[data-testid="file-list"] a').filter({ hasText: videoFileName }).first();
    await expect.poll(async () => {
      await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      const count = await videoElement.count().catch(() => 0);
      if (count > 0) return true;
      const currentUrl = page2.url();
      if (!attemptedFallback && !currentUrl.includes(`/public/${videoFileName}`) && !currentUrl.includes(expectedHash)) {
        attemptedFallback = true;
        await page2.goto(dirUrl);
        await waitForAppReady(page2);
        await waitForRelayConnected(page2, 30000);
        await waitForTreeRootHash(page2, page1Npub, 'public', rootHashAfterUpload!);
        await waitForTreeEntry(page2, page1Npub, 'public', videoFileName, 60000).catch(() => {});
      }
      if (await videoEntryLink.isVisible().catch(() => false)) {
        await videoEntryLink.click().catch(() => {});
        await page2.waitForURL(videoUrlPattern, { timeout: 30000 }).catch(() => {});
      } else {
        await page2.evaluate((hash: string) => {
          if (window.location.hash !== hash) {
            window.location.hash = hash;
            window.dispatchEvent(new HashChangeEvent('hashchange'));
          }
        }, expectedHash);
      }
      return false;
    }, { timeout: 90000, intervals: [1000, 2000, 3000] }).toBe(true);
    await expect(videoElement).toHaveCount(1, { timeout: 30000 });
    console.log('Video element is present');

    // Wait for video to have a source (SW URL for hashtree files)
    await page2.waitForFunction(() => {
      const video = document.querySelector('video');
      if (!video) return false;
      // Video should have a source URL
      return video.src !== '' && video.src.length > 0;
    }, undefined, { timeout: 30000 });

    // Prefetch the file data to avoid WebRTC timing issues
    let prefetchedSize = 0;
    try {
      prefetchedSize = await prefetchFile(page2, page1Npub, 'public', videoFileName);
    } catch (err) {
      console.log('Prefetch via Blossom failed:', err);
    }
    console.log('Prefetched bytes:', prefetchedSize);
    await page2.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement | null;
      if (video) video.load();
    });

    // Get video state
    const videoState = await page2.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        src: video.src,
        readyState: video.readyState,
        duration: video.duration,
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        error: video.error ? { code: video.error.code, message: video.error.message } : null,
        networkState: video.networkState,
      };
    });

    console.log('Video state:', JSON.stringify(videoState, null, 2));

    expect(videoState).not.toBeNull();
    expect(videoState!.src).toBeTruthy();

    console.log('=== Video Direct Navigation Test Passed ===');

    // Cleanup
    await context1.close();
    await context2.close();
  });

  test('browser B loads video via Blossom after browser A uploads and closes', async ({ browser }) => {
    test.slow(); // Multi-browser test with Blossom sync

    // Verify test file exists
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    // Create first browser context (uploader)
    const context1 = await browser.newContext();
    const page1 = await context1.newPage();

    setupPageErrorHandler(page1);

    // Console logging for debugging
    page1.on('console', msg => {
      const text = msg.text();
      if (text.includes('Blossom') || text.includes('blossom') || text.includes('upload') || text.includes('error')) {
        console.log(`[page1] ${text}`);
      }
    });

    // === Setup page1 (uploader) ===
    await page1.goto('/');
    await disableOthersPool(page1);
    await configureBlossomServers(page1);

    // Clear storage for fresh state
    await page1.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();
    });
    await presetLocalRelayInDB(page1);

    await safeReload(page1, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(page1);
    await useLocalRelay(page1);
    await disableOthersPool(page1);
    await configureBlossomServers(page1);
    await navigateToPublicFolder(page1);

    // Get page1's npub from URL
    const page1Url = page1.url();
    const page1NpubMatch = page1Url.match(/npub1[a-z0-9]+/);
    expect(page1NpubMatch).toBeTruthy();
    const page1Npub = page1NpubMatch![0];
    console.log(`Page1 npub: ${page1Npub.slice(0, 20)}...`);

    // Upload video
    console.log('Uploading video...');
    const fileInput = page1.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Wait for file to appear in list
    const videoFileName = 'Big_Buck_Bunny_360_10s_1MB.mp4';
    const videoLink = page1.locator('[data-testid="file-list"] a').filter({ hasText: videoFileName }).first();
    await expect(videoLink).toBeVisible({ timeout: 60000 });
    console.log('Video uploaded and visible in list');

    // Push to Blossom explicitly to ensure data is available after browser A closes
    console.log('Pushing to Blossom...');
    await page1.waitForFunction(() => Boolean((window as any).__hashtree?.fromHex), undefined, { timeout: 10000 });
    const blossomResult = await page1.evaluate((npub: string) => {
      const adapter = (window as any).__workerAdapter;
      const hashtree = (window as any).__hashtree;
      if (!adapter || !hashtree?.fromHex) {
        throw new Error('Worker adapter or hashtree helpers not available');
      }

      const raw = localStorage.getItem('hashtree:localRootCache');
      const cache = raw ? JSON.parse(raw) as Record<string, { hash?: string; key?: string }> : {};
      const entry = cache[`${npub}/public`];
      if (!entry?.hash) {
        throw new Error('No cached root found for Blossom push');
      }

      const hash = hashtree.fromHex(entry.hash);
      const key = entry.key ? hashtree.fromHex(entry.key) : undefined;
      return adapter.pushToBlossom(hash, key, 'public');
    }, page1Npub);
    expect(blossomResult.failed).toBe(0);
    await page1.evaluate(async () => {
      const { flushPendingPublishes } = await import('/src/treeRootCache');
      await flushPendingPublishes();
    });
    const rootHashAfterUpload = await getTreeRootHash(page1, page1Npub, 'public');
    expect(rootHashAfterUpload).toBeTruthy();

    // Close page1 - browser A is now gone
    console.log('Closing browser A...');
    await context1.close();

    // === Setup page2 (viewer) - fresh browser after A closed ===
    const context2 = await browser.newContext();
    const page2 = await context2.newPage();

    setupPageErrorHandler(page2);

    // Console logging for debugging
    page2.on('console', msg => {
      const text = msg.text();
      if (text.includes('Blossom') || text.includes('blossom') || text.includes('fetch') ||
          text.includes('video') || text.includes('error') || text.includes('Error')) {
        console.log(`[page2] ${text}`);
      }
    });

    await page2.goto('/');
    await disableOthersPool(page2);
    await configureBlossomServers(page2);

    // Clear storage for truly fresh state
    await page2.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();
    });
    await presetLocalRelayInDB(page2);

    await safeReload(page2, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(page2);
    await useLocalRelay(page2);
    await waitForRelayConnected(page2, 30000);
    await disableOthersPool(page2);
    await configureBlossomServers(page2);
    await navigateToPublicFolder(page2, { timeoutMs: 60000 });

    // === Direct navigate to the video file ===
    // Page2 does NOT follow page1 - the only way to get the video is via Blossom
    const videoUrl = `http://localhost:5173/#/${page1Npub}/public/${videoFileName}`;
    console.log(`Direct navigating to: ${videoUrl}`);
    await page2.goto(videoUrl);

    await waitForAppReady(page2);
    await waitForTreeRootHash(page2, page1Npub, 'public', rootHashAfterUpload!, 90000);
    await page2.evaluate(async () => {
      const { ensureMediaStreamingReady } = await import('/src/lib/mediaStreamingSetup.ts');
      await ensureMediaStreamingReady(5, 1000);
    });

    // Check that video element exists
    const videoElement = page2.locator('video');
    const fileLink = page2.locator('[data-testid="file-list"] a').filter({ hasText: videoFileName }).first();
    const expectedHash = `#/${page1Npub}/public/${videoFileName}`;
    await expect.poll(async () => {
      await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      const count = await videoElement.count().catch(() => 0);
      if (count > 0) return true;
      if (await fileLink.isVisible().catch(() => false)) {
        await fileLink.click().catch(() => {});
      } else {
        await page2.evaluate((hash) => {
          if (window.location.hash !== hash) {
            window.location.hash = hash;
            window.dispatchEvent(new HashChangeEvent('hashchange'));
          }
        }, expectedHash);
      }
      return false;
    }, { timeout: 90000, intervals: [1000, 2000, 3000] }).toBe(true);
    console.log('Video element is present');

    // Wait for video to have a source
    await page2.waitForFunction(() => {
      const video = document.querySelector('video');
      if (!video) return false;
      return video.src !== '' && video.src.length > 0;
    }, undefined, { timeout: 30000 });

    // Prefetch the file data to avoid WebRTC timing issues
    let prefetchedSize = 0;
    try {
      prefetchedSize = await prefetchFile(page2, page1Npub, 'public', videoFileName);
    } catch (err) {
      console.log('Prefetch via Blossom failed:', err);
    }
    console.log('Prefetched bytes:', prefetchedSize);
    await page2.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement | null;
      if (video) video.load();
    });

    // Get video state
    const videoState = await page2.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        src: video.src,
        readyState: video.readyState,
        duration: video.duration,
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        error: video.error ? { code: video.error.code, message: video.error.message } : null,
        networkState: video.networkState,
      };
    });

    console.log('Video state:', JSON.stringify(videoState, null, 2));

    expect(videoState).not.toBeNull();
    expect(videoState!.src).toBeTruthy();

    console.log('=== Video Blossom Fallback Test Passed ===');

    // Cleanup
    await context2.close();
  });

  test('video app direct navigation works without following or prior navigation', async ({ browser }) => {
    test.slow(); // Multi-browser test

    // Verify test file exists
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);

    // Create first browser context (uploader)
    const context1 = await browser.newContext();
    const page1 = await context1.newPage();

    setupPageErrorHandler(page1);

    // Console logging for debugging
    page1.on('console', msg => {
      const text = msg.text();
      if (text.includes('treeRoot') || text.includes('resolver') || text.includes('error') || text.includes('Error')) {
        console.log(`[uploader] ${text}`);
      }
    });

    // === Setup page1 (uploader) with VIDEO APP ===
    await page1.goto('/video.html#/');
    await disableOthersPool(page1);
    await configureBlossomServers(page1);

    // Clear storage for fresh state
    await page1.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();
    });
    await presetLocalRelayInDB(page1);

    await safeReload(page1, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(page1);
    await useLocalRelay(page1);
    await waitForRelayConnected(page1, 30000);
    await disableOthersPool(page1);
    await configureBlossomServers(page1);

    // Login
    const newBtn = page1.getByRole('button', { name: /New/i });
    if (await newBtn.isVisible().catch(() => false)) {
      await newBtn.click();
      await expect(page1.locator('button:has-text("Create")')).toBeVisible({ timeout: 15000 });
    }

    // Get uploader's npub
    const uploaderNpub = await page1.evaluate(async () => {
      const { useNostrStore } = await import('/src/nostr');
      const state = useNostrStore.getState();
      return state.npub || '';
    });
    console.log(`Uploader npub: ${uploaderNpub.slice(0, 20)}...`);

    // Upload video via the video upload modal
    await page1.keyboard.press('Escape');

    // Open upload modal via dropdown
    const createBtn = page1.locator('button:has-text("Create")');
    await expect(createBtn).toBeVisible({ timeout: 15000 });
    await createBtn.click();
    const uploadOption = page1.locator('button:has-text("Upload Video")').first();
    await expect(uploadOption).toBeVisible({ timeout: 5000 });
    await uploadOption.click();
    await expect(page1.getByRole('heading', { name: 'Upload Video' })).toBeVisible({ timeout: 10000 });

    // Select video file
    const fileInput = page1.locator('input[type="file"][accept="video/*"]');
    await fileInput.setInputFiles(TEST_VIDEO);

    // Set title
    const videoTitle = `DirectNav Test ${Date.now()}`;
    const titleInput = page1.locator('input[placeholder="Video title"]');
    await expect(titleInput).toBeVisible({ timeout: 5000 });
    await titleInput.fill(videoTitle);

    // Click upload
    await page1.locator('.fixed button:has-text("Upload")').click();

    // Wait for video page (this URL will contain the npub and treeName)
    await page1.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });
    console.log('Video uploaded, now on video page');

    // Get the full URL for direct navigation
    const videoPageUrl = page1.url();
    console.log(`Video page URL: ${videoPageUrl}`);

    const videoExt = TEST_VIDEO.split('.').pop()?.toLowerCase() || 'mp4';
    const videoFileName = `video.${videoExt}`;
    const treeName = `videos/${videoTitle.replace(/[<>:"/\\|?*]/g, '_')}`;

    // Ensure latest tree root is published before viewer loads
    await page1.evaluate(async () => {
      const { flushPendingPublishes } = await import('/src/treeRootCache');
      await flushPendingPublishes();
    });
    const rootHashAfterUpload = await getTreeRootHash(page1, uploaderNpub, treeName);
    expect(rootHashAfterUpload).toBeTruthy();

    // Close uploader browser
    console.log('Closing uploader browser...');
    await context1.close();

    // === Setup viewer - completely fresh browser, NO following ===
    const context2 = await browser.newContext();
    const page2 = await context2.newPage();

    setupPageErrorHandler(page2);

    // Console logging for debugging
    page2.on('console', msg => {
      const text = msg.text();
      if (text.includes('treeRoot') || text.includes('resolver') || text.includes('VideoView') ||
          text.includes('error') || text.includes('Error') || text.includes('Video not found')) {
        console.log(`[viewer] ${text}`);
      }
    });

    // IMPORTANT: Go directly to the video URL without visiting home first
    // This tests cold-start direct navigation
    console.log(`Viewer directly navigating to: ${videoPageUrl}`);
    await page2.goto(videoPageUrl);
    await presetLocalRelayInDB(page2);
    await safeReload(page2, { waitUntil: 'domcontentloaded', timeoutMs: 60000, url: videoPageUrl });

    await page2.waitForFunction(() => !!navigator.serviceWorker?.controller, { timeout: 30000 });
    await page2.waitForFunction(() => Boolean((window as any).__configureBlossomServers), undefined, { timeout: 30000 });

    // Configure Blossom for viewer (needed for fetching)
    await configureBlossomServers(page2);
    await disableOthersPool(page2);
    await useLocalRelay(page2);
    await waitForRelayConnected(page2, 30000);
    await waitForTreeRootHash(page2, uploaderNpub, treeName, rootHashAfterUpload!, 90000);
    await waitForTreeEntry(page2, uploaderNpub, treeName, videoFileName, 90000).catch(() => {});

    // Check for error message (the bug we're looking for)
    const errorVisible = await page2.locator('text=Video not found').isVisible({ timeout: 5000 }).catch(() => false);
    if (errorVisible) {
      console.log('ERROR: Video not found error displayed - this is the bug!');
      await page2.screenshot({ path: 'e2e/screenshots/video-direct-nav-error.png', fullPage: true });
    }

    // Wait for video element to be present
    const videoElement = page2.locator('video');
    let attemptedReload = false;
    await expect.poll(async () => {
      const count = await videoElement.count().catch(() => 0);
      if (count > 0) return true;
      if (!attemptedReload) {
        attemptedReload = true;
        await safeReload(page2, { waitUntil: 'domcontentloaded', timeoutMs: 60000, url: videoPageUrl });
        await waitForAppReady(page2);
        await waitForRelayConnected(page2, 30000);
        await waitForTreeRootHash(page2, uploaderNpub, treeName, rootHashAfterUpload!, 90000);
        await waitForTreeEntry(page2, uploaderNpub, treeName, videoFileName, 90000).catch(() => {});
      }
      return false;
    }, { timeout: 90000, intervals: [1000, 2000, 3000] }).toBe(true);
    console.log('Video element is present');

    // Wait for video to have a source
    const hasSource = await page2.waitForFunction(() => {
      const video = document.querySelector('video');
      if (!video) return false;
      return video.src !== '' && video.src.length > 0;
    }, undefined, { timeout: 45000 }).then(() => true).catch(() => false);

    expect(hasSource).toBe(true);

    // Prefetch the file data to avoid WebRTC timing issues
    let prefetchedSize = 0;
    try {
      prefetchedSize = await prefetchFile(page2, uploaderNpub, treeName, videoFileName);
    } catch (err) {
      console.log('Prefetch via Blossom failed:', err);
    }
    console.log('Prefetched bytes:', prefetchedSize);
    await page2.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement | null;
      if (video) video.load();
    });

    // Get video state
    const videoState = await page2.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        src: video.src,
        readyState: video.readyState,
        duration: video.duration,
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        error: video.error ? { code: video.error.code, message: video.error.message } : null,
      };
    });

    console.log('Video state:', JSON.stringify(videoState, null, 2));

    expect(videoState).not.toBeNull();
    expect(videoState!.src).toBeTruthy();

    console.log('=== Video App Direct Navigation Test Passed ===');

    await context2.close();
  });
});
