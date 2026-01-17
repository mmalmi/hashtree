import { test, expect } from './fixtures';
import path from 'path';
import fs from 'fs';
import os from 'os';
import { fileURLToPath } from 'url';
import { setupPageErrorHandler, navigateToPublicFolder, goToTreeList, disableOthersPool, configureBlossomServers, waitForAppReady } from './test-utils.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Helper to create tree via modal and navigate into it
// NOTE: Since new users start in /public, we navigate to root first to create a NEW tree
async function createAndEnterTree(page: any, name: string) {
  // Go to user's tree list first
  await goToTreeList(page);

  // Wait for tree list to load with New Folder button
  await expect(page.getByRole('button', { name: 'New Folder' })).toBeVisible({ timeout: 10000 });

  await page.getByRole('button', { name: 'New Folder' }).click();
  await page.locator('input[placeholder="Folder name..."]').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  // After local createTree, navigates directly into empty tree
  await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 10000 });
}

// Helper to create a temp file and upload it (must be inside a tree)
async function uploadTempFile(page: any, name: string, content: string | Buffer) {
  const tmpDir = os.tmpdir();
  const filePath = path.join(tmpDir, name);
  fs.writeFileSync(filePath, content);
  const fileInput = page.locator('input[type="file"]').first();
  await fileInput.setInputFiles(filePath);
  await expect(page.getByTestId('file-list').locator('a').filter({ hasText: name }).first())
    .toBeVisible({ timeout: 10000 });
  fs.unlinkSync(filePath);
}

test.describe('Hashtree Explorer', () => {
  // Increase timeout for all tests since new user setup now creates 3 default folders
  test.setTimeout(60000);
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);

    // Go to page first to be able to clear storage
    await page.goto('/');
    await disableOthersPool(page);
    await configureBlossomServers(page);

    // Clear IndexedDB, localStorage, and OPFS before each test
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
        for await (const name of root.keys()) {
          await root.removeEntry(name, { recursive: true });
        }
      } catch {
        // OPFS might not be available
      }
    });

    // Reload to get truly fresh state (after clearing storage)
    await page.reload();
    await waitForAppReady(page); // Wait for page to load after reload
    await disableOthersPool(page); // Re-apply after reload
    await configureBlossomServers(page);

    // New users get auto-redirected to their public folder - wait for that
    await navigateToPublicFolder(page);
  });

  test('should display header and initial state', async ({ page }) => {
    // Header shows app name "Hashtree"
    await expect(page.locator('header').getByText('Iris').first()).toBeVisible({ timeout: 5000 });
    // New users are redirected to their public folder - shows "Empty directory" or folder actions
    await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 5000 });
  });

  // Uses File button instead of setInputFiles for reliable testing
  test('should create a local tree and create files', async ({ page }) => {
    const fileList = page.getByTestId('file-list');

    // Create tree via modal - this navigates into empty tree
    await createAndEnterTree(page, 'test-tree');

    // Create a file using File button
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('hello.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // File opens in edit mode - add content
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('Hello, World!');
    await page.getByRole('button', { name: 'Save' }).click();

    // Exit edit mode
    await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: 'Done' }).click();

    // Navigate back to directory
    await page.locator('a:has-text("test-tree")').first().click();

    // File should appear in file browser
    await expect(fileList.locator('a').filter({ hasText: 'hello.txt' }).first()).toBeVisible({ timeout: 10000 });

    // Click to view content
    await fileList.locator('a:has-text("hello.txt")').click();
    await expect(page.locator('pre')).toHaveText('Hello, World!', { timeout: 10000 });
  });

  test('should create file using File button', async ({ page }) => {
    const fileList = page.getByTestId('file-list');

    // Create tree via modal
    await createAndEnterTree(page, 'file-btn-test');

    // Create file using File button - this auto-opens in edit mode
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('test-file.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // File opens in edit mode - exit edit mode first
    await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: 'Done' }).click();

    // Navigate back to the tree directory by clicking on the tree name in sidebar
    await page.locator('a:has-text("file-btn-test")').first().click();

    // File should appear in file browser (use a:has-text since entries are links)
    await expect(fileList.locator('a').filter({ hasText: 'test-file.txt' }).first()).toBeVisible({ timeout: 10000 });
  });

  test('should create and edit a file', async ({ page }) => {
    const fileList = page.getByTestId('file-list');

    // Create tree via modal
    await createAndEnterTree(page, 'edit-test');

    // Create new file using File button - this auto-navigates to edit mode
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('editable.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for textarea to be visible (file creation navigates to edit mode)
    await expect(page.locator('textarea')).toBeVisible({ timeout: 10000 });

    // Type content and save
    await page.locator('textarea').fill('Hello, Hashtree!');
    await page.getByRole('button', { name: 'Save' }).click();

    // Click Done to exit edit mode
    await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: 'Done' }).click();

    // Click on file in list to ensure it's selected
    await page.getByRole('link', { name: 'editable.txt' }).click();

    // Content should be visible in preview
    await expect(page.locator('pre')).toHaveText('Hello, Hashtree!', { timeout: 10000 });
  });

  test('should persist file edits after navigation', async ({ page }) => {
    const fileList = page.getByTestId('file-list');

    // Create tree via modal
    await createAndEnterTree(page, 'persist-test');

    // Create a file with initial content using File button
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('persist.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for textarea (edit mode)
    await expect(page.locator('textarea')).toBeVisible({ timeout: 10000 });

    // Type initial content and save
    await page.locator('textarea').fill('Initial content');
    await page.getByRole('button', { name: 'Save' }).click();

    // Exit edit mode
    await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: 'Done' }).click();

    // Verify initial content
    await fileList.locator('a:has-text("persist.txt")').click();
    await expect(page.locator('pre')).toHaveText('Initial content', { timeout: 10000 });

    // Now edit to new content
    await page.getByRole('button', { name: 'Edit' }).click();
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });

    // Clear and retype to ensure the change is detected
    await page.locator('textarea').clear();
    await page.locator('textarea').fill('Updated content');

    await page.getByRole('button', { name: 'Save' }).click();

    // Wait for save to complete by checking that Save button becomes disabled
    // (disabled when content matches savedContent, meaning save completed)
    await expect(page.getByRole('button', { name: /Save/ })).toBeDisabled({ timeout: 5000 });

    // Exit edit mode
    await page.getByRole('button', { name: 'Done' }).click();

    // Poll for the updated content to appear in preview
    // The file viewer should reload content after the store updates
    await fileList.locator('a:has-text("persist.txt")').click();
    await expect(page.locator('pre')).toHaveText('Updated content', { timeout: 15000 });

    // Navigate to homepage
    await page.getByRole('link', { name: 'Iris' }).click();

    // Navigate back to the tree
    await goToTreeList(page);

    await page.locator(`a:has-text("persist-test")`).first().click();

    // File should still be in the list (use a:has-text since entries are links)
    await expect(fileList.locator('a').filter({ hasText: 'persist.txt' }).first()).toBeVisible({ timeout: 10000 });

    // Click the file
    await fileList.locator('a:has-text("persist.txt")').click();

    // Content should still be the updated value (persisted correctly)
    await expect(page.locator('pre')).toHaveText('Updated content', { timeout: 10000 });
  });

  test('should rename a file', async ({ page }) => {
    const fileList = page.getByTestId('file-list');

    // Create tree via modal
    await createAndEnterTree(page, 'rename-test');

    // Create file using File button
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('old-name.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // File opens in edit mode - add content
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('rename me');
    await page.getByRole('button', { name: 'Save' }).click();

    // Exit edit mode
    await expect(page.getByRole('button', { name: 'Done' })).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: 'Done' }).click();

    // Wait for Rename button in preview toolbar to be visible
    await expect(page.getByRole('button', { name: 'Rename' })).toBeVisible({ timeout: 5000 });

    // Click rename button
    await page.getByRole('button', { name: 'Rename' }).click();

    // Fill new name and submit by pressing Enter
    const input = page.locator('input[placeholder="New name..."]');
    await input.waitFor({ timeout: 5000 });
    await input.fill('new-name.txt');
    await input.press('Enter');

    // Wait for modal to close
    await expect(input).not.toBeVisible({ timeout: 5000 });

    // Navigate back to directory to see file list
    await page.locator('a:has-text("rename-test")').first().click();

    // Wait for new name to appear first (rename succeeded)
    await expect(fileList.locator('a').filter({ hasText: 'new-name.txt' }).first()).toBeVisible({ timeout: 10000 });
    // Then verify old name is gone
    await expect(fileList.locator('a').filter({ hasText: 'old-name.txt' })).not.toBeVisible({ timeout: 5000 });
  });

  test('should delete a file', async ({ page }) => {
    // Create tree via modal
    await createAndEnterTree(page, 'delete-file-test');

    // Create two files so we can verify specific file is deleted
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('keep-me.txt');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('keep this');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.getByRole('button', { name: 'Done' }).click();

    // Go back to directory - wait for back button to be visible first
    const backBtn = page.getByTestId('viewer-back');
    await expect(backBtn).toBeVisible({ timeout: 5000 });
    await backBtn.click();

    // Create file to delete
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('to-delete.txt');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('delete me');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.getByRole('button', { name: 'Done' }).click();

    // Go back to directory to verify both files are visible
    await page.getByTestId('viewer-back').click();
    const fileList = page.getByTestId('file-list');
    await expect(fileList.locator('a').filter({ hasText: 'to-delete.txt' })).toBeVisible({ timeout: 5000 });
    await expect(fileList.locator('a').filter({ hasText: 'keep-me.txt' })).toBeVisible({ timeout: 5000 });

    // Click on file to delete to open it in viewer
    await fileList.locator('a').filter({ hasText: 'to-delete.txt' }).click();

    // Wait for Delete button to be visible in preview toolbar (confirms we're in view mode)
    const deleteBtn = page.getByTestId('viewer-delete');
    await expect(deleteBtn).toBeVisible({ timeout: 10000 });

    // Click Delete button - set up dialog handler first
    page.once('dialog', dialog => dialog.accept());
    await deleteBtn.click();

    // Should navigate back to directory
    await expect(fileList).toBeVisible({ timeout: 5000 });

    // Verify deleted file is NOT listed
    await expect(fileList.locator('a').filter({ hasText: 'to-delete.txt' })).not.toBeVisible({ timeout: 5000 });

    // Verify other file IS still listed
    await expect(fileList.locator('a').filter({ hasText: 'keep-me.txt' })).toBeVisible({ timeout: 5000 });

    // Wait for Nostr publish (throttled 1s + network)
    await page.waitForTimeout(2000);

    // Reload and verify deletion persisted
    const treeUrl = page.url();
    await page.goto(treeUrl, { waitUntil: 'domcontentloaded' });
    await waitForAppReady(page);
    if (!page.url().includes('delete-file-test')) {
      await page.goto(treeUrl);
      await waitForAppReady(page);
    }
    await expect(fileList.locator('a').filter({ hasText: 'to-delete.txt' })).not.toBeVisible({ timeout: 15000 });
    await expect(fileList.locator('a').filter({ hasText: 'keep-me.txt' })).toBeVisible({ timeout: 10000 });
  });

  test('should delete a folder', async ({ page }) => {
    // Create tree via modal
    await createAndEnterTree(page, 'delete-folder-test');

    // Create a folder
    await page.getByRole('button', { name: /Folder/ }).first().click();
    await page.locator('input[placeholder="Folder name..."]').fill('to-delete-folder');
    await page.getByRole('button', { name: 'Create' }).click();
    await page.waitForTimeout(500);

    // Should see the folder in the list
    const fileList = page.getByTestId('file-list');
    await expect(fileList.locator('a').filter({ hasText: 'to-delete-folder' })).toBeVisible({ timeout: 5000 });

    // Click the folder to enter it
    await fileList.locator('a').filter({ hasText: 'to-delete-folder' }).click();
    await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 5000 });

    // Find and click delete button for the folder (in folder actions)
    // FolderActions renders in both FileBrowser and DirectoryActions (Viewer) - use visible one
    const deleteBtn = page.getByRole('button', { name: 'Delete' });
    await expect(deleteBtn).toBeVisible({ timeout: 5000 });

    // Click Delete button - set up dialog handler first
    page.once('dialog', dialog => dialog.accept());
    await deleteBtn.click();

    // Should navigate back to parent directory
    await expect(fileList).toBeVisible({ timeout: 5000 });

    // Verify deleted folder is NOT listed
    await expect(fileList.locator('a').filter({ hasText: 'to-delete-folder' })).not.toBeVisible({ timeout: 5000 });

    // Wait for Nostr publish (throttled 1s + network)
    await page.waitForTimeout(2000);

    // Reload and verify deletion persisted
    const treeUrl = page.url();
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForAppReady(page);
    if (!page.url().includes('delete-folder-test')) {
      await page.goto(treeUrl);
      await waitForAppReady(page);
    }
    await expect(fileList.locator('a').filter({ hasText: 'to-delete-folder' })).not.toBeVisible({ timeout: 15000 });
  });

  test('should handle tree names with slashes', async ({ page }) => {
    const fileList = page.getByTestId('file-list');

    // Create tree with slash in name
    await createAndEnterTree(page, 'videos/test-folder');

    // Create a file
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('test.txt');
    await page.getByRole('button', { name: 'Create' }).click();
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('content in slashed tree');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.getByRole('button', { name: 'Done' }).click();

    // Go back to directory
    const viewerBack = page.getByTestId('viewer-back');
    if (await viewerBack.isVisible().catch(() => false)) {
      await viewerBack.click();
    }
    await expect(fileList.locator('a').filter({ hasText: 'test.txt' })).toBeVisible({ timeout: 5000 });

    // Wait for publish
    await page.evaluate(async () => {
      const { flushPendingPublishes } = await import('/src/treeRootCache');
      await flushPendingPublishes();
    });

    // Reload and verify file persisted
    const encodedTreeName = encodeURIComponent('videos/test-folder');
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForAppReady(page);
    const npub = await page.evaluate(() => (window as any).__nostrStore?.getState?.().npub ?? null);
    if (npub && !page.url().includes(encodedTreeName)) {
      await page.goto(`http://localhost:5173/#/${npub}/${encodedTreeName}`);
      await waitForAppReady(page);
    }
    await page.evaluate(async () => {
      const { waitForTreeRoot } = await import('/src/stores');
      const { getRouteSync } = await import('/src/stores/route');
      const route = getRouteSync();
      if (route.npub && route.treeName) {
        await waitForTreeRoot(route.npub, route.treeName, 30000);
      }
    });
    await expect(fileList.locator('a').filter({ hasText: 'test.txt' })).toBeVisible({ timeout: 15000 });

    // Verify content
    await fileList.locator('a').filter({ hasText: 'test.txt' }).click();
    await expect(page.locator('pre')).toHaveText('content in slashed tree', { timeout: 5000 });
  });


  test('should open stream panel', async ({ page }) => {
    // Create tree via modal
    await createAndEnterTree(page, 'stream-test');

    // Click Stream link (now a Link instead of button)
    const streamLink = page.getByRole('link', { name: /Stream/ }).first();
    await expect(streamLink).toBeVisible({ timeout: 5000 });
    await streamLink.click();
    await page.waitForTimeout(300);

    // Should navigate to stream route and show the livestream panel
    await expect(page.getByText('Livestream', { exact: true })).toBeVisible({ timeout: 5000 });

    // Should have Start Camera button
    await expect(page.getByRole('button', { name: 'Start Camera' })).toBeVisible({ timeout: 5000 });

    // Close panel by navigating back to the folder using browser
    await page.goBack();
    await page.waitForTimeout(500);

    // Should be back in the folder - Stream link should be visible again in the toolbar
    await expect(page.getByRole('link', { name: /Stream/ }).first()).toBeVisible({ timeout: 5000 });
  });

  test('should show empty file content for new files', async ({ page }) => {
    const fileList = page.getByTestId('file-list');

    // Create tree via modal
    await createAndEnterTree(page, 'empty-file-test');

    // Create a file using File button
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('empty.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // File opens in edit mode - exit without adding content (empty file)
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: 'Done' }).click();
    await page.waitForTimeout(300);

    // Navigate back to directory
    await page.locator('a:has-text("empty-file-test")').first().click();
    await page.waitForTimeout(500);

    // File should appear in file browser
    await expect(fileList.locator('a').filter({ hasText: 'empty.txt' }).first()).toBeVisible({ timeout: 10000 });

    // Click to view - empty files should show empty pre tag (not download pane)
    await fileList.locator('a:has-text("empty.txt")').click();
    await page.waitForTimeout(500);

    // Should show viewer header with filename
    await expect(page.getByTestId('viewer-header')).toBeVisible({ timeout: 5000 });
    // Should NOT show download pane for empty text files
    await expect(page.locator('span.i-lucide-download')).not.toBeVisible();
    // Pre element exists in DOM (may be hidden when empty, but that's ok)
    await expect(page.locator('pre')).toHaveCount(1);
  });

  test('should cancel editing without saving', async ({ page }) => {
    // Create tree via modal
    await createAndEnterTree(page, 'cancel-test');

    // Create file using File button
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('cancel-test.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // File opens in edit mode - add content
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('original');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.waitForTimeout(500);

    // Exit edit mode
    await page.getByRole('button', { name: 'Done' }).click();

    // Wait for viewer header (file preview mode)
    await expect(page.getByTestId('viewer-header')).toBeVisible({ timeout: 5000 });

    // Wait for pre element with content
    await expect(page.locator('pre')).toHaveText('original', { timeout: 5000 });

    // Re-enter edit mode using test id
    const editBtn = page.getByTestId('viewer-edit');
    await expect(editBtn).toBeVisible({ timeout: 5000 });
    await editBtn.click();

    // Wait for textarea with current content
    const textarea = page.locator('textarea');
    await expect(textarea).toBeVisible({ timeout: 5000 });
    await expect(textarea).toHaveValue('original', { timeout: 5000 });

    // Type something but don't save
    await textarea.fill('This will be cancelled');

    // Click Done without saving
    await page.getByRole('button', { name: 'Done' }).click();

    // Unsaved changes modal should appear - click "Don't Save" to discard changes
    await expect(page.getByRole('heading', { name: 'Unsaved Changes' })).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: "Don't Save" }).click();

    // Should exit edit mode - verify viewer header is back
    await expect(page.getByTestId('viewer-header')).toBeVisible({ timeout: 5000 });
    await expect(page.locator('textarea')).not.toBeVisible();
    await expect(editBtn).toBeVisible();
  });

  test('should close modal by clicking outside', async ({ page }) => {
    // Create tree via modal
    await createAndEnterTree(page, 'modal-test');

    await page.getByRole('button', { name: /File/ }).first().click();
    await expect(page.locator('input[placeholder="File name..."]')).toBeVisible();

    // Click outside the modal
    await page.locator('div.fixed.inset-0.bg-black\\/70').click({ position: { x: 10, y: 10 } });
    await page.waitForTimeout(200);

    await expect(page.locator('input[placeholder="File name..."]')).not.toBeVisible();
  });


  test('should persist login across page reload', async ({ page }) => {
    // Avatar button should be visible (logged in state)
    const profileButton = page.locator('header button[title*="My Profile"]');
    await expect(profileButton).toBeVisible();

    // Reload page
    await page.reload();
    await page.waitForTimeout(500);

    // Should still be logged in - avatar button still visible
    await expect(profileButton).toBeVisible();
  });

  test('should navigate to settings page and display sections', async ({ page }) => {
    await page.locator('a[href="#/settings"]').first().click();
    await page.waitForTimeout(300);

    expect(page.url()).toContain('/settings');

    await expect(page.getByRole('button', { name: 'Servers' })).toBeVisible({ timeout: 5000 });
    await expect(page.getByText(/Relays \(\d+\)/)).toBeVisible({ timeout: 5000 });
    await expect(page.getByText(/localhost|relay\.damus\.io/)).toBeVisible({ timeout: 5000 });

    await page.getByRole('button', { name: 'P2P' }).click();
    await expect(page.locator('text=Peers').first()).toBeVisible({ timeout: 5000 });

    await page.getByRole('button', { name: 'Storage' }).click();
    await expect(page.getByText('Local Storage')).toBeVisible({ timeout: 5000 });
    await expect(page.getByText('Items')).toBeVisible({ timeout: 5000 });
    await expect(page.getByText('Size')).toBeVisible({ timeout: 5000 });
  });

  test('should navigate to wallet page', async ({ page }) => {
    // Click on the wallet link in header (HashRouter uses #/wallet)
    await page.locator('a[href="#/wallet"]').first().click();
    await page.waitForTimeout(300);

    // Should be on wallet page
    expect(page.url()).toContain('/wallet');
  });

  test('should navigate to edit profile page', async ({ page }) => {
    // We're already in public folder from navigateToPublicFolder in beforeEach
    // Get the npub from current URL
    const url = page.url();
    const npubMatch = url.match(/npub[a-z0-9]+/);
    expect(npubMatch).toBeTruthy();
    const npub = npubMatch![0];

    // Navigate to profile page
    await page.goto(`/#/${npub}/profile`);
    await page.waitForTimeout(300);

    // Should be on profile page with Edit Profile button
    await expect(page.getByRole('button', { name: 'Edit Profile' })).toBeVisible({ timeout: 5000 });

    // Click Edit Profile
    await page.getByRole('button', { name: 'Edit Profile' }).click();
    await page.waitForTimeout(300);

    // Should navigate to edit page with form fields
    expect(page.url()).toContain('/edit');
    await expect(page.locator('input[placeholder="Your name"]')).toBeVisible();
    await expect(page.locator('textarea[placeholder="Tell us about yourself"]')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Save' })).toBeVisible();

    // Fill in a name
    await page.locator('input[placeholder="Your name"]').fill('Test User');

    // Go back using the back button (chevron-left icon)
    await page.locator('button:has(span.i-lucide-chevron-left)').click();
    await page.waitForTimeout(300);

    // Should be back on profile page
    expect(page.url()).not.toContain('/edit');
    await expect(page.getByRole('button', { name: 'Edit Profile' })).toBeVisible();
  });

  test('should navigate to follows page and display following list', async ({ page }) => {
    // Get the npub from current URL
    const url = page.url();
    const npubMatch = url.match(/npub[a-z0-9]+/);
    expect(npubMatch).toBeTruthy();
    const npub = npubMatch![0];

    // Navigate to follows page
    await page.goto(`/#/${npub}/follows`);
    await page.waitForTimeout(300);

    // Should be on follows page
    expect(page.url()).toContain('/follows');

    // Should display Following count in header
    await expect(page.getByText(/Following \(\d+\)/)).toBeVisible({ timeout: 5000 });

    // Initially shows "Not following anyone yet" for new user
    await expect(page.getByText('Not following anyone yet')).toBeVisible({ timeout: 5000 });

    // Should have back button that leads to profile
    const backButton = page.locator('button:has(span.i-lucide-chevron-left)');
    await expect(backButton).toBeVisible();
  });

  test('should show trees listing on profile page in mobile view', async ({ page }) => {
    // Get the npub from current URL
    const url = page.url();
    const npubMatch = url.match(/npub[a-z0-9]+/);
    expect(npubMatch).toBeTruthy();
    const npub = npubMatch![0];

    // Set mobile viewport
    await page.setViewportSize({ width: 375, height: 667 });

    // Navigate to profile page
    await page.goto(`/#/${npub}/profile`);
    await page.waitForTimeout(300);

    // Should see ProfileView elements
    await expect(page.getByRole('button', { name: 'Edit Profile' })).toBeVisible({ timeout: 5000 });

    // Should also see FileBrowser/trees listing below profile (in mobile stacked layout)
    // In mobile, both desktop (hidden) and mobile (visible) file-lists exist - check that at least one is visible
    const fileLists = page.getByTestId('file-list');
    const count = await fileLists.count();
    expect(count).toBeGreaterThan(0);
    // Check the last one (mobile layout) is visible
    await expect(fileLists.last()).toBeVisible({ timeout: 5000 });
  });

  test('should display file content when directly navigating to file URL', async ({ page }) => {
    // Create tree and create a text file via File button
    await createAndEnterTree(page, 'direct-nav-test');

    // Create text file using File button
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('readme.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // File opens in edit mode - add content
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('Hello Direct Nav');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.waitForTimeout(300);

    // Exit edit mode
    await page.getByRole('button', { name: 'Done' }).click();
    await page.waitForTimeout(500);

    // Get current URL (should be the file URL)
    const fileUrl = page.url();
    expect(fileUrl).toContain('readme.txt');

    // Navigate away to tree list
    await goToTreeList(page);
    await page.waitForTimeout(500);

    // Navigate directly back to the file URL
    await page.goto(fileUrl);
    await page.waitForTimeout(500);

    // The file should be displayed in preview (shows filename in viewer header)
    await expect(page.getByTestId('viewer-header')).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId('viewer-header').getByText('readme.txt')).toBeVisible({ timeout: 5000 });

    // Content should be visible
    await expect(page.locator('pre')).toHaveText('Hello Direct Nav', { timeout: 5000 });
  });

  test('should display file content on mobile when directly navigating to file URL', async ({ page }) => {
    // Create tree and create a text file via File button
    await createAndEnterTree(page, 'mobile-file-test');

    // Create text file using File button
    await page.getByRole('button', { name: /File/ }).first().click();
    await page.locator('input[placeholder="File name..."]').fill('mobile-readme.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // File opens in edit mode - add content
    await expect(page.locator('textarea')).toBeVisible({ timeout: 5000 });
    await page.locator('textarea').fill('Hello Mobile View');
    await page.getByRole('button', { name: 'Save' }).click();
    await page.waitForTimeout(300);

    // Exit edit mode
    await page.getByRole('button', { name: 'Done' }).click();
    await page.waitForTimeout(500);

    // Get current URL (should be the file URL)
    const fileUrl = page.url();
    expect(fileUrl).toContain('mobile-readme.txt');

    // Navigate away to tree list
    await goToTreeList(page);
    await page.waitForTimeout(500);

    // Set mobile viewport BEFORE navigating
    await page.setViewportSize({ width: 375, height: 667 });
    await page.waitForTimeout(100);

    // Navigate directly back to the file URL
    await page.goto(fileUrl);
    await page.waitForTimeout(500);

    // On mobile, the file viewer should show (not the file browser)
    // The file should be displayed in preview (shows filename in viewer header)
    await expect(page.getByTestId('viewer-header')).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId('viewer-header').getByText('mobile-readme.txt')).toBeVisible({ timeout: 5000 });

    // Content should be visible
    await expect(page.locator('pre')).toHaveText('Hello Mobile View', { timeout: 5000 });
  });

  test('should display Yjs document editor on mobile when navigating to document folder', async ({ page }) => {
    // Navigate to public folder first (has New Document button)
    const { navigateToPublicFolder } = await import('./test-utils.js');
    await navigateToPublicFolder(page);

    // Create a Yjs document using New Document button
    await page.getByRole('button', { name: 'New Document' }).click();
    const docInput = page.locator('input[placeholder="Document name..."]');
    await expect(docInput).toBeVisible({ timeout: 10000 });
    await docInput.fill('mobile-doc');
    await page.getByRole('button', { name: 'Create' }).click();

    // Verify we're viewing the document (Tiptap editor should be visible)
    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 20000 });

    // Get the document URL
    const docUrl = page.url();
    expect(docUrl).toContain('mobile-doc');

    // Navigate away
    await goToTreeList(page);
    await page.waitForTimeout(500);

    // Set mobile viewport BEFORE navigating back
    await page.setViewportSize({ width: 375, height: 667 });
    await page.waitForTimeout(100);

    // Navigate directly back to the document URL
    await page.goto(docUrl);
    await waitForAppReady(page);

    // On mobile, the document editor should show (not the file browser)
    // The Tiptap ProseMirror editor should be visible
    await expect(page.locator('.ProseMirror')).toBeVisible({ timeout: 10000 });
  });
});
