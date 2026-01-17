import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool } from './test-utils.js';

test.describe('Add Folder Navigation', () => {
  test.describe.configure({ timeout: 90000 });
  // Disable "others pool" to prevent WebRTC cross-talk from parallel tests
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
    await page.goto('/');
    await disableOthersPool(page);
  });

  test('should navigate into folder created via new folder button', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Click "New Folder" button using getByRole - will find the visible one
    await page.getByRole('button', { name: 'New Folder' }).first().click();

    // Type folder name in the modal
    await page.locator('input[placeholder="Folder name..."]').fill('nav-test-folder');
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for folder to appear in the list
    await expect(page.locator('a:has-text("nav-test-folder")')).toBeVisible({ timeout: 5000 });

    // Click the folder
    await page.locator('a:has-text("nav-test-folder")').click();

    // Should navigate to folder - URL contains folder name
    await page.waitForURL(/nav-test-folder/, { timeout: 5000 });
    expect(page.url()).toContain('nav-test-folder');
  });

  test('debug: check path resolution after clicking folder', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Get npub and treeName
    const routeInfo = await page.evaluate(async () => {
      const { parseRoute } = await import('/src/utils/route.ts');
      return parseRoute();
    });
    console.log('[test] Route:', routeInfo);

    // Create a folder using the proper action (like "new folder" button does)
    const createResult = await page.evaluate(async () => {
      // @ts-ignore - accessing app internals
      const store = await import('/src/store.ts');
      const stores = await import('/src/stores/index.ts');
      const routeUtils = await import('/src/utils/route.ts');
      const nostr = await import('/src/nostr.ts');

      const tree = store.getTree();
      const route = routeUtils.parseRoute();
      let rootCid = stores.getTreeRootSync(route.npub, route.treeName);
      const nostrState = nostr.nostrStore.getState();

      // Debug: check all state
      const debugState = {
        routeNpub: route.npub,
        routeTreeName: route.treeName,
        rootCidExists: !!rootCid?.hash,
        isOwnTree: nostr.isOwnTree(),
        nostrIsLoggedIn: nostrState.isLoggedIn,
        nostrPubkey: nostrState.pubkey?.slice(0, 16),
        selectedTree: nostrState.selectedTree ? {
          name: nostrState.selectedTree.name,
          pubkey: nostrState.selectedTree.pubkey?.slice(0, 16),
        } : null,
      };
      console.log('[eval] Debug state:', debugState);

      if (!rootCid?.hash) return { error: 'No root', debugState };

      // Create empty dir
      const { cid: emptyDir } = await tree.putDirectory([]);

      // Add as "debug-nav-folder"
      const newRoot = await tree.setEntry(rootCid, [], 'debug-nav-folder', emptyDir, 0, store.LinkType.Dir);

      // Check if autosave will work
      const willAutosave = nostr.isOwnTree() && nostrState.selectedTree && nostrState.npub;
      console.log('[eval] Will autosave:', willAutosave);

      // Autosave
      nostr.autosaveIfOwn(newRoot);

      // Check treeRootStore after autosave
      const treeRootAfter = stores.getTreeRootSync(route.npub, route.treeName);

      // Verify it's there
      const entries = await tree.listDirectory(newRoot);
      const found = entries.find((e: any) => e.name === 'debug-nav-folder');

      return {
        success: true,
        debugState,
        willAutosave,
        foundFolder: found ? { name: found.name, type: found.type } : null,
        treeRootUpdated: !!treeRootAfter?.hash
      };
    });
    console.log('[test] Create result:', JSON.stringify(createResult, null, 2));

    if (createResult.error) {
      console.log('[test] Error - debugState:', createResult.debugState);
      expect(createResult.error).toBeUndefined();
      return;
    }
    expect(createResult.success).toBe(true);

    const folderLocator = page.locator('text=debug-nav-folder').first();
    const folderVisible = await folderLocator.isVisible().catch(() => false);
    console.log('[test] Folder visible without reload:', folderVisible);

    if (!folderVisible) {
      console.log('[test] Folder not visible - checking why');
      // Debug: what does the UI show?
      const entriesInUI = await page.locator('[data-entry-name]').allTextContents();
      console.log('[test] Entries in UI:', entriesInUI);

      // Try reload
      await page.reload();
      await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });
    }

    // Now click the folder
    await expect(folderLocator).toBeVisible({ timeout: 10000 });
    await folderLocator.click();

    // Wait for navigation
    await page.waitForURL(/debug-nav-folder/, { timeout: 5000 });

    // URL should contain the folder name
    const url = page.url();
    expect(url).toContain('debug-nav-folder');
    console.log('[test] After click URL:', url);
  });

  test('debug folder entry type after upload', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Simulate what uploadFilesWithPaths does
    const result = await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const tree = getTree();
      const { getTreeRootSync } = await import('/src/stores/index.ts');
      const { parseRoute } = await import('/src/utils/route.ts');
      const route = parseRoute();

      let currentRootCid = getTreeRootSync(route.npub, route.treeName);

      if (!currentRootCid?.hash) {
        return { error: 'No root CID found' };
      }

      // This is what ensureDir does in upload.ts
      const { cid: emptyDirCid } = await tree.putDirectory([]);

      // Add folder - this is the fixed code path
      const newRootCid = await tree.setEntry(
        currentRootCid,
        [],
        'debug-folder',
        emptyDirCid,
        0,
        LinkType.Dir
      );

      // Save
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      autosaveIfOwn(newRootCid);

      // Check what we stored
      const entries = await tree.listDirectory(newRootCid);
      const debugFolder = entries.find(e => e.name === 'debug-folder');

      return {
        success: true,
        entries: entries.map(e => ({
          name: e.name,
          type: e.type,
          typeNumber: e.type,
          isDir: e.type === LinkType.Dir
        })),
        debugFolder: debugFolder ? {
          name: debugFolder.name,
          type: debugFolder.type,
          typeNumber: debugFolder.type,
          isDir: debugFolder.type === LinkType.Dir,
          LinkTypeDir: LinkType.Dir
        } : null
      };
    });

    console.log('[test] Debug result:', JSON.stringify(result, null, 2));
    expect(result.success).toBe(true);
    expect(result.debugFolder).toBeTruthy();
    expect(result.debugFolder.isDir).toBe(true);
  });

  test('should navigate into folder created via Add Folder (directory upload)', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // Simulate the uploadDirectory flow by calling uploadFilesWithPaths directly
    const uploadResult = await page.evaluate(async () => {
      const { uploadFilesWithPaths } = await import('/src/stores/upload.ts');
      const { getTreeRootSync } = await import('/src/stores/index.ts');
      const { parseRoute } = await import('/src/utils/route.ts');
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn, nostrStore } = await import('/src/nostr.ts');

      const route = parseRoute();
      const initialRoot = getTreeRootSync(route.npub, route.treeName);
      console.log('[eval] Initial root:', !!initialRoot?.hash);
      console.log('[eval] selectedTree:', nostrStore.getState().selectedTree?.name);

      // Create a fake file to upload into "upload-test-folder"
      const fakeFileContent = new TextEncoder().encode('test content');
      const fakeFile = new File([fakeFileContent], 'test.txt', { type: 'text/plain' });

      // Simulate what uploadDirectory does - upload with a folder path
      const filesWithPaths = [
        { file: fakeFile, relativePath: 'upload-test-folder/test.txt' }
      ];

      try {
        await uploadFilesWithPaths(filesWithPaths);
        console.log('[eval] Upload completed');

        // Wait a bit for autosave to process
        await new Promise(r => setTimeout(r, 500));

        // Check the tree after upload
        const tree = getTree();
        const newRoot = getTreeRootSync(route.npub, route.treeName);
        console.log('[eval] New root after upload:', !!newRoot?.hash);

        if (!newRoot?.hash) {
          return { error: 'No root after upload' };
        }

        const entries = await tree.listDirectory(newRoot);
        const folder = entries.find(e => e.name === 'upload-test-folder');

        return {
          success: true,
          folderFound: !!folder,
          folderType: folder?.type,
          folderIsDir: folder?.type === LinkType.Dir,
          LinkTypeDir: LinkType.Dir,
          entriesCount: entries.length,
          entryNames: entries.map(e => e.name)
        };
      } catch (err) {
        return { error: String(err) };
      }
    });

    console.log('[test] Upload result:', JSON.stringify(uploadResult, null, 2));
    expect(uploadResult.success).toBe(true);
    expect(uploadResult.folderFound).toBe(true);
    expect(uploadResult.folderIsDir).toBe(true);

    const folderLocator = page.locator('a:has-text("upload-test-folder")').first();
    const folderVisible = await folderLocator.isVisible().catch(() => false);
    console.log('[test] Folder visible in UI:', folderVisible);

    if (!folderVisible) {
      // Debug: what's in the file list?
      const allLinks = await page.locator('[data-testid="file-list"] a').allTextContents();
      console.log('[test] All links in file-list:', allLinks);

      // Try reload to see if it appears after refresh
      await page.reload();
      await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });
    }

    // Now try to click and navigate
    await expect(folderLocator).toBeVisible({ timeout: 10000 });
    await folderLocator.click();

    // Should navigate to the folder
    await page.waitForURL(/upload-test-folder/, { timeout: 5000 });
    expect(page.url()).toContain('upload-test-folder');

    // Verify we're inside the folder - should see test.txt
    await expect(page.locator('a:has-text("test.txt")')).toBeVisible({ timeout: 5000 });
    console.log('[test] Successfully navigated into folder and found test.txt');
  });

  test('debug: verify folder entry type is correct for navigation', async ({ page }) => {
    await navigateToPublicFolder(page, { timeoutMs: 60000, requireRelay: false });

    // First create a folder with a file like Add Folder does
    await page.evaluate(async () => {
      const { uploadFilesWithPaths } = await import('/src/stores/upload.ts');
      const fakeFile = new File(['content'], 'file.txt', { type: 'text/plain' });
      await uploadFilesWithPaths([{ file: fakeFile, relativePath: 'type-check-folder/file.txt' }]);
    });

    // Wait for folder to appear
    await expect(page.locator('a:has-text("type-check-folder")')).toBeVisible({ timeout: 5000 });

    // Check the folder entry's type as seen by FileBrowser
    const entryInfo = await page.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { getTreeRootSync } = await import('/src/stores/index.ts');
      const { parseRoute } = await import('/src/utils/route.ts');

      const route = parseRoute();
      const tree = getTree();
      const root = getTreeRootSync(route.npub, route.treeName);

      if (!root?.hash) return { error: 'No root' };

      const entries = await tree.listDirectory(root);
      const folder = entries.find(e => e.name === 'type-check-folder');

      if (!folder) return { error: 'Folder not found' };

      // Check if clicking this would navigate properly
      // FileBrowser uses entry.type === LinkType.Dir to determine folder icon
      // and resolvePath uses entry.type to determine if it's a directory

      // Also check what resolvePath returns for this folder
      const resolved = await tree.resolvePath(root, ['type-check-folder']);

      return {
        folderName: folder.name,
        folderType: folder.type,
        LinkTypeDir: LinkType.Dir,
        isDir: folder.type === LinkType.Dir,
        resolvedType: resolved?.type,
        resolvedIsDir: resolved?.type === LinkType.Dir
      };
    });

    console.log('[test] Entry info:', JSON.stringify(entryInfo, null, 2));
    expect(entryInfo.isDir).toBe(true);
    expect(entryInfo.resolvedIsDir).toBe(true);

    // Now click the folder
    await page.locator('a:has-text("type-check-folder")').click();

    // Should navigate
    await page.waitForURL(/type-check-folder/, { timeout: 5000 });

    // Should show file.txt inside
    await expect(page.locator('a:has-text("file.txt")')).toBeVisible({ timeout: 5000 });
  });
});
