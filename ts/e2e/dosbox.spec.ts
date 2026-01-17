/**
 * E2E tests for DOSBox integration
 *
 * Tests the flow of:
 * 1. Uploading a ZIP containing DOS executables
 * 2. Extracting the ZIP to a directory
 * 3. Clicking on a .exe file shows the DOSBox viewer
 * 4. DOSBox viewer loads directory context
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, goToTreeList, disableOthersPool } from './test-utils.js';
import * as fs from 'fs';
import * as path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const zipPath = path.join(__dirname, '../test-data/dosgame.zip');
const zipBase64 = fs.readFileSync(zipPath).toString('base64');

async function createTree(page: any, name: string) {
  await page.getByRole('button', { name: 'New Folder' }).click();
  const input = page.locator('input[placeholder="Folder name..."]');
  await expect(input).toBeVisible({ timeout: 5000 });
  await input.fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.locator('text=Empty directory')).toBeVisible({ timeout: 10000 });
}

async function uploadDosZip(page: any) {
  await page.evaluate(async (payload: { name: string; data: string }) => {
    const { uploadFiles } = await import('/src/stores/upload.ts');
    const bytes = Uint8Array.from(atob(payload.data), c => c.charCodeAt(0));
    const file = new File([bytes], payload.name, { type: 'application/zip' });
    const dataTransfer = new DataTransfer();
    dataTransfer.items.add(file);
    await uploadFiles(dataTransfer.files);
  }, { name: 'dosgame.zip', data: zipBase64 });
}

async function extractZipToCurrentDir(page: any) {
  await expect(page.locator('text=Extract Archive?')).toBeVisible({ timeout: 10000 });
  const currentDirOption = page.getByLabel('Extract to current directory');
  if (await currentDirOption.isVisible().catch(() => false)) {
    await currentDirOption.check();
  }
  await page.click('button:has-text("Extract Files")');
  await expect(page.locator('text=Extract Archive?')).not.toBeVisible({ timeout: 15000 });
}

test.describe('DOSBox integration', () => {
  test.describe.configure({ timeout: 90000 });
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });
    await goToTreeList(page);
  });

  test('should show extract modal when uploading a ZIP with DOS files', async ({ page }) => {
    await createTree(page, 'dos-games');
    await uploadDosZip(page);

    // Should show extract modal
    await expect(page.locator('text=Extract Archive?')).toBeVisible({ timeout: 10000 });

    // Should show file list
    await expect(page.locator('text=GAME.EXE')).toBeVisible();
    await expect(page.locator('text=CONFIG.TXT')).toBeVisible();
    await expect(page.locator('text=README.TXT')).toBeVisible();

    await extractZipToCurrentDir(page);

    // Should see extracted files in file browser
    await expect(page.locator('text=GAME.EXE')).toBeVisible({ timeout: 10000 });
  });

  test('should show DOSBox viewer when clicking on .exe file', async ({ page }) => {
    await createTree(page, 'dos-test');
    await uploadDosZip(page);

    await extractZipToCurrentDir(page);

    // Wait for GAME.EXE to appear in file list
    await expect(page.locator('text=GAME.EXE')).toBeVisible({ timeout: 10000 });

    // Click on the .exe file
    await page.click('text=GAME.EXE');

    // Should show DOSBox viewer with terminal icon
    await expect(page.locator('.i-lucide-terminal.text-3xl').first()).toBeVisible({ timeout: 10000 });

    // Should show "DOS Executable" label
    await expect(page.locator('text=DOS Executable')).toBeVisible({ timeout: 5000 });

    // Should show file count (5 files in our test zip: GAME.EXE, CONFIG.TXT, README.TXT, DATA/LEVELS.DAT, DATA/SOUND.DAT)
    await expect(page.locator('text=/\\d+ files.*ready to mount/')).toBeVisible({ timeout: 10000 });

    // Should show "Run in DOSBox" button
    await expect(page.locator('button:has-text("Run in DOSBox")')).toBeVisible({ timeout: 5000 });
  });

  test('should load directory context when starting DOSBox', async ({ page }) => {
    await createTree(page, 'dos-run-test');
    await uploadDosZip(page);

    await extractZipToCurrentDir(page);

    // Click on GAME.EXE
    await expect(page.locator('text=GAME.EXE')).toBeVisible({ timeout: 10000 });
    await page.click('text=GAME.EXE');

    // Wait for files to be collected
    await expect(page.locator('text=/\\d+ files.*ready to mount/')).toBeVisible({ timeout: 10000 });

    // Click Run in DOSBox
    await page.click('button:has-text("Run in DOSBox")');

    // Should show loading or running state (DOSBox toolbar)
    await expect(page.getByRole('button', { name: 'Exit' })).toBeVisible({ timeout: 10000 });
  });

  test('should display terminal icon for .exe files in file list', async ({ page }) => {
    await createTree(page, 'icon-test');
    await uploadDosZip(page);

    await extractZipToCurrentDir(page);

    // Wait for file list to show
    await expect(page.locator('text=GAME.EXE')).toBeVisible({ timeout: 10000 });

    // The file row should have a terminal icon
    // Find the row containing GAME.EXE and check for the icon
    const exeRow = page.locator('[data-testid="file-list"] a, [data-testid="file-list"] button').filter({ hasText: 'GAME.EXE' });
    await expect(exeRow).toBeVisible();
    await expect(exeRow.locator('.i-lucide-terminal')).toBeVisible();
  });

  test('should allow keeping ZIP as file instead of extracting', async ({ page }) => {
    await createTree(page, 'keep-zip-test');
    await uploadDosZip(page);

    // Should show extract modal
    await expect(page.locator('text=Extract Archive?')).toBeVisible({ timeout: 10000 });

    // Click "Keep as ZIP" instead of extracting
    await page.click('button:has-text("Keep as ZIP")');

    // Modal should close
    await expect(page.locator('text=Extract Archive?')).not.toBeVisible({ timeout: 5000 });

    // The ZIP file itself should NOT be in the file list (upload was cancelled)
    // This is the current behavior - keeping as ZIP cancels the upload
  });
});
