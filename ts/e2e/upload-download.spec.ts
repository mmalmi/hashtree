/**
 * E2E test for upload/download integrity
 * Verifies that downloaded file matches uploaded file exactly
 */
import { test, expect, Page } from './fixtures';
import * as path from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, navigateToPublicFolder } from './test-utils.js';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const TEST_VIDEO = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');

async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);
  await page.goto('/');
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
  await page.waitForSelector('header:has-text("Iris")', { timeout: 10000 });
  await navigateToPublicFolder(page);
}

function getFileHash(filePath: string): string {
  const content = fs.readFileSync(filePath);
  return crypto.createHash('sha256').update(content).digest('hex');
}

test.describe('Upload Download Integrity', () => {
  test.setTimeout(120000);

  test('uploaded video plays correctly and has proper duration', async ({ page }) => {
    const originalSize = fs.statSync(TEST_VIDEO).size;
    console.log('Original file size:', originalSize);

    await setupFreshUser(page);

    // Upload video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Wait for file to appear with correct size
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 60000 });

    // Check displayed size is correct (~968 KB)
    const sizeText = await page.locator('[data-testid="file-list"]').textContent();
    console.log('File list shows:', sizeText);
    expect(sizeText).toContain('967'); // Should show ~967.8 KB

    // Click to view
    await videoLink.click();
    await page.waitForTimeout(2000);

    // Wait for video element
    const videoElement = page.locator('video');
    await expect(videoElement).toBeVisible({ timeout: 10000 });

    // Wait for video to load metadata
    await page.waitForFunction(() => {
      const video = document.querySelector('video');
      return video && video.readyState >= 1 && video.duration > 0;
    }, { timeout: 15000 });

    // Check video properties
    const videoProps = await page.evaluate(() => {
      const video = document.querySelector('video');
      if (!video) return null;
      return {
        duration: video.duration,
        videoWidth: video.videoWidth,
        videoHeight: video.videoHeight,
        readyState: video.readyState,
        src: video.src?.substring(0, 50),
        error: video.error?.message
      };
    });

    console.log('Video properties:', videoProps);

    // Video should have correct duration (~10 seconds)
    expect(videoProps).not.toBeNull();
    expect(videoProps!.duration).toBeGreaterThan(9);
    expect(videoProps!.duration).toBeLessThan(11);
    expect(videoProps!.videoWidth).toBe(640);
    expect(videoProps!.videoHeight).toBe(360);
  });

  test('downloaded file should match uploaded file exactly', async ({ page }) => {
    // Get original file info
    const originalSize = fs.statSync(TEST_VIDEO).size;
    const originalHash = getFileHash(TEST_VIDEO);
    console.log('Original file size:', originalSize);
    console.log('Original file hash:', originalHash);

    // Capture console for debugging
    page.on('console', msg => {
      const text = msg.text();
      if (text.includes('upload') || text.includes('Upload') ||
          text.includes('chunk') || text.includes('error') || text.includes('Error')) {
        console.log('[Console]', text);
      }
    });

    await setupFreshUser(page);

    // Upload video
    console.log('Uploading file...');
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Wait for upload to complete - watch for the file to appear
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 60000 });
    console.log('File appeared in list');

    // Wait a bit for upload to fully complete
    await page.waitForTimeout(2000);

    // Check the displayed file size
    const fileSizeText = await page.locator('[data-testid="file-list"]').textContent();
    console.log('File list content:', fileSizeText);

    // Click to view the file
    await videoLink.click();
    await page.waitForTimeout(1000);

    // Disable File System Access API so we get a traditional download event
    // (showSaveFilePicker opens a native dialog that Playwright can't interact with)
    await page.evaluate(() => {
      delete (window as any).showSaveFilePicker;
    });

    // Set up download listener
    const downloadPromise = page.waitForEvent('download', { timeout: 30000 });

    // Click download button
    const downloadBtn = page.getByRole('button', { name: 'Download' });
    await expect(downloadBtn).toBeVisible({ timeout: 10000 });
    console.log('Download button found, clicking...');
    await downloadBtn.click();

    // Wait for download
    const download = await downloadPromise;
    console.log('Download started:', download.suggestedFilename());

    // Save to temp file
    const downloadPath = path.join(__dirname, 'fixtures', 'downloaded-test.mp4');
    await download.saveAs(downloadPath);

    // Verify downloaded file
    const downloadedSize = fs.statSync(downloadPath).size;
    const downloadedHash = getFileHash(downloadPath);
    console.log('Downloaded file size:', downloadedSize);
    console.log('Downloaded file hash:', downloadedHash);

    // Clean up
    fs.unlinkSync(downloadPath);

    // Assert
    expect(downloadedSize).toBe(originalSize);
    expect(downloadedHash).toBe(originalHash);
  });

  test('check what is actually stored after upload', async ({ page }) => {
    const originalSize = fs.statSync(TEST_VIDEO).size;
    console.log('Original file size:', originalSize);

    await setupFreshUser(page);

    // Upload video
    const fileInput = page.locator('input[type="file"][multiple]').first();
    await fileInput.setInputFiles(TEST_VIDEO);

    // Wait for file to appear
    const videoLink = page.locator('[data-testid="file-list"] a').filter({ hasText: 'Big_Buck_Bunny_360_10s_1MB.mp4' }).first();
    await expect(videoLink).toBeVisible({ timeout: 60000 });
    await page.waitForTimeout(2000);

    // Check IndexedDB storage stats (we use IndexedDB, not OPFS)
    const storageInfo = await page.evaluate(async () => {
      let idbSize = 0;
      let idbCount = 0;
      try {
        const dbs = await indexedDB.databases();
        for (const dbInfo of dbs) {
          if (dbInfo.name?.includes('hashtree')) {
            const db = await new Promise<IDBDatabase>((resolve, reject) => {
              const req = indexedDB.open(dbInfo.name!);
              req.onsuccess = () => resolve(req.result);
              req.onerror = () => reject(req.error);
            });
            for (const storeName of db.objectStoreNames) {
              const tx = db.transaction(storeName, 'readonly');
              const store = tx.objectStore(storeName);
              const countReq = store.count();
              await new Promise(resolve => { countReq.onsuccess = resolve; });
              idbCount += countReq.result;

              // Try to estimate size by iterating entries
              const cursor = store.openCursor();
              await new Promise<void>((resolve) => {
                cursor.onsuccess = () => {
                  const result = cursor.result;
                  if (result) {
                    const value = result.value;
                    if (value instanceof ArrayBuffer) {
                      idbSize += value.byteLength;
                    } else if (value instanceof Uint8Array) {
                      idbSize += value.byteLength;
                    } else if (value?.data instanceof Uint8Array) {
                      idbSize += value.data.byteLength;
                    }
                    result.continue();
                  } else {
                    resolve();
                  }
                };
                cursor.onerror = () => resolve();
              });
            }
            db.close();
          }
        }
      } catch (e) {
        console.log('IDB error:', e);
      }

      return { idbSize, idbCount };
    });

    console.log('Storage info:', storageInfo);
    console.log('IDB total size:', storageInfo.idbSize);
    console.log('IDB entry count:', storageInfo.idbCount);

    // IndexedDB should have entries (chunks of the file)
    expect(storageInfo.idbCount).toBeGreaterThan(0);
  });
});
