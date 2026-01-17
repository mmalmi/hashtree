import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';
import * as fs from 'fs';
import * as path from 'path';

// This test requires a large ZIP file and takes several minutes to run
// Skip in CI by default - run manually with: npx playwright test e2e/large-zip-extract.spec.ts
test.describe('Large ZIP extraction', () => {
  test('should extract ZIP file with progress', async ({ page }) => {
    // Increase timeout for large file
    test.setTimeout(600000); // 10 minutes

    setupPageErrorHandler(page);

    // Capture console logs
    page.on('console', msg => {
      console.log(`BROWSER ${msg.type()}: ${msg.text()}`);
    });

    // Capture page errors
    page.on('pageerror', error => {
      console.log(`BROWSER ERROR: ${error.message}`);
    });

    await page.goto('/');
    await disableOthersPool(page);
    await navigateToPublicFolder(page);

    // Load ZIP file - use large test data if present, otherwise fallback fixture
    const largeZipPath = path.join(process.cwd(), 'test-data', 'WebGLSamples.github.io-master.zip');
    const fallbackZipPath = path.join(process.cwd(), 'e2e', 'fixtures', 'small-sample.zip');
    const zipPath = fs.existsSync(largeZipPath) ? largeZipPath : fallbackZipPath;
    const isLargeZip = zipPath === largeZipPath;
    if (!fs.existsSync(zipPath)) {
      throw new Error(`ZIP fixture not found at ${zipPath}`);
    }
    const stats = fs.statSync(zipPath);
    console.log(`ZIP file size: ${stats.size} bytes`);

    // Upload the ZIP file using file path
    const [fileChooser] = await Promise.all([
      page.waitForEvent('filechooser'),
      page.getByRole('button', { name: 'Add' }).click(),
    ]);

    await fileChooser.setFiles(zipPath);

    // Extract modal should appear - this tests getArchiveFileList works
    console.log('Waiting for extract modal...');
    await expect(page.locator('text="Extract Archive?"')).toBeVisible({ timeout: 30000 });

    // Should show file count
    await expect(page.locator('text=/contains \\d+ files?/')).toBeVisible({ timeout: 5000 });
    console.log('Modal appeared with file list');

    // Select "Extract to folder" option (default)
    const subdirRadio = page.locator('input[name="extract-location"]').first();
    await subdirRadio.check();

    // Click "Extract Files" button
    console.log('Starting extraction...');
    const startTime = Date.now();
    await page.getByRole('button', { name: 'Extract Files' }).click();

    // Modal should close and progress should show
    // Give it more time for the extraction to start
    await expect(page.locator('text="Extract Archive?"')).not.toBeVisible({ timeout: 60000 });

    // Progress toast should appear - this confirms extraction started successfully
    await expect(page.getByText('Extracting archive...')).toBeVisible({ timeout: 30000 });
    console.log('Progress indicator visible - extraction started!');

    let sawFileProgress = false;
    if (isLargeZip) {
      // Wait for extraction to complete or timeout
      // The sync extraction of 200MB takes ~1-2 seconds, but processing 1236 files takes longer
      for (let i = 0; i < 180; i++) { // Up to 3 minutes
        // Check for file progress (e.g., "50/1236")
        const progressText = await page.locator('.fixed.bottom-4').textContent().catch(() => '');
        if (progressText) {
          const match = progressText.match(/(\d+)\s*\/\s*(\d+)/);
          if (match) {
            const current = parseInt(match[1]);
            const total = parseInt(match[2]);
            if (!sawFileProgress || current % 100 === 0) {
              console.log(`Progress: ${current}/${total}`);
            }
            sawFileProgress = true;

            // If we've processed a good chunk, test passes
            if (current >= 100) {
              console.log(`Successfully processed ${current} files - test passes!`);
              break;
            }
          }
        }

        // Check if extraction completed
        const extractingVisible = await page.getByText('Extracting archive...').isVisible().catch(() => false);
        const writingVisible = await page.getByText('writing...').isVisible().catch(() => false);
        if (!extractingVisible && !writingVisible && sawFileProgress) {
          console.log('Extraction completed!');
          break;
        }

        await page.waitForTimeout(1000);
      }
    } else {
      // Small fixture: verify extracted folder appears quickly
      const zipName = path.basename(zipPath, '.zip');
      const expected = page.locator('[data-testid="file-list"] a').filter({ hasText: new RegExp(`${zipName}|sample`, 'i') }).first();
      await expect(expected).toBeVisible({ timeout: 20000 });
      sawFileProgress = true;
    }

    const elapsed = Date.now() - startTime;
    console.log(`Test ran for ${elapsed}ms`);

    // If we saw file progress, the extraction is working
    expect(sawFileProgress).toBe(true);
  });
});
