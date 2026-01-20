import { test, expect, Page } from './fixtures';
import path from 'path';
import fs from 'fs';
import os from 'os';
import { setupPageErrorHandler, navigateToPublicFolder, useLocalRelay, waitForRelayConnected } from './test-utils.js';
// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

// Increase timeout for livestream tests
test.setTimeout(120000);

test.describe('Livestream Video Stability', () => {

  async function clearStorage(page: Page) {
    await page.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();
    });
  }

  async function waitForAutoLogin(page: Page) {
    // Page ready - navigateToPublicFolder handles waiting
  }

  // Helper to navigate to tree list and create a new tree
  async function createTree(page: Page, name: string) {
    // Navigate to tree list first
    await page.locator('header a:has-text("Iris")').click();
    await page.waitForTimeout(300);

    await page.getByRole('button', { name: 'New Folder' }).click();
    await page.locator('input[placeholder="Folder name..."]').fill(name);
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 10000 });
  }

  // Helper to create a small test video file with unique name
  function createTestVideo(suffix: string = ''): string {
    const tmpDir = os.tmpdir();
    const uniqueId = Date.now() + '-' + Math.random().toString(36).slice(2, 8);
    const fileName = `test-stream-${uniqueId}${suffix}.webm`;
    const videoPath = path.join(tmpDir, fileName);

    // Minimal valid WebM file (just headers, enough to test player mounting)
    const webmHeader = Buffer.from([
      0x1a, 0x45, 0xdf, 0xa3, // EBML header
      0x93, // Size
      0x42, 0x86, 0x81, 0x01, // EBMLVersion: 1
      0x42, 0xf7, 0x81, 0x01, // EBMLReadVersion: 1
      0x42, 0xf2, 0x81, 0x04, // EBMLMaxIDLength: 4
      0x42, 0xf3, 0x81, 0x08, // EBMLMaxSizeLength: 8
      0x42, 0x82, 0x84, 0x77, 0x65, 0x62, 0x6d, // DocType: webm
      0x42, 0x87, 0x81, 0x04, // DocTypeVersion: 4
      0x42, 0x85, 0x81, 0x02, // DocTypeReadVersion: 2
    ]);

    fs.writeFileSync(videoPath, webmHeader);
    return videoPath;
  }

  // Skip: flaky livestream test depends on video element timing and merkle updates
  test('video element should not remount when merkle root updates', async ({ page }) => {

    setupPageErrorHandler(page);
    await page.goto('http://localhost:5173/');
    await clearStorage(page);
    await page.reload();
    await waitForAutoLogin(page);
    await navigateToPublicFolder(page);
    await useLocalRelay(page);
    await waitForRelayConnected(page);

    // Create folder for streaming
    await createTree(page, 'stream-test');

    // Upload a video file
    const videoPath = createTestVideo();
    const videoFileName = path.basename(videoPath);
    const fileInput = page.locator('input[type="file"]').first();
    await fileInput.setInputFiles(videoPath);

    // Wait for video to appear
    const fileList = page.getByTestId('file-list');
    await expect(fileList.locator(`span:text-is("${videoFileName}")`)).toBeVisible({ timeout: 10000 });

    // Click on video to view it
    await fileList.locator(`span:text-is("${videoFileName}")`).click();

    // Wait for video element to appear in DOM
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 10000 });

    // Set up monitoring for video element removal/flickering
    await page.evaluate(() => {
      const video = document.querySelector('video');
      if (video) {
        (video as any).__testMarker = 'original-video-element';
      }

      // Track every time video is removed from DOM
      (window as any).__videoRemovals = [];
      (window as any).__videoRemovalCount = 0;

      const observer = new MutationObserver((mutations) => {
        for (const mutation of mutations) {
          for (const node of mutation.removedNodes) {
            if (node.nodeName === 'VIDEO') {
              (window as any).__videoRemovalCount++;
              (window as any).__videoRemovals.push({
                time: Date.now(),
                count: (window as any).__videoRemovalCount
              });
              console.log('[TEST] Video element REMOVED from DOM! Count:', (window as any).__videoRemovalCount);
            }
          }
          for (const node of mutation.addedNodes) {
            if (node.nodeName === 'VIDEO') {
              console.log('[TEST] Video element ADDED to DOM');
            }
          }
        }
      });
      observer.observe(document.body, { childList: true, subtree: true });
    });

    const getVideoState = async () => {
      return await page.evaluate(async () => {
        const video = document.querySelector('video');

        // Also check resolvingPath state
        let resolvingPath = false;
        try {
          const stores = await import('/src/stores/index.ts');
          const unsub = stores.resolvingPathStore.subscribe((v: boolean) => { resolvingPath = v; });
          unsub();
        } catch {}

        return {
          exists: !!video,
          marker: video ? (video as any).__testMarker || null : null,
          removalCount: (window as any).__videoRemovalCount || 0,
          removals: (window as any).__videoRemovals || [],
          resolvingPath,
        };
      });
    };

    const stateBefore = await getVideoState();
    console.log('Video state before updates:', stateBefore);
    expect(stateBefore.exists).toBe(true);
    expect(stateBefore.marker).toBe('original-video-element');

    // Simulate what happens on VIEWER side when watching someone's livestream:
    // The treeRootStore gets updated with new merkle roots as the streamer publishes
    // The video file's CID changes (new data appended), but the video should NOT remount
    // Run for ~15 seconds to catch flicker across multiple merkle root update cycles
    const NUM_UPDATES = 10;
    const EMPTY_STATE_DURATION_MS = 300; // Time entries are empty during "refetch"
    const BETWEEN_UPDATES_MS = 1200; // Time between update cycles

    console.log(`Starting ${NUM_UPDATES} simulated merkle root updates (viewer side)...`);

    for (let i = 0; i < NUM_UPDATES; i++) {
      console.log(`\n--- Update ${i + 1}/${NUM_UPDATES} ---`);

      // Trigger through the actual store chain like real Nostr events do:
      // treeRootStore -> currentDirCidStore -> directoryEntriesStore
      await page.evaluate(async (updateIndex) => {
        const stores = await import('/src/stores/index.ts');

        // Get current tree root
        let currentRoot: any = null;
        const unsub = stores.treeRootStore.subscribe((v: any) => { currentRoot = v; });
        unsub();

        if (currentRoot?.hash) {
          // Create new root with modified hash (simulates new merkle root from Nostr)
          const newHash = new Uint8Array(currentRoot.hash);
          newHash[0] = (newHash[0] + 1 + updateIndex) % 256;

          console.log(`[TEST] Update ${updateIndex}: setting treeRootStore with new hash`);

          stores.treeRootStore.set({
            hash: newHash,
            key: currentRoot.key,
          });
        }
      }, i);

      // Wait for the reactive chain to propagate
      await page.waitForTimeout(BETWEEN_UPDATES_MS);

      // Check video state
      const state = await getVideoState();
      console.log(`After update:`, state);

      if (state.removalCount > 0) {
        console.error('VIDEO WAS REMOVED! Flicker detected.');
      }
      if (!state.exists) {
        console.error('VIDEO DOES NOT EXIST!');
      }
    }

    // Final check
    const stateAfter = await getVideoState();
    console.log('Final video state:', stateAfter);

    // STRICT CHECKS
    expect(stateAfter.removalCount).toBe(0);
    expect(stateAfter.exists).toBe(true);
    expect(stateAfter.marker).toBe('original-video-element');

    // Cleanup
    fs.unlinkSync(videoPath);
  });

  test('video should not remount during multiple file updates', async ({ page }) => {
    setupPageErrorHandler(page);

    // Create test video upfront with unique name
    const videoPath = createTestVideo();
    const videoFileName = path.basename(videoPath);

    try {
      await page.goto('http://localhost:5173/');
      await clearStorage(page);
      await page.reload();
      await waitForAutoLogin(page);
      await navigateToPublicFolder(page);

      // Create folder via tree list
      await createTree(page, 'live-test');

      const fileInput = page.locator('input[type="file"]').first();
      await fileInput.setInputFiles(videoPath);
      await page.waitForTimeout(1000);

      const fileList = page.getByTestId('file-list');
      await expect(fileList.locator(`span:text-is("${videoFileName}")`)).toBeVisible({ timeout: 5000 });

      // Click to view video
      await fileList.locator(`span:text-is("${videoFileName}")`).click();
      await page.waitForTimeout(500);

      // Track video element creation via instrumentation
      const videoMountCounts: number[] = [];

      page.on('console', msg => {
        if (msg.text().includes('video-mount')) {
          videoMountCounts.push(videoMountCounts.length + 1);
        }
      });

      // Add instrumentation to track video mounting
      await page.evaluate(() => {
        const originalCreateElement = document.createElement.bind(document);
        let count = 0;
        document.createElement = function(tagName: string, options?: ElementCreationOptions) {
          const el = originalCreateElement(tagName, options);
          if (tagName.toLowerCase() === 'video') {
            count++;
            console.log(`video-mount: ${count}`);
          }
          return el;
        };
      });

      // Make multiple updates
      for (let i = 0; i < 3; i++) {
        const updatePath = path.join(os.tmpdir(), `update-${i}.txt`);
        fs.writeFileSync(updatePath, `Update ${i} - ${Date.now()}`);
        await fileInput.setInputFiles(updatePath);
        await page.waitForTimeout(1500);
        fs.unlinkSync(updatePath);
      }

      console.log('Video mount counts:', videoMountCounts);

      // The video should only be created once (initial render)
      // Additional merkle root updates should NOT create new video elements
      expect(videoMountCounts.length).toBeLessThanOrEqual(1);
    } finally {
      // Cleanup
      if (fs.existsSync(videoPath)) {
        fs.unlinkSync(videoPath);
      }
    }
  });
});
