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
    test.setTimeout(240000);

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
      const gotoWithRetry = async (url: string, attempts = 3) => {
        let lastError: unknown;
        for (let attempt = 0; attempt < attempts; attempt++) {
          try {
            await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 60000 });
            return;
          } catch (err) {
            lastError = err;
          }
        }
        throw lastError;
      };

      // Setup fresh user
      await gotoWithRetry('/');
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
      await expect(videoEl).toBeAttached({ timeout: 15000 });
      await page.waitForFunction(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        return !!video && video.readyState >= 2;
      }, undefined, { timeout: 60000 });

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

      const targetTime = Math.min(15, Math.max(1, videoInfo.duration - 1));
      await page.evaluate((time: number) => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        if (!video) return;
        video.muted = true;
        video.currentTime = time;
      }, targetTime);

      const playButton = page.getByRole('button', { name: 'Play video' });
      if (await playButton.isVisible().catch(() => false)) {
        await playButton.click();
      }
      await page.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        if (video && video.paused) {
          void video.play();
        }
      });

      await page.waitForFunction(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        return !!video && !video.paused;
      }, undefined, { timeout: 60000 });
      await page.waitForFunction(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        return !!video && video.currentTime > 0.1;
      }, undefined, { timeout: 60000 });

      const playbackState = await page.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement | null;
        if (!video) {
          return { ok: false, error: 'Video element missing' };
        }
        let decodedFrames: number | null = null;
        let corruptedFrames: number | null = null;
        if ('getVideoPlaybackQuality' in video) {
          const q = (video as any).getVideoPlaybackQuality();
          decodedFrames = q?.totalVideoFrames ?? 0;
          corruptedFrames = q?.corruptedVideoFrames ?? 0;
        }
        return {
          ok: true,
          currentTime: video.currentTime,
          readyState: video.readyState,
          error: video.error?.message || null,
          decodedFrames,
          corruptedFrames,
        };
      });

      console.log('Playback state:', JSON.stringify(playbackState, null, 2));
      expect(playbackState.ok).toBe(true);
      if (playbackState.ok) {
        expect(playbackState.error).toBeNull();
        expect(playbackState.readyState).toBeGreaterThanOrEqual(1);
        if (playbackState.decodedFrames !== null) {
          expect(playbackState.decodedFrames).toBeGreaterThan(0);
          expect(playbackState.corruptedFrames).toBe(0);
        }
      }

    } finally {
      await context.close();
      await browser.close();
    }
  });
});
