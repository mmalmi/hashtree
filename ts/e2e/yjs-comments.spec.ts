/**
 * E2E tests for Yjs document comments feature
 *
 * Tests Google Docs-style commenting on collaborative documents.
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, configureBlossomServers } from './test-utils.js';

test.describe('Yjs Document Comments', () => {
  test.setTimeout(60000);

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);

    await page.goto('/');
    await disableOthersPool(page);
    await configureBlossomServers(page);

    // Clear storage for fresh state (including OPFS)
    await page.evaluate(async () => {
      const dbs = await indexedDB.databases();
      for (const db of dbs) {
        if (db.name) indexedDB.deleteDatabase(db.name);
      }
      localStorage.clear();
      sessionStorage.clear();

      // Clear OPFS
      try {
        const root = await navigator.storage.getDirectory();
        // @ts-ignore - removeEntry is available in OPFS
        for await (const name of root.keys()) {
          await root.removeEntry(name, { recursive: true });
        }
      } catch {
        // OPFS might not be available
      }
    });

    await page.reload();
    await page.waitForTimeout(500);
    await configureBlossomServers(page);
    // Page ready - navigateToPublicFolder handles waiting
    await navigateToPublicFolder(page);
  });

  test('comment button is disabled when no text selected', async ({ page }) => {
    // Create a new document
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.waitForTimeout(500);
    await page.locator('input[placeholder="Document name..."]').fill('comment-test');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(2000);

    // Wait for editor to be visible
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });

    // Comment button should be disabled (no selection)
    const addCommentBtn = page.locator('button[title="Add comment (select text first)"]');
    await expect(addCommentBtn).toBeVisible({ timeout: 10000 });
    await expect(addCommentBtn).toBeDisabled();
  });

  test('comment button is enabled when text is selected', async ({ page }) => {
    // Create a new document
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.waitForTimeout(500);
    await page.locator('input[placeholder="Document name..."]').fill('comment-test-2');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(2000);

    // Wait for editor to be visible and type some text
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });
    await editor.click();
    await page.keyboard.type('This is some text to comment on.');
    await page.waitForTimeout(500);

    // Select some text
    await page.keyboard.press('Control+a');
    await page.waitForTimeout(200);

    // Comment button should now be enabled
    const addCommentBtn = page.locator('button[title="Add comment (select text first)"]');
    await expect(addCommentBtn).toBeEnabled({ timeout: 5000 });
  });

  test('comments panel toggles visibility', async ({ page }) => {
    // Create a new document
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.waitForTimeout(500);
    await page.locator('input[placeholder="Document name..."]').fill('panel-test');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(2000);

    // Wait for editor
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });

    // Comments panel should be hidden initially
    const commentsPanel = page.locator('h3:has-text("Comments")');
    await expect(commentsPanel).not.toBeVisible();

    // Click toggle button to show panel
    const toggleBtn = page.locator('button[title="Toggle comments panel"]');
    await toggleBtn.click();
    await page.waitForTimeout(300);

    // Panel should now be visible
    await expect(commentsPanel).toBeVisible({ timeout: 5000 });

    // Panel should show "No comments yet" message
    await expect(page.locator('text=No comments yet')).toBeVisible();

    // Click toggle again to hide
    await toggleBtn.click();
    await page.waitForTimeout(300);
    await expect(commentsPanel).not.toBeVisible();
  });

  test('toolbar shows comment buttons', async ({ page }) => {
    // Create a new document
    await page.getByRole('button', { name: 'New Document' }).click();
    await page.waitForTimeout(500);
    await page.locator('input[placeholder="Document name..."]').fill('toolbar-test');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(2000);

    // Wait for editor
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });

    // Should have add comment button
    const addCommentBtn = page.locator('button[title="Add comment (select text first)"]');
    await expect(addCommentBtn).toBeVisible({ timeout: 10000 });

    // Should have toggle panel button
    const toggleBtn = page.locator('button[title="Toggle comments panel"]');
    await expect(toggleBtn).toBeVisible({ timeout: 5000 });
  });
});
