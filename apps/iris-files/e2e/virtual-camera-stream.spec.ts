/**
 * Virtual Camera Livestream Test
 *
 * Uses Chromium's built-in fake camera feature instead of mocking MediaRecorder.
 * This is more realistic because:
 * - Real MediaRecorder API is used
 * - Real camera capture timing and chunk generation
 * - Real WebM encoding by the browser
 *
 * The only fake part is the video source (Y4M file instead of physical camera).
 */
import { test, expect, type Page } from './fixtures';
import { chromium } from '@playwright/test';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, followUser, waitForAppReady, ensureLoggedIn, useLocalRelay, waitForRelayConnected, configureBlossomServers } from './test-utils';
// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// Y4M video file for Chromium's fake camera (10 seconds, 320x240, 15fps)
const FAKE_CAM_VIDEO = path.join(__dirname, 'fixtures', 'test-video-fake-cam.y4m');

// Setup fresh user with cleared storage
async function setupFreshUser(page: Page): Promise<void> {
  await page.goto('http://localhost:5173');

  // Clear storage
  await page.evaluate(async () => {
    const dbs = await indexedDB.databases();
    for (const db of dbs) {
      if (db.name) indexedDB.deleteDatabase(db.name);
    }
    localStorage.clear();
    sessionStorage.clear();
  });

  await page.reload();
  await page.waitForTimeout(500);
  await waitForAppReady(page);
  await ensureLoggedIn(page);
  await useLocalRelay(page);
  await waitForRelayConnected(page);
}

async function dismissBlockingModal(page: Page): Promise<void> {
  const modal = page.locator('.fixed.inset-0').first();
  if (!(await modal.isVisible().catch(() => false))) return;
  await page.keyboard.press('Escape').catch(() => {});
  await page.waitForTimeout(200);
  if (await modal.isVisible().catch(() => false)) {
    const closeBtn = modal.getByRole('button', { name: /close|cancel|back/i }).first();
    if (await closeBtn.isVisible().catch(() => false)) {
      await closeBtn.click().catch(() => {});
    } else {
      await modal.click({ position: { x: 5, y: 5 } }).catch(() => {});
    }
  }
  await modal.waitFor({ state: 'hidden', timeout: 5000 }).catch(() => {});
}

// Get user's npub (prefer store; fallback to UI navigation)
async function getNpub(page: Page): Promise<string> {
  const npub = await page.evaluate(() => {
    const store = (window as any).__nostrStore;
    return store?.getState?.()?.npub ?? null;
  });
  if (npub) return npub;

  await dismissBlockingModal(page);
  const publicLink = page.getByRole('link', { name: 'public' }).first();
  await expect(publicLink).toBeVisible({ timeout: 15000 });
  await publicLink.click();
  await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });

  const url = page.url();
  const match = url.match(/npub1[a-z0-9]+/);
  if (!match) throw new Error('Could not find npub in URL');
  return match[0];
}

test.describe('Virtual Camera Livestream', () => {
  // Skip: flaky virtual camera test depends on fake-device-for-media-stream timing
  test('streaming with real MediaRecorder and virtual camera', async ({}, testInfo) => {
    test.slow();
    test.setTimeout(120000);

    // Verify Y4M file exists
    expect(fs.existsSync(FAKE_CAM_VIDEO)).toBe(true);
    console.log(`Using fake camera video: ${FAKE_CAM_VIDEO}`);

    const relayUrl = `ws://localhost:4736/w${testInfo.workerIndex}`;
    process.env.PW_TEST_RELAY_URL = relayUrl;

    // Launch browser with fake camera args
    const browser = await chromium.launch({
      args: [
        '--use-fake-device-for-media-stream',
        '--use-fake-ui-for-media-stream',
        `--use-file-for-fake-video-capture=${FAKE_CAM_VIDEO}`,
        // Allow autoplay for video elements
        '--autoplay-policy=no-user-gesture-required',
      ],
    });

    // Create two separate contexts (broadcaster and viewer)
    const contextA = await browser.newContext({
      permissions: ['camera', 'microphone'],
    });
    const contextB = await browser.newContext({
      permissions: ['camera', 'microphone'],
    });
    await contextA.addInitScript((url: string) => {
      (window as unknown as { __testRelayUrl?: string }).__testRelayUrl = url;
    }, relayUrl);
    await contextB.addInitScript((url: string) => {
      (window as unknown as { __testRelayUrl?: string }).__testRelayUrl = url;
    }, relayUrl);

    const pageA = await contextA.newPage(); // Broadcaster
    const pageB = await contextB.newPage(); // Viewer

    // Logging
    const logs = { broadcaster: [] as string[], viewer: [] as string[] };

    pageA.on('console', msg => {
      const text = msg.text();
      logs.broadcaster.push(text);
      if (text.includes('[Stream]') || text.includes('📤') ||
          text.includes('MediaRecorder') || text.includes('Publish') ||
          text.includes('[WebM]')) {
        console.log(`[A] ${text}`);
      }
    });

    pageB.on('console', msg => {
      const text = msg.text();
      logs.viewer.push(text);
      if (text.includes('SwFileHandler') || text.includes('MediaPlayer') ||
          text.includes('[SW]') || text.includes('Poll') ||
          text.includes('404') || text.includes('error')) {
        console.log(`[B] ${text}`);
      }
    });

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // === Setup Broadcaster ===
      console.log('\n=== Setting up Broadcaster (Virtual Camera) ===');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      console.log(`Broadcaster: ${npubA.slice(0, 20)}...`);
      await configureBlossomServers(pageA);

      // === Setup Viewer ===
      console.log('\n=== Setting up Viewer ===');
      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      console.log(`Viewer: ${npubB.slice(0, 20)}...`);
      await configureBlossomServers(pageB);

      // === Mutual follows for WebRTC ===
      console.log('\n=== Setting up mutual follows ===');
      await followUser(pageA, npubB);
      await followUser(pageB, npubA);
      console.log('Mutual follows established');

      // Prompt WebRTC connections
      await Promise.all([
        pageA.evaluate(() => (window as any).__getWorkerAdapter?.()?.sendHello?.()),
        pageB.evaluate(() => (window as any).__getWorkerAdapter?.()?.sendHello?.()),
      ]);
      await pageA.waitForTimeout(5000);

      const peersA = await pageA.evaluate(() => {
        const store = (window as any).webrtcStore;
        return store?.getPeers?.()?.map((p: any) => ({
          pubkey: p.pubkey?.slice(0, 16),
          isConnected: p.isConnected,
        })) || [];
      });
      console.log('Broadcaster peers:', JSON.stringify(peersA));

      // === Start streaming with real camera ===
      console.log('\n=== Starting stream with virtual camera ===');
      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await pageA.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });

      const streamLink = pageA.getByRole('link', { name: 'Stream' });
      await expect(streamLink).toBeVisible({ timeout: 10000 });
      await streamLink.click();
      await pageA.waitForTimeout(500);

      // Start camera (uses virtual camera)
      const startCameraBtn = pageA.getByRole('button', { name: 'Start Camera' });
      await expect(startCameraBtn).toBeVisible({ timeout: 10000 });
      await startCameraBtn.click();

      // Wait for camera to initialize
      await pageA.waitForTimeout(2000);

      // Verify video preview is showing
      const videoPreview = pageA.locator('video');
      await expect(videoPreview).toBeVisible({ timeout: 5000 });

      const testFilename = `vcam_test_${Date.now()}`;
      const filenameInput = pageA.locator('input[placeholder="filename"]');
      await expect(filenameInput).toBeVisible({ timeout: 10000 });
      await filenameInput.fill(testFilename);

      // Start recording
      const startRecordingBtn = pageA.getByRole('button', { name: /Start Recording/ });
      await expect(startRecordingBtn).toBeVisible({ timeout: 10000 });
      await startRecordingBtn.click();

      console.log('Recording started with real MediaRecorder');

      // === Viewer navigates to stream ===
      console.log('\n=== Viewer navigating to stream ===');
      const streamUrl = `http://localhost:5173/#/${npubA}/public/${testFilename}.webm?live=1`;
      console.log(`Stream URL: ${streamUrl}`);
      await pageB.goto(streamUrl);

      // Wait for initial load
      await pageB.waitForTimeout(3000);

      // Track duration and size growth
      const durationSamples: { time: number; duration: number; size: number; viewerSize: number }[] = [];
      let lastKnownSize = 0;

      const getViewerState = async () => {
        return await pageB.evaluate(() => {
          const video = document.querySelector('video') as HTMLVideoElement;
          let decodedFrames = 0, droppedFrames = 0, corruptedFrames = 0;
          if (video && 'getVideoPlaybackQuality' in video) {
            const quality = (video as any).getVideoPlaybackQuality();
            decodedFrames = quality?.totalVideoFrames || 0;
            droppedFrames = quality?.droppedVideoFrames || 0;
            corruptedFrames = quality?.corruptedVideoFrames || 0;
          }
          return {
            hasVideo: !!video,
            videoSrc: video?.src || null,
            videoReadyState: video?.readyState || 0,
            videoDuration: video?.duration || 0,
            currentTime: video?.currentTime || 0,
            buffered: video?.buffered?.length > 0 ? video.buffered.end(video.buffered.length - 1) : 0,
            videoError: video?.error?.message || null,
            decodedFrames,
            droppedFrames,
            corruptedFrames,
          };
        });
      };

      // Get broadcaster stats
      const getBroadcasterStats = async () => {
        return await pageA.evaluate(async () => {
          const { getStreamState } = await import('/src/components/stream/streamState.ts');
          const state = getStreamState();
          return {
            isRecording: state.isRecording,
            recordingTime: state.recordingTime,
            stats: state.streamStats,
          };
        });
      };

      // Sample duration and size every 3 seconds for 30 seconds
      console.log('\n=== Tracking stream growth (real MediaRecorder) ===');
      let durationGrowthCount = 0;
      let prevDuration = 0;
      let sizeGrowthCount = 0;

      for (let i = 0; i < 10; i++) {
        const viewerState = await getViewerState();
        const broadcasterStats = await getBroadcasterStats();
        const duration = isNaN(viewerState.videoDuration) ? 0 : viewerState.videoDuration;

        const viewerFileSize = await pageB.evaluate(async () => {
          const video = document.querySelector('video') as HTMLVideoElement;
          if (!video?.src || video.src.startsWith('blob:')) return null;
          try {
            const resp = await fetch(video.src, { method: 'HEAD' });
            return parseInt(resp.headers.get('content-length') || '0', 10);
          } catch {
            return null;
          }
        });

        const broadcasterSize = broadcasterStats.stats?.totalSize || 0;
        if (broadcasterSize > lastKnownSize) {
          sizeGrowthCount++;
          lastKnownSize = broadcasterSize;
        }

        durationSamples.push({
          time: i * 3,
          duration,
          size: broadcasterSize,
          viewerSize: viewerFileSize || 0,
        });

        // Only count growth between finite durations
        if (isFinite(duration) && isFinite(prevDuration) && duration > prevDuration + 0.5) {
          durationGrowthCount++;
        }
        if (isFinite(duration)) {
          prevDuration = duration;
        }

        console.log(
          `t=${i * 3}s: viewer_dur=${isFinite(duration) ? duration.toFixed(1) : 'Inf'}s, ` +
          `broadcaster_size=${(broadcasterSize / 1024).toFixed(1)}KB, ` +
          `frames=${viewerState.decodedFrames}, corrupted=${viewerState.corruptedFrames}`
        );

        // Screenshot every sample
        await pageB.screenshot({ path: `test-results/vcam-t${i * 3}s.png`, fullPage: true });

        await pageB.waitForTimeout(3000);
      }

      // Stop recording
      const stopBtn = pageA.getByRole('button', { name: /Stop Recording/ });
      if (await stopBtn.isVisible({ timeout: 1000 }).catch(() => false)) {
        await stopBtn.click();
        await pageA.waitForTimeout(2000);
      }

      // === Chunk boundary playback test ===
      // The file should now be >2MB (chunked). Play video and capture frames
      // at chunk intersection to verify no garbling.
      console.log('\n=== Chunk Boundary Visual Test ===');

      // Reload to get final file version
      await pageB.reload();
      await pageB.waitForTimeout(2000);

      // Navigate to the stream file directly (not live mode)
      const finalStreamUrl = `http://localhost:5173/#/${npubA}/public/${testFilename}.webm`;
      await pageB.goto(finalStreamUrl);
      await pageB.waitForTimeout(3000);

      // Get final file info
      const fileInfo = await pageB.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement;
        return {
          duration: video?.duration || 0,
          src: video?.src || '',
        };
      });
      console.log(`Final video duration: ${fileInfo.duration}s`);

      // Play video and seek to different positions, taking screenshots
      // Chunk boundary is around 2MB. With ~100KB/s video, that's around 20s into video
      // But for this test, just play through and capture frames
      await pageB.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement;
        if (video) {
          video.currentTime = 0;
          video.play();
        }
      });

      // Capture frames while playing - take screenshots every 2 seconds
      for (let i = 0; i < 8; i++) {
        await pageB.waitForTimeout(2000);
        const playState = await pageB.evaluate(() => {
          const video = document.querySelector('video') as HTMLVideoElement;
          return {
            currentTime: video?.currentTime || 0,
            paused: video?.paused,
            readyState: video?.readyState,
          };
        });
        console.log(`Playback t=${playState.currentTime.toFixed(1)}s, paused=${playState.paused}`);

        // Screenshot the video element specifically
        const videoEl = pageB.locator('video');
        if (await videoEl.isVisible()) {
          await videoEl.screenshot({ path: `test-results/vcam-playback-${i}.png` });
        }
      }

      console.log('Chunk boundary screenshots saved - check test-results/vcam-playback-*.png for garbling');

      // Final state
      const finalState = await getViewerState();
      console.log('\n=== Final Viewer State ===');
      console.log(JSON.stringify(finalState, null, 2));

      // Final screenshot
      await pageB.screenshot({ path: 'test-results/vcam-final.png', fullPage: true });

      // === Analysis ===
      console.log('\n=== Stream Growth Analysis ===');
      const firstSize = durationSamples[0].size;
      const lastSize = durationSamples[durationSamples.length - 1].size;
      console.log(`Broadcaster file size: ${(firstSize / 1024).toFixed(1)}KB -> ${(lastSize / 1024).toFixed(1)}KB`);
      console.log(`Size grew ${sizeGrowthCount} times during streaming`);
      console.log(`Final decoded frames: ${finalState.decodedFrames}, corrupted: ${finalState.corruptedFrames}`);

      // Check size growth over time
      console.log('\n=== Viewer Size Over Time ===');
      durationSamples.forEach((s) => {
        console.log(`t=${s.time}s: ${(s.size / 1024).toFixed(1)}KB`);
      });

      // === Assertions ===
      expect(finalState.hasVideo).toBe(true);
      expect(finalState.videoReadyState).toBeGreaterThan(0);

      // File size should have grown multiple times during streaming
      console.log(`File size grew ${sizeGrowthCount} times`);
      expect(sizeGrowthCount).toBeGreaterThanOrEqual(3);

      // Final file should be much larger than first
      expect(lastSize).toBeGreaterThan(firstSize * 2);

      // Note: Duration is Infinity for MediaRecorder WebM (no duration metadata)
      // This is expected behavior - we focus on file size and frame count instead
      const lastDuration = durationSamples[durationSamples.length - 1].duration;
      if (!isFinite(lastDuration)) {
        console.log('Note: Duration is Infinity (MediaRecorder WebM has no duration element - expected)');
      }

      // Viewer should have decoded frames (video is playing)
      expect(finalState.decodedFrames).toBeGreaterThanOrEqual(20);
      expect(finalState.corruptedFrames).toBe(0);

      // Buffered amount should be reasonable (video is playing)
      expect(finalState.buffered).toBeGreaterThan(1);

      console.log(`SUCCESS: Virtual camera streaming works.`);
      console.log(`  File size: ${(firstSize / 1024).toFixed(1)}KB -> ${(lastSize / 1024).toFixed(1)}KB (${sizeGrowthCount} updates)`);
      console.log(`  Frames: ${finalState.decodedFrames} decoded, ${finalState.corruptedFrames} corrupted`);
      console.log(`  Buffered: ${finalState.buffered.toFixed(1)}s`);

    } finally {
      await contextA.close();
      await contextB.close();
      await browser.close();
    }
  });
});
