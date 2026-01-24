/**
 * E2E tests for Yjs document viewer
 *
 * Tests that directories with .yjs file are detected and rendered with Tiptap editor.
 * A Yjs document directory is identified by having a .yjs config file inside.
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, configureBlossomServers } from './test-utils.js';

async function waitForTreeRootChange(page: any, previousRoot: string | null, timeoutMs: number = 60000) {
  await page.waitForFunction(
    () => typeof (window as any).__getTreeRoot === 'function',
    undefined,
    { timeout: 10000 }
  );
  await page.waitForFunction(
    (prev) => {
      const current = (window as any).__getTreeRoot?.();
      return !!current && current !== prev;
    },
    previousRoot,
    { timeout: timeoutMs }
  );
}

test.describe('Yjs Document Viewer', () => {
  test.setTimeout(60000);

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);

    await page.goto('/');
    await disableOthersPool(page);
    await configureBlossomServers(page);

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
    await configureBlossomServers(page);
    // Page ready - navigateToPublicFolder handles waiting
    await navigateToPublicFolder(page);
  });

  test('New Document button creates folder with .yjs file', async ({ page }) => {
    // We're inside the public folder from navigateToPublicFolder

    // Click New Document button
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.waitForTimeout(500);

    // Fill in document name
    await page.locator('input[placeholder="Document name..."]').fill('notes');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(1500);

    // The folder should be visible (named "notes", not "notes.yjs")
    const docFolder = page.locator('a:has-text("notes")').first();
    await expect(docFolder).toBeVisible({ timeout: 30000 });

    // Click to navigate into the document folder
    await docFolder.click();
    await page.waitForTimeout(2000);

    // Verify .yjs file exists inside the folder
    const yjsFile = page.locator('a:has-text(".yjs")').first();
    await expect(yjsFile).toBeVisible({ timeout: 30000 });
  });

  test('non-document directory shows normal directory actions', async ({ page }) => {
    // We're inside the public folder from navigateToPublicFolder

    // Create a regular subfolder using New Folder button
    await page.getByRole('button', { name: 'New Folder' }).first().click();
    await page.waitForTimeout(500);
    await page.locator('input[placeholder="Folder name..."]').fill('regular-folder');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(1500);

    // Wait for the folder to appear
    const regularFolder = page.locator('a:has-text("regular-folder")');
    await expect(regularFolder).toBeVisible({ timeout: 30000 });

    // Navigate into the regular folder
    await regularFolder.click();
    await page.waitForTimeout(1000);

    // Should show normal directory view - look for the upload drop zone text
    const dropZone = page.locator('text=Drop or click to add');
    await expect(dropZone).toBeVisible({ timeout: 30000 });

    // Should NOT show the Tiptap ProseMirror editor
    const editor = page.locator('.ProseMirror');
    await expect(editor).not.toBeVisible();

    // Should have File/Folder/Document buttons visible
    await expect(page.getByRole('button', { name: 'New File' })).toBeVisible({ timeout: 30000 });
    await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 30000 });
    await expect(page.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });
  });

  test('folder with manually created .yjs file shows Tiptap editor', async ({ page }) => {
    // We're inside the public folder from navigateToPublicFolder

    // Create a regular folder first
    console.log('Creating folder...');
    await page.getByRole('button', { name: 'New Folder' }).first().click();
    await page.waitForTimeout(500);
    await page.locator('input[placeholder="Folder name..."]').fill('manual-doc');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(1500);

    // Navigate into the folder
    console.log('Looking for manual-doc folder...');
    const folder = page.locator('a:has-text("manual-doc")');
    const folderCount = await folder.count();
    console.log(`Found ${folderCount} elements matching manual-doc`);
    await expect(folder).toBeVisible({ timeout: 30000 });
    await folder.click();
    await page.waitForTimeout(1000);

    // Should show normal directory (no .yjs file yet)
    console.log('Checking for drop zone...');
    const dropZone = page.locator('text=Drop or click to add');
    await expect(dropZone).toBeVisible({ timeout: 30000 });

    // Create a .yjs file inside
    console.log('Creating .yjs file...');
    await page.getByRole('button', { name: 'New File' }).click();
    await page.waitForTimeout(500);
    await page.locator('input[placeholder="File name..."]').fill('.yjs');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(1500);

    // Go back to parent - wait for the link to appear
    console.log('Going back to parent...');
    const backLink = page.locator('a:has-text("..")');
    await expect(backLink).toBeVisible({ timeout: 30000 });
    await backLink.click();
    await page.waitForTimeout(1000);

    // Navigate back into the folder - wait for it to be visible first
    console.log('Navigating back to manual-doc...');
    const currentUrl = page.url();
    console.log(`Current URL: ${currentUrl}`);
    const allLinks = await page.locator('a').allTextContents();
    console.log(`All links on page: ${allLinks.join(', ')}`);
    const manualDocLink = page.locator('a:has-text("manual-doc")');
    const linkCount = await manualDocLink.count();
    console.log(`Found ${linkCount} manual-doc links`);
    await expect(manualDocLink).toBeVisible({ timeout: 30000 });
    await manualDocLink.click();
    await page.waitForTimeout(1500);

    // Should now show the Tiptap editor (detects .yjs file)
    const editor = page.locator('.ProseMirror, .prose');
    await expect(editor.first()).toBeVisible({ timeout: 30000 });
  });

  test('typing in document editor works and auto-saves', async ({ page }) => {
    // We're inside the public folder from navigateToPublicFolder

    // Click New Document button
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.waitForTimeout(500);

    // Fill in document name
    await page.locator('input[placeholder="Document name..."]').fill('editable-doc');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(2000);

    // Wait for editor to be visible (confirms document loaded)
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });
    await editor.click();

    // Type some text
    const rootBefore = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await page.keyboard.type('Hello, this is a test document!');

    // Verify text appears in editor
    await expect(editor).toContainText('Hello, this is a test document!');

    // Wait for auto-save to update the tree root
    await waitForTreeRootChange(page, rootBefore, 60000);

    // Verify a "deltas" folder was created for delta-based storage
    const deltasFolder = page.getByRole('link', { name: /^deltas$/ }).first();
    await expect(deltasFolder).toBeVisible({ timeout: 30000 });
  });

  test('clicking .yjs file to view it does not cause errors', async ({ page }) => {
    // This test verifies that viewing a .yjs file directly doesn't throw
    // "undefined is not iterable" errors in Viewer.tsx

    // Track console errors
    const consoleErrors: string[] = [];
    page.on('console', msg => {
      if (msg.type() === 'error') {
        consoleErrors.push(msg.text());
      }
    });
    page.on('pageerror', err => {
      consoleErrors.push(err.message);
    });

    // We're inside the public folder from navigateToPublicFolder

    // Create a document folder using New Document button
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.waitForTimeout(1000);

    await page.locator('input[placeholder="Document name..."]').fill('test-doc');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(3000);

    // After creating a document, we should automatically navigate into it
    // Verify the .yjs file is visible (we're inside the document folder)
    const yjsFile = page.locator('a:has-text(".yjs")').first();
    await expect(yjsFile).toBeVisible({ timeout: 30000 });

    // Click on the .yjs file to view it - this should not cause errors
    await yjsFile.click();
    await page.waitForTimeout(3000);

    // Verify we're viewing the file (header shows filename)
    // The file viewer should show the .yjs filename
    const fileHeader = page.locator('span:has-text(".yjs")');
    await expect(fileHeader.first()).toBeVisible({ timeout: 30000 });

    // Wait a bit more to ensure any async errors would have appeared
    await page.waitForTimeout(2000);

    // Filter for the specific error we're fixing
    const iterableErrors = consoleErrors.filter(e => e.includes('undefined is not iterable'));
    expect(iterableErrors).toHaveLength(0);
  });

  test('editor maintains focus after auto-save', async ({ page }) => {
    // This test verifies that typing in the editor doesn't lose focus
    // when the merkle root updates after auto-save

    // Create a new document
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.locator('input[placeholder="Document name..."]').fill('focus-test');
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for editor to be visible
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });
    await editor.click();

    // Type initial content
    const rootBefore = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await page.keyboard.type('First sentence.');

    // Wait for auto-save to complete
    await waitForTreeRootChange(page, rootBefore, 60000);

    // Verify editor still has focus (activeElement should be inside ProseMirror)
    const hasFocus = await page.evaluate(() => {
      const active = document.activeElement;
      const editor = document.querySelector('.ProseMirror');
      return editor?.contains(active) || active === editor;
    });
    expect(hasFocus).toBe(true);

    // Now type more content WITHOUT clicking the editor again
    // If focus is maintained, this text should appear after the first sentence
    const rootBeforeSecond = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await page.keyboard.type(' Second sentence.');

    // Verify both sentences are in the editor
    await expect(editor).toContainText('First sentence. Second sentence.', { timeout: 30000 });

    // Wait for the second save to complete
    await waitForTreeRootChange(page, rootBeforeSecond, 60000);

    // Type a third sentence
    const rootBeforeThird = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await page.keyboard.type(' Third sentence.');
    await waitForTreeRootChange(page, rootBeforeThird, 60000);

    // Verify all content is there
    await expect(editor).toContainText('First sentence. Second sentence. Third sentence.', { timeout: 30000 });
  });

  test('editor maintains focus during rapid typing and saves', async ({ page }) => {
    // This test verifies focus is maintained during rapid typing that triggers
    // multiple overlapping saves and merkle root updates

    // Create a new document
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.locator('input[placeholder="Document name..."]').fill('rapid-focus-test');
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for editor to be visible
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });
    await editor.click();

    // Type content rapidly, triggering save debounces
    const rootBefore = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    for (let i = 1; i <= 5; i++) {
      await page.keyboard.type(`Line ${i}. `);
      // Small delay to allow debounce to start but not complete
      await page.waitForTimeout(300);
    }

    // Wait for a save to complete
    await waitForTreeRootChange(page, rootBefore, 60000);

    // Now verify we can still type without clicking
    await page.keyboard.type('Final line.');

    // Verify all content is there
    await expect(editor).toContainText('Line 1. Line 2. Line 3. Line 4. Line 5. Final line.', { timeout: 30000 });
  });
});
