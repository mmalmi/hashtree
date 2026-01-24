/**
 * Cross-user livestream test
 *
 * Tests real cross-user streaming where broadcaster and viewer are in
 * separate browser contexts with separate storage.
 *
 * This test will expose where the data flow breaks:
 * 1. Tree root propagation via Nostr
 * 2. Chunk transfer via WebRTC or Blossom
 */
import { test, expect, type Page, type BrowserContext } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, followUser, waitForAppReady, ensureLoggedIn, useLocalRelay, enableOthersPool, waitForRelayConnected, navigateToPublicFolder, configureBlossomServers, disableOthersPool, waitForFollowInWorker, safeGoto, clearAllStorage } from './test-utils';
// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s.webm');

// Get video as base64 for mock MediaRecorder
function getTestVideoBase64(): string {
  const buffer = fs.readFileSync(TEST_VIDEO);
  return buffer.toString('base64');
}

type SetupOptions = {
  enableOthersPool?: boolean;
  othersPoolMax?: number;
};

// Setup fresh user with cleared storage
async function setupFreshUser(page: Page, options: SetupOptions = {}): Promise<void> {
  await safeGoto(page, '/', { retries: 4, delayMs: 1500 });
  await clearAllStorage(page);
  await safeGoto(page, '/', { retries: 4, delayMs: 1500 });
  await waitForAppReady(page);
  await ensureLoggedIn(page);
  await disableOthersPool(page);
  await useLocalRelay(page);
  if (options.enableOthersPool) {
    await enableOthersPool(page, options.othersPoolMax ?? 2);
  }
  await waitForRelayConnected(page);
}

// Get user's npub from public folder URL
async function getNpub(page: Page): Promise<string> {
  await navigateToPublicFolder(page);

  const url = page.url();
  const match = url.match(/npub1[a-z0-9]+/);
  if (!match) throw new Error('Could not find npub in URL');
  return match[0];
}

// Inject mock MediaRecorder and getUserMedia for headless testing
async function injectMockMediaRecorder(page: Page, videoBase64: string): Promise<void> {
  await page.evaluate((b64) => {
    const videoBytes = Uint8Array.from(atob(b64), c => c.charCodeAt(0));
    const CHUNK_SIZE = 100 * 1024; // 100KB chunks
    const chunks: Uint8Array[] = [];
    for (let i = 0; i < videoBytes.length; i += CHUNK_SIZE) {
      chunks.push(videoBytes.slice(i, i + CHUNK_SIZE));
    }
    console.log(`[MockRecorder] Split video into ${chunks.length} chunks`);
    (window as any).__mockChunksTotal = chunks.length;
    (window as any).__mockChunksDelivered = 0;

    // Create fake MediaStream using canvas
    const canvas = document.createElement('canvas');
    canvas.width = 320;
    canvas.height = 240;
    const ctx = canvas.getContext('2d');
    if (ctx) {
      ctx.fillStyle = '#333';
      ctx.fillRect(0, 0, 320, 240);
    }
    const fakeStream = canvas.captureStream(30);

    // Mock getUserMedia
    navigator.mediaDevices.getUserMedia = async () => fakeStream;

    class MockMediaRecorder {
      stream: MediaStream;
      state: string = 'inactive';
      ondataavailable: ((e: { data: Blob }) => void) | null = null;
      onstop: (() => void) | null = null;
      private intervalId: number | null = null;
      private chunkIndex = 0;

      constructor(stream: MediaStream, _options?: MediaRecorderOptions) {
        this.stream = stream;
      }

      start(timeslice?: number) {
        this.state = 'recording';
        this.chunkIndex = 0;
        const interval = timeslice || 1000;

        const feedChunk = () => {
          if (this.chunkIndex < chunks.length && this.ondataavailable) {
            console.log(`[MockRecorder] Feeding chunk ${this.chunkIndex + 1}/${chunks.length}`);
            this.ondataavailable({ data: new Blob([chunks[this.chunkIndex]]) });
            this.chunkIndex++;
            (window as any).__mockChunksDelivered = this.chunkIndex;
          } else if (this.chunkIndex >= chunks.length) {
            console.log('[MockRecorder] All chunks fed');
          }
        };

        feedChunk();
        this.intervalId = window.setInterval(feedChunk, interval);
      }

      stop() {
        if (this.intervalId) {
          clearInterval(this.intervalId);
          this.intervalId = null;
        }
        this.state = 'inactive';
        if (this.onstop) this.onstop();
      }

      static isTypeSupported() { return true; }
    }

    (window as any).MediaRecorder = MockMediaRecorder;
    console.log('[MockRecorder] Mocked MediaRecorder and getUserMedia');
  }, videoBase64);
}

async function waitForPeerConnected(page: Page, timeoutMs: number = 30000): Promise<void> {
  await page.waitForFunction(() => {
    const getStore = (window as any).__getWebRTCStore;
    const store = getStore?.() || (window as any).webrtcStore;
    const peers = store?.getPeers?.() || [];
    return peers.some((p: any) => p?.isConnected || p?.connected);
  }, undefined, { timeout: timeoutMs });
}

async function waitForFileEntry(page: Page, filename: string, timeoutMs: number = 30000): Promise<boolean> {
  try {
    await expect.poll(
      async () => {
        return page.evaluate(async (name) => {
          try {
            const { getTree } = await import('/src/store.ts');
            const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
            const { getRouteSync } = await import('/src/stores/index.ts');
            const route = getRouteSync();
            const rootCid = getTreeRootSync(route.npub, route.treeName);
            if (!rootCid) return false;
            const tree = getTree();
            const basePath = route.path ?? [];
            const targetPath = basePath[basePath.length - 1] === name
              ? basePath
              : basePath.concat([name]);
            await tree.resolvePath(rootCid, targetPath);
            return true;
          } catch {
            return false;
          }
        }, filename);
      },
      { timeout: timeoutMs, intervals: [500, 1000, 2000] }
    ).toBe(true);
    return true;
  } catch {
    return false;
  }
}

test.describe('Cross-User Livestream', () => {
  // Skip: flaky multi-context WebRTC streaming test with timing-dependent peer discovery
  test('viewer can fetch stream data from broadcaster', async ({ browser }) => {
    test.slow();
    test.setTimeout(300000);

    expect(fs.existsSync(TEST_VIDEO)).toBe(true);
    const videoBase64 = getTestVideoBase64();

    // Create two completely separate browser contexts
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();
    const pageA = await contextA.newPage(); // Broadcaster
    const pageB = await contextB.newPage(); // Viewer

    // Detailed logging
    const logs = {
      broadcaster: [] as string[],
      viewer: [] as string[],
    };

    pageA.on('console', msg => {
      const text = msg.text();
      logs.broadcaster.push(text);
      if (text.includes('[MockRecorder]') || text.includes('[Stream]') ||
          text.includes('WebRTC') || text.includes('Blossom') || text.includes('peer')) {
        console.log(`[A] ${text}`);
      }
    });

    pageB.on('console', msg => {
      const text = msg.text();
      logs.viewer.push(text);
      if (text.includes('SwFileHandler') || text.includes('tree') ||
          text.includes('WebRTC') || text.includes('Blossom') || text.includes('peer') ||
          text.includes('404') || text.includes('error')) {
        console.log(`[B] ${text}`);
      }
    });

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // === Setup Broadcaster ===
      console.log('\n=== Setting up Broadcaster ===');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      console.log(`Broadcaster: ${npubA.slice(0, 20)}...`);
      const pubkeyA = await pageA.evaluate(() => (window as any).__nostrStore?.getState?.().pubkey || null);
      await injectMockMediaRecorder(pageA, videoBase64);
      await configureBlossomServers(pageA);

      // === Setup Viewer ===
      console.log('\n=== Setting up Viewer ===');
      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      console.log(`Viewer: ${npubB.slice(0, 20)}...`);
      const pubkeyB = await pageB.evaluate(() => (window as any).__nostrStore?.getState?.().pubkey || null);
      await configureBlossomServers(pageB);

      // === Mutual follows for WebRTC ===
      console.log('\n=== Setting up mutual follows ===');
      await followUser(pageA, npubB);
      await followUser(pageB, npubA);
      if (pubkeyA && pubkeyB) {
        await waitForFollowInWorker(pageA, pubkeyB, 30000);
        await waitForFollowInWorker(pageB, pubkeyA, 30000);
      }
      console.log('Mutual follows established');

      // === Check WebRTC connections ===
      console.log('\n=== Waiting for WebRTC connections ===');
      await Promise.all([
        pageA.evaluate(() => (window as any).__getWorkerAdapter?.()?.sendHello?.()),
        pageB.evaluate(() => (window as any).__getWorkerAdapter?.()?.sendHello?.()),
      ]);
      try {
        await Promise.all([
          waitForPeerConnected(pageA, 60000),
          waitForPeerConnected(pageB, 60000),
        ]);
      } catch (err) {
        console.log('WebRTC peers not connected yet, continuing:', err instanceof Error ? err.message : err);
      }

      const peersA = await pageA.evaluate(() => {
        const store = (window as any).webrtcStore;
        return store?.getPeers?.()?.map((p: any) => ({
          pubkey: p.pubkey?.slice(0, 16),
          isConnected: p.isConnected,
          pool: p.pool,
        })) || [];
      });
      console.log('Broadcaster peers:', JSON.stringify(peersA, null, 2));

      const peersB = await pageB.evaluate(() => {
        const store = (window as any).webrtcStore;
        return store?.getPeers?.()?.map((p: any) => ({
          pubkey: p.pubkey?.slice(0, 16),
          isConnected: p.isConnected,
          pool: p.pool,
        })) || [];
      });
      console.log('Viewer peers:', JSON.stringify(peersB, null, 2));

      // === Start streaming ===
      console.log('\n=== Starting stream ===');
      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await pageA.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });

      const streamLink = pageA.getByRole('link', { name: 'Stream' });
      await expect(streamLink).toBeVisible({ timeout: 10000 });
      await streamLink.click();

      const startCameraBtn = pageA.getByRole('button', { name: 'Start Camera' });
      await expect(startCameraBtn).toBeVisible({ timeout: 10000 });
      await startCameraBtn.click();

      const testFilename = `test_stream_${Date.now()}`;
      const filenameInput = pageA.locator('input[placeholder="filename"]');
      await expect(filenameInput).toBeVisible({ timeout: 10000 });
      await filenameInput.fill(testFilename);

      const startRecordingBtn = pageA.getByRole('button', { name: /Start Recording/ });
      await expect(startRecordingBtn).toBeVisible({ timeout: 10000 });
      await startRecordingBtn.click();

      // Wait for some chunks to be recorded and published
      console.log('Waiting for chunks to be recorded and published...');
      await pageA.waitForFunction(
        () => (window as any).__mockChunksDelivered >= 2,
        { timeout: 30000 }
      );

      const broadcasterHasEntry = await waitForFileEntry(pageA, `${testFilename}.webm`, 30000);
      console.log('Broadcaster entry visible:', broadcasterHasEntry);

      // Check broadcaster's tree root
      const broadcasterRoot = await pageA.evaluate((filename) => {
        const { getTreeRootSync } = (window as any);
        // Try to find the tree root for this user
        const url = window.location.hash;
        const npubMatch = url.match(/npub1[a-z0-9]+/);
        if (!npubMatch) return null;

        // Can't easily get this without more work
        return { hasData: true };
      }, testFilename);
      console.log('Broadcaster tree root:', broadcasterRoot);

      // === Viewer navigates to stream ===
      console.log('\n=== Viewer navigating to stream ===');
      const streamUrl = `http://localhost:5173/#/${npubA}/public/${testFilename}.webm?live=1`;
      console.log(`Stream URL: ${streamUrl}`);
      await pageB.goto(streamUrl);

      // Wait for page to try loading
      const viewerHasEntry = await waitForFileEntry(pageB, `${testFilename}.webm`, 90000);
      console.log('Viewer entry visible:', viewerHasEntry);
      if (!viewerHasEntry) {
        console.log('Viewer entry not visible yet, continuing to check video state');
      }
      await pageB.waitForSelector('video', { timeout: 120000 });
      let viewerHasData = false;
      let viewerReadyState = 0;
      await expect.poll(async () => {
        const result = await pageB.evaluate(async () => {
          const video = document.querySelector('video') as HTMLVideoElement | null;
          try {
            const { getTree } = await import('/src/store.ts');
            const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
            const { getRouteSync } = await import('/src/stores/index.ts');
            const adapter = (window as any).__workerAdapter;
            adapter?.sendHello?.();
            const route = getRouteSync();
            if (!route) {
              return { hasData: false, readyState: video?.readyState ?? 0 };
            }
            const rootCid = getTreeRootSync(route.npub, route.treeName);
            if (!rootCid) {
              return { hasData: false, readyState: video?.readyState ?? 0 };
            }
            const tree = getTree();
            const resolved = await tree.resolvePath(rootCid, route.path);
            if (!resolved?.cid) {
              return { hasData: false, readyState: video?.readyState ?? 0 };
            }
            const readChunk = () => {
              if (typeof (tree as any).readFileRange === 'function') {
                return (tree as any).readFileRange(resolved.cid, 0, 2048);
              }
              return tree.readFile(resolved.cid);
            };
            const data = await Promise.race([
              readChunk(),
              new Promise<Uint8Array | null>((resolve) => {
                setTimeout(() => resolve(null), 5000);
              }),
            ]);
            return {
              hasData: !!data && data.length > 0,
              readyState: video?.readyState ?? 0,
            };
          } catch {
            return { hasData: false, readyState: video?.readyState ?? 0 };
          }
        });
        viewerHasData = result.hasData;
        viewerReadyState = result.readyState;
        return viewerHasData || viewerReadyState > 0;
      }, { timeout: 120000, intervals: [1000, 2000, 5000] }).toBe(true);

      // Check what happened
      const viewerState = await pageB.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement;
        const loading = document.querySelector('.animate-spin');
        const errorEl = document.querySelector('.text-red-400');

        return {
          hasVideo: !!video,
          videoSrc: video?.src || null,
          videoReadyState: video?.readyState || null,
          videoDuration: video?.duration || null,
          videoError: video?.error?.message || null,
          isLoading: !!loading,
          errorText: errorEl?.textContent || null,
          bodyText: document.body.innerText.slice(0, 500),
        };
      });

      console.log('\n=== Viewer State ===');
      console.log(JSON.stringify(viewerState, null, 2));

      // Check for specific errors in viewer logs
      const viewerErrors = logs.viewer.filter(l =>
        l.includes('error') || l.includes('Error') || l.includes('404') || l.includes('failed')
      );
      if (viewerErrors.length > 0) {
        console.log('\n=== Viewer Errors ===');
        viewerErrors.slice(0, 20).forEach(e => console.log(e));
      }

      // Stop recording
      const stopBtn = pageA.getByRole('button', { name: /Stop Recording/ });
      if (await stopBtn.isVisible({ timeout: 1000 }).catch(() => false)) {
        await stopBtn.click();
      }

      // === Assertions ===
      // The viewer should have a video element
      expect(viewerState.hasVideo).toBe(true);

      // The video should NOT be in loading state forever
      // If isLoading is true and we have no video data, the data flow is broken
      if (viewerState.isLoading && viewerState.videoReadyState === 0) {
        console.log('\n=== FAILURE: Viewer stuck in loading state ===');
        console.log('This means chunks are not being transferred from broadcaster to viewer.');
        console.log('Possible causes:');
        console.log('1. WebRTC connection not established');
        console.log('2. Chunks not available via Blossom');
        console.log('3. Tree root not propagated via Nostr');
      }

      // We expect the video to have loaded something (readyState > 0)
      expect(viewerState.videoReadyState > 0 || viewerHasData).toBe(true);

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('streaming fails without mutual follows (no WebRTC, no Blossom)', async ({ browser }) => {
    /**
     * This test verifies what happens when:
     * - Users do NOT follow each other (no WebRTC connection)
     * - Chunks are NOT on Blossom
     *
     * Expected: Viewer gets stuck on loading because chunks are only in
     * broadcaster's local storage and there's no way to fetch them.
     */
    test.slow();
    test.setTimeout(120000);

    expect(fs.existsSync(TEST_VIDEO)).toBe(true);
    const videoBase64 = getTestVideoBase64();

    const contextA = await browser.newContext();
    const contextB = await browser.newContext();
    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // Setup broadcaster
      console.log('\n=== Setting up Broadcaster (NO mutual follows) ===');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      console.log(`Broadcaster: ${npubA.slice(0, 20)}...`);
      await injectMockMediaRecorder(pageA, videoBase64);

      // Setup viewer - but DON'T follow broadcaster
      console.log('\n=== Setting up Viewer (NO mutual follows) ===');
      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      console.log(`Viewer: ${npubB.slice(0, 20)}...`);

      // NO mutual follows - WebRTC won't connect

      // Start streaming
      console.log('\n=== Starting stream ===');
      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await pageA.waitForURL(/\/#\/npub.*\/public/, { timeout: 10000 });

      const streamLink = pageA.getByRole('link', { name: 'Stream' });
      await expect(streamLink).toBeVisible({ timeout: 10000 });
      await streamLink.click();

      const startCameraBtn = pageA.getByRole('button', { name: 'Start Camera' });
      await expect(startCameraBtn).toBeVisible({ timeout: 10000 });
      await startCameraBtn.click();

      const testFilename = `no_follow_test_${Date.now()}`;
      const filenameInput = pageA.locator('input[placeholder="filename"]');
      await expect(filenameInput).toBeVisible({ timeout: 10000 });
      await filenameInput.fill(testFilename);

      const startRecordingBtn = pageA.getByRole('button', { name: /Start Recording/ });
      await startRecordingBtn.click();

      // Wait for some chunks to be recorded and published
      await pageA.waitForFunction(
        () => (window as any).__mockChunksDelivered >= 2,
        { timeout: 15000 }
      );

      const broadcasterHasEntry = await waitForFileEntry(pageA, `${testFilename}.webm`, 20000);
      console.log('Broadcaster entry visible:', broadcasterHasEntry);

      // Viewer navigates to stream
      console.log('\n=== Viewer navigating to stream (without WebRTC connection) ===');
      const streamUrl = `http://localhost:5173/#/${npubA}/public/${testFilename}.webm?live=1`;
      await pageB.goto(streamUrl);

      // Wait for tree root + file entry to resolve before checking playback state
      const viewerHasEntry = await waitForFileEntry(pageB, `${testFilename}.webm`, 30000);
      console.log('Viewer entry visible:', viewerHasEntry);
      expect(viewerHasEntry).toBe(true);

      const viewerState = await pageB.evaluate(() => {
        const video = document.querySelector('video') as HTMLVideoElement;
        const loading = document.querySelector('.animate-spin');
        const errorEl = document.querySelector('.text-red-400');
        const buffered = video && video.buffered.length > 0 ? video.buffered.end(0) : 0;

        return {
          hasVideo: !!video,
          videoReadyState: video?.readyState || 0,
          videoDuration: video?.duration || 0,
          videoBuffered: buffered,
          isLoading: !!loading,
          errorText: errorEl?.textContent || null,
        };
      });

      console.log('\n=== Viewer State (NO WebRTC) ===');
      console.log(JSON.stringify(viewerState, null, 2));

      // Stop recording
      const stopBtn = pageA.getByRole('button', { name: /Stop Recording/ });
      if (await stopBtn.isVisible({ timeout: 1000 }).catch(() => false)) {
        await stopBtn.click();
      }

      // Without WebRTC connection and without Blossom, the viewer should:
      // - Have a video element
      // - But no data loaded (readyState 0 or low, duration 0 or NaN)
      // - Possibly stuck loading or showing an error

      if (!viewerState.hasVideo) {
        console.log('EXPECTED: No video element without follows/Blossom');
        return;
      }

      const hasLoadedData =
        viewerState.videoReadyState > 1 ||
        viewerState.videoDuration > 0 ||
        viewerState.videoBuffered > 0.1;

      if (hasLoadedData) {
        console.log('UNEXPECTED: Viewer got data without follows/Blossom');
      } else {
        console.log('EXPECTED: Viewer stuck without data - no WebRTC connection to broadcaster');
        console.log('This is the gray window bug scenario.');
        console.log('Fix: Either require mutual follows, or auto-upload to Blossom during streaming.');
      }

      expect(hasLoadedData).toBe(false);

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });
});
