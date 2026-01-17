/**
 * E2E test to verify Blossom servers are not unnecessarily queried
 * when data exists locally in IndexedDB.
 *
 * This tests for the bug where loading a local file triggers thousands
 * of HTTP requests to Blossom servers despite data being available locally.
 */
import { test, expect, Page, Request } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, navigateToPublicFolder } from './test-utils.js';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');

// Blossom server domains to monitor
const BLOSSOM_DOMAINS = [
  'cdn.iris.to',
  'blossom.iris.to',
];

interface BlossomRequest {
  url: string;
  method: string;
  timestamp: number;
}

async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);
  await page.goto('/');
  await page.evaluate(async () => {
    // Clear IndexedDB (settings, etc)
    const dbs = await indexedDB.databases();
    for (const db of dbs) {
      if (db.name) indexedDB.deleteDatabase(db.name);
    }
    localStorage.clear();
    sessionStorage.clear();

    // Clear OPFS storage
    try {
      const root = await navigator.storage.getDirectory();
      // @ts-ignore
      for await (const [name] of root.entries()) {
        await root.removeEntry(name, { recursive: true });
      }
    } catch {
      // OPFS might not be available
    }
  });
  await page.reload();
  await page.waitForTimeout(500);
  // Page ready - navigateToPublicFolder handles waiting
  await navigateToPublicFolder(page);
}

function isBlossomRequest(url: string): boolean {
  return BLOSSOM_DOMAINS.some(domain => url.includes(domain));
}

test.describe('Blossom Fallback Behavior', () => {
  test.setTimeout(120000);

  test('should NOT make Blossom GET requests when loading locally stored file', async ({ page }) => {
    // Track all Blossom requests
    const blossomRequests: BlossomRequest[] = [];

    // Intercept network requests
    page.on('request', (request: Request) => {
      const url = request.url();
      if (isBlossomRequest(url)) {
        blossomRequests.push({
          url,
          method: request.method(),
          timestamp: Date.now(),
        });
        console.log(`[BLOSSOM ${request.method()}] ${url}`);
      }
    });

    // Start fresh
    await setupFreshUser(page);
    console.log('Fresh user setup complete');

    // Upload the video file
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);
    console.log('File input set');

    // Wait for upload to complete - file should appear in list
    const videoLink = page.locator('[data-testid="file-list"] a')
      .filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' })
      .first();
    await expect(videoLink).toBeVisible({ timeout: 60000 });
    console.log('File appeared in list');

    // Wait a bit for upload to fully complete and any background sync
    await page.waitForTimeout(3000);

    // Count Blossom requests during upload phase
    const uploadPhaseRequests = blossomRequests.length;
    console.log(`Blossom requests during upload: ${uploadPhaseRequests}`);

    // Clear the request log before viewing
    const viewStartIndex = blossomRequests.length;

    // Click to view the file (this triggers loading from storage)
    await videoLink.click();
    console.log('Clicked to view file');

    // Wait for video to load and become visible (not invisible)
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      // Video exists, has loaded metadata, and is not invisible
      return video &&
             video.readyState >= 1 &&
             video.duration > 0 &&
             !video.classList.contains('invisible');
    }, { timeout: 30000 });
    console.log('Video loaded and playable');

    // Give some time for any lazy loading / streaming to happen
    await page.waitForTimeout(3000);

    // Count Blossom requests during view phase
    const viewPhaseRequests = blossomRequests.slice(viewStartIndex);
    const getRequests = viewPhaseRequests.filter(r => r.method === 'GET');
    const putRequests = viewPhaseRequests.filter(r => r.method === 'PUT');
    const headRequests = viewPhaseRequests.filter(r => r.method === 'HEAD');

    console.log('\n=== Blossom Request Summary ===');
    console.log(`Total requests during view: ${viewPhaseRequests.length}`);
    console.log(`  GET requests: ${getRequests.length}`);
    console.log(`  PUT requests: ${putRequests.length}`);
    console.log(`  HEAD requests: ${headRequests.length}`);

    if (getRequests.length > 0) {
      console.log('\nGET request URLs (first 10):');
      getRequests.slice(0, 10).forEach(r => console.log(`  ${r.url}`));
      if (getRequests.length > 10) {
        console.log(`  ... and ${getRequests.length - 10} more`);
      }
    }

    // The assertion: NO GET requests to Blossom when viewing local file
    // PUT requests might be acceptable for background sync (fire-and-forget writes)
    expect(getRequests.length).toBe(0);
  });

  test('should have reasonable PUT count during upload (fire-and-forget sync)', async ({ page }) => {
    // Track Blossom requests during upload
    const blossomRequests: BlossomRequest[] = [];

    page.on('request', (request: Request) => {
      const url = request.url();
      if (isBlossomRequest(url)) {
        blossomRequests.push({
          url,
          method: request.method(),
          timestamp: Date.now(),
        });
      }
    });

    await setupFreshUser(page);

    // Upload
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    const videoLink = page.locator('[data-testid="file-list"] a')
      .filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' })
      .first();
    await expect(videoLink).toBeVisible({ timeout: 60000 });

    // Wait for fire-and-forget uploads to settle
    await page.waitForTimeout(5000);

    // Analyze upload phase
    const putRequests = blossomRequests.filter(r => r.method === 'PUT');
    const getRequests = blossomRequests.filter(r => r.method === 'GET');

    console.log('\n=== Upload Phase Analysis ===');
    console.log(`PUT requests: ${putRequests.length}`);
    console.log(`GET requests: ${getRequests.length}`);

    // 1MB file at 1KB chunks = ~1000 chunks
    // Each chunk goes to ~2 write-enabled servers = ~2000 PUT requests max
    // Plus some tree nodes
    // Should be well under 3000 for a 1MB file
    expect(putRequests.length).toBeLessThan(3000);

    // There should be NO GET requests during upload
    expect(getRequests.length).toBe(0);
  });
});
