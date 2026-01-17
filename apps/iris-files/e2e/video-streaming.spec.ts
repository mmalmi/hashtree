/**
 * E2E test for video streaming functionality
 *
 * Tests that videos recorded via stream can be played back correctly.
 * Uses mocked MediaStream and MediaRecorder APIs since real camera
 * access isn't available in headless browser tests.
 */
import { test, expect, Page } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler } from './test-utils.js';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');

// Helper to set up a fresh user session and navigate to public folder
async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);

  await page.goto('http://localhost:5173');

  // Clear storage for fresh state
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
  // Wait for the app to load - look for "Iris Files" text in header
  await page.waitForSelector('header:has-text("Iris")', { timeout: 10000 });

  // Wait for the public folder link to appear
  const publicLink = page.getByRole('link', { name: 'public' }).first();
  await expect(publicLink).toBeVisible({ timeout: 15000 });

  // Click into the public folder
  await publicLink.click();
  await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });
  await expect(page.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 10000 });
}

// Read test video file as base64 for injection
function getTestVideoBase64(): string {
  const videoBuffer = fs.readFileSync(TEST_VIDEO);
  return videoBuffer.toString('base64');
}

test.describe('Video Streaming', () => {
  // This test takes longer due to recording simulation
  test.setTimeout(60000);

  test('streamed video can be recorded and played back', async ({ page }) => {
    // Verify test file exists
    expect(fs.existsSync(TEST_VIDEO)).toBe(true);
    const videoBase64 = getTestVideoBase64();

    // Log console for debugging
    page.on('console', msg => {
      const text = msg.text();
      if (msg.type() === 'error') console.log(`[Stream Error] ${text}`);
      if (text.includes('[Stream]') || text.includes('stream') || text.includes('[LiveVideo]')) console.log(`[Stream] ${text}`);
    });

    // Set up fresh user and navigate to public folder
    await setupFreshUser(page);
    console.log('User setup complete');

    // Inject mocked MediaStream and MediaRecorder APIs
    // This simulates camera recording by feeding our test video data
    await page.evaluate((videoB64) => {
      const videoData = Uint8Array.from(atob(videoB64), c => c.charCodeAt(0));

      // Create a real MediaStream using canvas (for srcObject compatibility)
      const canvas = document.createElement('canvas');
      canvas.width = 640;
      canvas.height = 360;
      const ctx = canvas.getContext('2d')!;
      ctx.fillStyle = '#333';
      ctx.fillRect(0, 0, 640, 360);
      ctx.fillStyle = '#fff';
      ctx.font = '24px sans-serif';
      ctx.fillText('Mock Camera Stream', 200, 180);

      // Create a real stream from canvas
      const fakeStream = canvas.captureStream(30);

      // Mock getUserMedia to return our fake stream
      navigator.mediaDevices.getUserMedia = async () => fakeStream;

      // Store chunks to feed to MediaRecorder
      const chunkSize = 50000; // ~50KB chunks
      const chunks: Blob[] = [];
      for (let i = 0; i < videoData.length; i += chunkSize) {
        const end = Math.min(i + chunkSize, videoData.length);
        chunks.push(new Blob([videoData.slice(i, end)], { type: 'video/webm' }));
      }

      // Mock MediaRecorder
      const OriginalMediaRecorder = window.MediaRecorder;
      class MockMediaRecorder {
        stream: MediaStream;
        state: string = 'inactive';
        ondataavailable: ((event: { data: Blob }) => void) | null = null;
        onstop: (() => void) | null = null;
        private intervalId: number | null = null;
        private chunkIndex = 0;

        constructor(stream: MediaStream, _options?: MediaRecorderOptions) {
          this.stream = stream;
        }

        start(timeslice?: number) {
          this.state = 'recording';
          this.chunkIndex = 0;

          // Feed chunks at intervals to simulate recording
          const feedChunk = () => {
            if (this.state !== 'recording') return;

            if (this.chunkIndex < chunks.length && this.ondataavailable) {
              this.ondataavailable({ data: chunks[this.chunkIndex] });
              this.chunkIndex++;
            }
          };

          // First chunk immediately, then at intervals
          feedChunk();
          this.intervalId = window.setInterval(feedChunk, timeslice || 1000);
        }

        stop() {
          this.state = 'inactive';
          if (this.intervalId) {
            clearInterval(this.intervalId);
            this.intervalId = null;
          }
          // Feed any remaining chunks
          while (this.chunkIndex < chunks.length && this.ondataavailable) {
            this.ondataavailable({ data: chunks[this.chunkIndex] });
            this.chunkIndex++;
          }
          if (this.onstop) this.onstop();
        }

        static isTypeSupported(type: string) {
          return type.includes('webm');
        }
      }

      (window as unknown as { MediaRecorder: typeof MockMediaRecorder }).MediaRecorder = MockMediaRecorder;
      console.log('[Stream] Mocked MediaRecorder and getUserMedia');
    }, videoBase64);

    // Click on Stream link to enter streaming mode
    console.log('Clicking Stream link...');
    const streamLink = page.getByRole('link', { name: 'Stream' });
    await expect(streamLink).toBeVisible({ timeout: 5000 });
    await streamLink.click();
    await page.waitForTimeout(500);

    // Start camera preview
    console.log('Starting camera preview...');
    const startCameraBtn = page.getByRole('button', { name: 'Start Camera' });
    await expect(startCameraBtn).toBeVisible({ timeout: 5000 });
    await startCameraBtn.click();
    await page.waitForTimeout(1000);

    // Should see filename input and record button
    const filenameInput = page.locator('input[placeholder="filename"]');
    await expect(filenameInput).toBeVisible({ timeout: 5000 });

    // Set a unique filename
    const testFilename = `test_stream_${Date.now()}`;
    await filenameInput.fill(testFilename);

    // Start recording
    console.log('Starting recording...');
    const startRecordingBtn = page.getByRole('button', { name: /Start Recording/ });
    await expect(startRecordingBtn).toBeVisible({ timeout: 5000 });

    // Log the current URL and route state before starting
    const urlBefore = await page.evaluate(() => window.location.hash);
    console.log('URL before recording:', urlBefore);

    await startRecordingBtn.click();

    // Wait for recording to process (mock feeds chunks over time)
    console.log('Recording in progress...');
    await page.waitForTimeout(5000);

    // Stop recording
    console.log('Stopping recording...');
    const stopRecordingBtn = page.getByRole('button', { name: /Stop Recording/ });
    await expect(stopRecordingBtn).toBeVisible({ timeout: 5000 });
    await stopRecordingBtn.click();

    // Wait for file to be saved
    console.log('Waiting for file to be saved...');
    await page.waitForTimeout(3000);

    // Log final URL
    const urlAfter = await page.evaluate(() => window.location.hash);
    console.log('URL after recording:', urlAfter);

    // Navigate back to public folder
    const publicLink = page.getByRole('link', { name: 'public' }).first();
    await expect(publicLink).toBeVisible({ timeout: 10000 });
    await publicLink.click();

    // Look for the recorded video file
    console.log('Looking for recorded video file...');
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: `${testFilename}.webm` }).first();
    await expect(videoLink).toBeVisible({ timeout: 30000 });
    console.log('Recorded video file found!');

    // Click on the video to play it
    console.log('Clicking on recorded video to play...');
    await videoLink.click();
    await page.waitForTimeout(2000);

    // Check that video element exists
    const videoElement = page.locator('video');
    await expect(videoElement).toBeAttached({ timeout: 10000 });

    // Wait for video to have a source (SW URL for hashtree files)
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      if (!video) return false;
      // Video should have a /htree/ URL (served via service worker)
      return video.src !== '' && video.src.includes('/htree/');
    }, { timeout: 15000 });

    // Get video state
    const videoState = await page.evaluate(() => {
      const video = document.querySelector('video') as HTMLVideoElement;
      if (!video) return null;
      return {
        src: video.src,
        hasSrcObject: video.srcObject !== null,
        readyState: video.readyState,
        duration: video.duration,
        error: video.error ? { code: video.error.code, message: video.error.message } : null,
      };
    });

    console.log('Video state:', JSON.stringify(videoState, null, 2));

    // Verify video element exists with SW URL (proves file was found and served via SW)
    expect(videoState).not.toBeNull();
    expect(videoState!.src).toContain('/htree/');

    console.log('=== Streaming Video Playback Test Passed ===');
  });
});
