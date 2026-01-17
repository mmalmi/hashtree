/**
 * Chunk Boundary Video Test
 * 
 * Tests that video files uploaded via putFile (using putFileEncrypted)
 * play correctly across chunk boundaries without garbling.
 */
import { test, expect } from './fixtures';
import { chromium } from '@playwright/test';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, waitForAppReady } from './test-utils';
// Tests create their own browser contexts - safe for parallel execution

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// Use a video larger than 2MB to ensure multiple chunks
const TEST_VIDEO = path.join(__dirname, 'fixtures', 'big-buck-bunny-30s.webm');

test.describe('Chunk Boundary Video', () => {
  test('uploaded video plays without garbling at chunk boundaries', async () => {
    test.slow();
    test.setTimeout(60000);

    // Verify test video exists
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);
    const videoStats = fs.statSync(TEST_VIDEO);
    console.log(`Test video: ${TEST_VIDEO}, size: ${videoStats.size} bytes`);

    const browser = await chromium.launch({
      args: ['--autoplay-policy=no-user-gesture-required'],
    });
    const context = await browser.newContext();
    const page = await context.newPage();

    page.on('console', msg => {
      const text = msg.text();
      if (text.includes('[SwFileHandler]') || text.includes('error')) {
        console.log(`[Page] ${text}`);
      }
    });
    setupPageErrorHandler(page);

    try {
      // Setup fresh user
      await page.goto('/');
      await disableOthersPool(page);
      await page.evaluate(async () => {
        const dbs = await indexedDB.databases();
        for (const db of dbs) {
          if (db.name) indexedDB.deleteDatabase(db.name);
        }
        localStorage.clear();
      });
      await page.reload();
      await waitForAppReady(page);
      await disableOthersPool(page);
      await navigateToPublicFolder(page);

      const url = page.url();
      const match = url.match(/npub1[a-z0-9]+/);
      const npub = match ? match[0] : '';
      console.log(`User: ${npub.slice(0, 20)}...`);

      // Read video file and upload using putFile
      const videoBuffer = fs.readFileSync(TEST_VIDEO);
      console.log(`Uploading ${videoBuffer.length} bytes via putFile...`);

      const fileCid = await page.evaluate(async (videoBase64: string) => {
        const { getTree } = await import('/src/store.ts');
        const { autosaveIfOwn } = await import('/src/nostr.ts');
        const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
        const { parseRoute } = await import('/src/utils/route.ts');

        const tree = getTree();
        const route = parseRoute();
        let rootCid = getTreeRootSync(route.npub, route.treeName);

        // If no tree exists yet, create an empty one
        if (!rootCid) {
          const { cid } = await tree.putDirectory([]);
          rootCid = cid;
        }

        const videoBytes = Uint8Array.from(atob(videoBase64), c => c.charCodeAt(0));

        // Use putFile which internally uses putFileEncrypted
        const result = await tree.putFile(videoBytes);
        console.log('[Test] putFile result:', result);

        // Add to existing tree
        const newRootCid = await tree.setEntry(rootCid, [], 'chunk-test.webm', result.cid, result.size);
        autosaveIfOwn(newRootCid);

        // Get hash for verification
        const hashHex = Array.from(newRootCid.hash).map((b: number) => b.toString(16).padStart(2, '0')).join('');
        return { hashHex, size: result.size };
      }, videoBuffer.toString('base64'));

      console.log(`File uploaded, CID hash: ${fileCid.hashHex.slice(0, 16)}..., size: ${fileCid.size}`);

      // Navigate to the uploaded file
      const videoUrl = `http://localhost:5173/#/${npub}/public/chunk-test.webm`;
      console.log(`Navigating to: ${videoUrl}`);
      await page.goto(videoUrl);
      const videoEl = page.locator('video');
      await expect(videoEl).toBeVisible({ timeout: 15000 });
      await page.waitForFunction(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        return !!video && video.readyState >= 2;
      }, { timeout: 15000 });

      // Check video element
      const videoInfo = await page.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement;
        return {
          hasVideo: !!video,
          src: video?.src || '',
          duration: video?.duration || 0,
          readyState: video?.readyState || 0,
          error: video?.error?.message || null,
        };
      });
      console.log('Video info:', JSON.stringify(videoInfo, null, 2));

      // Play video and capture frames
      await page.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement;
        if (video) {
          video.currentTime = 0;
          video.play();
        }
      });

      // Capture frames during playback
      const startTime = await page.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        return video?.currentTime || 0;
      });
      await page.waitForFunction(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        return !!video && !video.paused;
      }, { timeout: 10000 });
      for (let i = 0; i < 8; i++) {
        const targetTime = startTime + (i + 1) * 0.5;
        await page.waitForFunction((target: number) => {
          const video = document.querySelector('video') as HTMLVideoElement | null;
          return !!video && video.currentTime >= target;
        }, targetTime, { timeout: 10000 });
        const state = await page.evaluate(() => {
          const video = document.querySelector('video') as HTMLVideoElement;
          let decodedFrames = 0, corruptedFrames = 0;
          if (video && 'getVideoPlaybackQuality' in video) {
            const q = (video as any).getVideoPlaybackQuality();
            decodedFrames = q?.totalVideoFrames || 0;
            corruptedFrames = q?.corruptedVideoFrames || 0;
          }
          return {
            currentTime: video?.currentTime || 0,
            decodedFrames,
            corruptedFrames,
          };
        });
        console.log(`t=${state.currentTime.toFixed(1)}s, frames=${state.decodedFrames}, corrupted=${state.corruptedFrames}`);
        
        // Screenshot
        const videoEl = page.locator('video');
        if (await videoEl.isVisible()) {
          await videoEl.screenshot({ path: `test-results/chunk-boundary-${i}.png` });
        }
      }

      // Final state
      const finalState = await page.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement;
        let decodedFrames = 0, corruptedFrames = 0;
        if (video && 'getVideoPlaybackQuality' in video) {
          const q = (video as any).getVideoPlaybackQuality();
          decodedFrames = q?.totalVideoFrames || 0;
          corruptedFrames = q?.corruptedVideoFrames || 0;
        }
        return { decodedFrames, corruptedFrames };
      });

      console.log(`Final: ${finalState.decodedFrames} frames, ${finalState.corruptedFrames} corrupted`);

      // Assertions
      expect(finalState.decodedFrames).toBeGreaterThan(0);
      expect(finalState.corruptedFrames).toBe(0);

    } finally {
      await context.close();
      await browser.close();
    }
  });
});
