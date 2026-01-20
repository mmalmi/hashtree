/**
 * E2E tests for linkvis (link-visible) trees
 *
 * Tests the three-tier visibility model:
 * - Creating link-visible trees with ?k= param in URL
 * - Uploading files to link-visible trees
 * - Accessing link-visible trees from a fresh browser with the link
 * - Verifying visibility icons in tree list and inside tree view
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, configureBlossomServers, waitForAppReady, goToTreeList, createFolder, flushPendingPublishes, waitForRelayConnected, clearAllStorage, ensureLoggedIn } from './test-utils.js';

async function waitForLinkKey(page: any): Promise<string> {
  await expect(page).toHaveURL(/\?k=[a-f0-9]+/i);
  const match = page.url().match(/\?k=([a-f0-9]+)/i);
  if (!match) {
    throw new Error('Expected ?k= param in URL');
  }
  return match[1];
}

async function waitForElapsed(page: any, minMs: number): Promise<void> {
  const start = Date.now();
  await page.waitForFunction(
    ({ startMs, minWait }: { startMs: number; minWait: number }) => Date.now() - startMs >= minWait,
    { startMs: start, minWait: minMs }
  );
}

async function createTreeWithVisibility(page: any, name: string, visibility: 'public' | 'link-visible' | 'private'): Promise<string | undefined> {
  await goToTreeList(page);
  const newFolderButton = page.getByRole('button', { name: 'New Folder' });
  await expect(newFolderButton).toBeVisible({ timeout: 30000 });
  await newFolderButton.click();

  const input = page.locator('input[placeholder="Folder name..."]');
  await expect(input).toBeVisible({ timeout: 10000 });
  await input.fill(name);

  if (visibility !== 'public') {
    const visibilityButton = page.getByRole('button', { name: new RegExp(visibility, 'i') });
    await visibilityButton.click();
    await expect(visibilityButton).toHaveClass(/ring-accent/);
  }

  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page).toHaveURL(new RegExp(`${name}`), { timeout: 30000 });
  await expect(page.getByRole('button', { name: 'New File' })).toBeVisible({ timeout: 30000 });

  if (visibility === 'link-visible') {
    return waitForLinkKey(page);
  }
  return undefined;
}

async function createFileWithContent(page: any, fileName: string, content: string): Promise<void> {
  await page.getByRole('button', { name: 'New File' }).click();
  const nameInput = page.locator('input[placeholder="File name..."]');
  await expect(nameInput).toBeVisible({ timeout: 10000 });
  await nameInput.fill(fileName);
  await page.getByRole('button', { name: 'Create' }).click();

  const editor = page.locator('textarea');
  await expect(editor).toBeVisible({ timeout: 30000 });
  await editor.fill(content);

  const saveButton = page.getByRole('button', { name: /Save|Saved|Saving/ });
  if (await saveButton.isEnabled().catch(() => false)) {
    await saveButton.click();
  }
  await expect(saveButton).toBeDisabled({ timeout: 30000 });

  await page.getByRole('button', { name: 'Done' }).click();
  await expect(editor).not.toBeVisible({ timeout: 30000 });
}

async function waitForTreePublished(page: any, npub: string, treeName: string, timeoutMs: number = 30000): Promise<void> {
  await waitForRelayConnected(page, Math.min(timeoutMs, 15000));
  await flushPendingPublishes(page);
  await page.waitForFunction(
    ({ owner, tree }) => {
      const raw = localStorage.getItem('hashtree:localRootCache');
      if (!raw) return false;
      try {
        const data = JSON.parse(raw);
        const entry = data?.[`${owner}/${tree}`];
        return entry && entry.dirty === false;
      } catch {
        return false;
      }
    },
    { owner: npub, tree: treeName },
    { timeout: timeoutMs }
  );
}

async function waitForTreeRoot(page: any, npub: string, treeName: string, timeoutMs: number = 60000): Promise<void> {
  await page.evaluate(async ({ targetNpub, targetTree, timeout }) => {
    const { waitForTreeRoot } = await import('/src/stores');
    await waitForTreeRoot(targetNpub, targetTree, timeout);
  }, { targetNpub: npub, targetTree: treeName, timeout: timeoutMs });
}

test.describe('Link-visible Tree Visibility', () => {
  // Increase timeout for all tests since new user setup now creates 3 default folders
  test.setTimeout(120000);

  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);

    // Go to page first to be able to clear storage
    await page.goto('/');
    await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests
    await configureBlossomServers(page);

    // Clear IndexedDB and localStorage before each test (including OPFS)
    await clearAllStorage(page, { clearOpfs: true });

    // Reload to get truly fresh state (after clearing storage)
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForAppReady(page, 60000); // Wait for page to load after reload
    await disableOthersPool(page); // Re-apply after reload
    await configureBlossomServers(page);

    // New users get auto-redirected to their public folder - wait for that
    await navigateToPublicFolder(page, { timeoutMs: 60000 });
  });

  test('should create link-visible tree with ?k= param in URL', async ({ page }) => {
    const linkKey = await createTreeWithVisibility(page, 'linkvis-test', 'link-visible');
    expect(linkKey).toBeTruthy();
    expect(page.url()).toContain(`?k=${linkKey}`);
    await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 30000 });
  });

  test('should show link icon for link-visible tree in tree list', async ({ page }) => {
    await createTreeWithVisibility(page, 'linkvis-icons', 'link-visible');
    await goToTreeList(page);

    // Find the linkvis-icons tree row and check for link icon (use file-list to avoid matching recent folders)
    const treeRow = page.getByTestId('file-list').locator('a:has-text("linkvis-icons")').first();
    await expect(treeRow).toBeVisible({ timeout: 30000 });

    // Should have link icon (i-lucide-link) for linkvis visibility
    const linkIcon = treeRow.locator('span.i-lucide-link');
    await expect(linkIcon).toBeVisible();
  });

  test('should show link icon inside link-visible tree view', async ({ page }) => {
    await createTreeWithVisibility(page, 'linkvis-inside', 'link-visible');

    // Should be inside the tree now - check for link icon in the current directory row
    const currentDirRow = page.locator('a:has-text("linkvis-inside")').first();
    await expect(currentDirRow).toBeVisible({ timeout: 30000 });

    // Should have link icon for linkvis visibility inside tree view
    const linkIcon = currentDirRow.locator('span.i-lucide-link');
    await expect(linkIcon).toBeVisible();
  });

  test('should preserve ?k= param when navigating within link-visible tree', async ({ page }) => {
    const kParam = await createTreeWithVisibility(page, 'linkvis-nav', 'link-visible');
    expect(kParam).toBeTruthy();
    await expect(page).toHaveURL(new RegExp(`linkvis-nav.*\\?k=${kParam}`), { timeout: 30000 });

    // Create a subfolder first (before creating files, to avoid edit mode)
    await createFolder(page, 'subfolder');
    await expect(page).toHaveURL(new RegExp(`linkvis-nav.*\\?k=${kParam}`), { timeout: 30000 });

    // Click on subfolder to navigate into it
    const subfolderLink = page.locator('[data-testid="file-list"] a:has-text("subfolder")').first();
    await expect(subfolderLink).toBeVisible({ timeout: 30000 });
    await subfolderLink.click();
    await expect(page).toHaveURL(new RegExp(`subfolder.*\\?k=${kParam}`), { timeout: 30000 });

    // Go back to parent using ".."
    const upLink = page.getByRole('link', { name: '..' }).first();
    await expect(upLink).toBeVisible({ timeout: 30000 });
    await upLink.click();
    await expect(page).toHaveURL(new RegExp(`linkvis-nav.*\\?k=${kParam}`), { timeout: 30000 });
  });

  test('should include ?k= param when clicking link-visible tree in tree list', async ({ page }) => {
    const kParam = await createTreeWithVisibility(page, 'linkvis-click', 'link-visible');
    expect(kParam).toBeTruthy();

    await goToTreeList(page);

    // Verify the RecentsView link has ?k= param
    const recentsLink = page.getByTestId('recents-view').locator('a', { hasText: 'linkvis-click' }).first();
    await expect(recentsLink).toBeVisible({ timeout: 30000 });
    const href = await recentsLink.getAttribute('href');
    expect(href).toContain(`?k=${kParam}`);

    await recentsLink.click();
    await expect(page).toHaveURL(new RegExp(`linkvis-click.*\\?k=${kParam}`), { timeout: 30000 });
  });

  test('should create file in link-visible tree and read it back', async ({ page }) => {
    test.slow();
    await createTreeWithVisibility(page, 'linkvis-file', 'link-visible');
    await createFileWithContent(page, 'secret.txt', 'This is secret content!');
    await expect(page.locator('pre')).toHaveText('This is secret content!', { timeout: 30000 });
  });

  test('should access link-visible tree from fresh browser with link', async ({ page, browser }) => {
    test.slow();
    page.on('console', msg => {
      const text = msg.text();
      if (text.includes('WebRTC') || text.includes('webrtc') || text.includes('peer')) {
        console.log(`[page1] ${text}`);
      }
    });

    const kParam = await createTreeWithVisibility(page, 'linkvis-share', 'link-visible');
    expect(kParam).toBeTruthy();

    // IMPORTANT: Wait at least 2 seconds before adding file
    // Nostr uses second-precision timestamps. If tree creation and file addition
    // happen in the same second, both events have the same created_at timestamp,
    // and the resolver may ignore the second event.
    await waitForElapsed(page, 2000);

    await createFileWithContent(page, 'shared.txt', 'Shared secret content');

    // Verify content is visible in view mode (may take time to render under load)
    await expect(page.locator('pre')).toBeVisible({ timeout: 30000 });
    await expect(page.locator('pre')).toHaveText('Shared secret content', { timeout: 30000 });

    // Verify tree is still visible in sidebar (confirms nostr publish succeeded)
    await expect(page.getByRole('link', { name: 'linkvis-share' })).toBeVisible({ timeout: 10000 });

    // Navigate back to tree root and verify file is there
    const ownerTreeLink = page.getByRole('link', { name: 'linkvis-share' }).first();
    await ownerTreeLink.click();
    await expect(page).toHaveURL(new RegExp(`linkvis-share.*\\?k=${kParam}`), { timeout: 30000 });
    const ownerFileLink = page.getByTestId('file-list').locator('text=shared.txt').first();
    await expect(ownerFileLink).toBeVisible({ timeout: 10000 });

    // Get the URL (should not have &edit=1 now)
    const shareUrl = page.url();
    expect(shareUrl).toMatch(/\?k=[a-f0-9]+/i);

    // Extract npub, treeName, and k param
    const urlMatch = shareUrl.match(/#\/(npub[^/]+)\/([^/?]+).*\?k=([a-f0-9]+)/i);
    expect(urlMatch).toBeTruthy();
    const [, npub, treeName] = urlMatch!;
    await waitForTreePublished(page, npub, treeName, 45000);

    // Open fresh browser context (no cookies, no localStorage)
    const context2 = await browser.newContext();
    const page2 = await context2.newPage();
    setupPageErrorHandler(page2);

    // Log WebRTC messages for debugging connectivity issues
    page2.on('console', msg => {
      const text = msg.text();
      if (text.includes('WebRTC') || text.includes('webrtc') || text.includes('peer')) {
        console.log(`[page2] ${text}`);
      }
    });

    // Navigate to home first so page2 gets a user identity
    await page2.goto('http://localhost:5173');
    await disableOthersPool(page2);
    await configureBlossomServers(page2);
    await waitForAppReady(page2);
    await waitForRelayConnected(page2, 20000);

    // Get page2's npub by clicking into their public folder
    await navigateToPublicFolder(page2, { timeoutMs: 60000 });
    const page2Url = page2.url();
    const page2Match = page2Url.match(/npub1[a-z0-9]+/);
    if (!page2Match) throw new Error('Could not find page2 npub in URL');
    const page2Npub = page2Match[0];
    console.log(`Page2 npub: ${page2Npub.slice(0, 20)}...`);

    // Page1 follows page2 for reliable WebRTC connection in follows pool
    await page.goto(`http://localhost:5173/#/${page2Npub}`);
    const followBtn = page.getByRole('button', { name: 'Follow', exact: true });
    await expect(followBtn).toBeVisible({ timeout: 30000 });
    await followBtn.click();
    await expect(
      page.getByRole('button', { name: 'Following' })
        .or(page.getByRole('button', { name: 'Unfollow' }))
        .or(followBtn.and(page.locator('[disabled]')))
    ).toBeVisible({ timeout: 10000 });

    // Page2 follows page1 (owner of the link-visible tree)
    await page2.goto(`http://localhost:5173/#/${npub}`);
    const followBtn2 = page2.getByRole('button', { name: 'Follow', exact: true });
    await expect(followBtn2).toBeVisible({ timeout: 30000 });
    await followBtn2.click();
    await expect(
      page2.getByRole('button', { name: 'Following' })
        .or(page2.getByRole('button', { name: 'Unfollow' }))
        .or(followBtn2.and(page2.locator('[disabled]')))
    ).toBeVisible({ timeout: 10000 });

    const fullUrlWithKey = `http://localhost:5173/#/${npub}/${treeName}?k=${kParam}`;
    await page2.goto(fullUrlWithKey);
    await waitForAppReady(page2, 60000);
    await waitForRelayConnected(page2, 30000);
    await disableOthersPool(page2);
    await configureBlossomServers(page2);
    await waitForTreeRoot(page2, npub, treeName, 60000);

    // Now look for the file
    const fileLink = page2.locator(`[data-testid="file-list"] >> text=shared.txt`);
    await expect(fileLink).toBeVisible({ timeout: 30000 });
    await fileLink.click();

    // Should NOT see "Link Required" - the key should work
    await expect(page2.getByText('Link Required')).not.toBeVisible({ timeout: 30000 });

    // Verify the content is decrypted and visible (may take time to fetch from network)
    // The fix to tryConnectedPeersForHash should handle the race condition
    // In parallel test runs, the "other" pool may be full with many test instances
    // so it might take longer to connect to the right peer (page1)
    await expect(page2.locator('text="Shared secret content"')).toBeVisible({ timeout: 45000 });

    // Also verify the file link is visible (should already be there if content is visible)
    await expect(page2.locator('[data-testid="file-list"] >> text=shared.txt')).toBeVisible({ timeout: 30000 });

    // Verify content remains visible (not replaced by "Link Required")
    await expect(page2.getByText('Link Required')).not.toBeVisible({ timeout: 10000 });
    await expect(page2.locator('text="Shared secret content"')).toBeVisible({ timeout: 10000 });

    await context2.close();
  });

  test('non-owner sees "Link Required" message when accessing link-visible tree without ?k= param', async ({ page, browser }) => {
    test.setTimeout(120000);
    await createTreeWithVisibility(page, 'linkvis-no-key', 'link-visible');

    // Extract npub and treeName from URL
    const shareUrl = page.url();
    console.log('Owner URL after creating link-visible tree:', shareUrl);
    const urlMatch = shareUrl.match(/#\/(npub[^/]+)\/([^/?]+)/);
    expect(urlMatch).toBeTruthy();
    const [, npub, treeName] = urlMatch!;
    await waitForTreePublished(page, npub, treeName, 45000);

    const context = await browser.newContext();
    const page2 = await context.newPage();

    try {
      setupPageErrorHandler(page2);
      await page2.goto('http://localhost:5173');
      await waitForAppReady(page2, 60000);
      await disableOthersPool(page2);
      await configureBlossomServers(page2);
      await ensureLoggedIn(page2, 30000);
      await waitForRelayConnected(page2, 20000);

      // Navigate to tree WITHOUT ?k= param - should show locked indicator
      const treeUrlWithoutKey = `http://localhost:5173/#/${npub}/${treeName}`;
      await page2.goto(treeUrlWithoutKey);
      await waitForAppReady(page2, 60000);
      await disableOthersPool(page2);
      await configureBlossomServers(page2);
      await waitForTreeRoot(page2, npub, treeName, 60000);

      // Should see "Link Required" message
      await expect(page2.getByText('Link Required')).toBeVisible({ timeout: 45000 });
      await expect(page2.getByText('This folder requires a special link to access')).toBeVisible();
    } finally {
      try {
        await context.close();
      } catch {
        // Ignore if context already closed by Playwright on timeout
      }
    }
  });

  test('owner can access link-visible tree without ?k= param (via selfEncryptedKey)', async ({ page }) => {
    await createTreeWithVisibility(page, 'linkvis-owner', 'link-visible');

    // Get URL with ?k= and then navigate WITHOUT it
    const shareUrl = page.url();
    const urlMatch = shareUrl.match(/#\/(npub[^/]+)\/([^/?]+)/);
    expect(urlMatch).toBeTruthy();
    const [, npub, treeName] = urlMatch!;

    // Navigate to tree WITHOUT ?k= param (owner should still have access via selfEncryptedKey)
    const treeUrlWithoutKey = `http://localhost:5173/#/${npub}/${treeName}`;
    await page.goto(treeUrlWithoutKey);

    // Owner should still be able to access (via selfEncryptedKey decryption)
    // The tree should show "Empty directory" since owner can decrypt
    await expect(page.getByText('Empty directory')).toBeVisible({ timeout: 30000 });
  });

  test('should preserve ?k= param after creating file in link-visible tree', async ({ page }) => {
    test.slow(); // File operations can be slow under parallel load
    const kParam = await createTreeWithVisibility(page, 'linkvis-upload', 'link-visible');
    expect(kParam).toBeTruthy();

    // Create a new file using the File button
    await page.getByRole('button', { name: 'New File' }).click();
    const nameInput = page.locator('input[placeholder="File name..."]');
    await expect(nameInput).toBeVisible({ timeout: 10000 });
    await nameInput.fill('uploaded.txt');
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for edit mode, then type content and save
    const editor = page.locator('textarea');
    await expect(editor).toBeVisible({ timeout: 30000 });
    await editor.fill('Test file content for upload');

    const saveButton = page.getByRole('button', { name: /Save|Saved|Saving/ });
    if (await saveButton.isEnabled().catch(() => false)) {
      await saveButton.click();
    }
    await expect(saveButton).toBeDisabled({ timeout: 30000 });

    // Check URL still has ?k= param after saving the file
    expect(page.url()).toContain(`?k=${kParam}`);

    // Exit edit mode
    await page.getByRole('button', { name: 'Done' }).click();
    await expect(editor).not.toBeVisible({ timeout: 30000 });

    // Check URL still has ?k= param after exiting edit mode
    expect(page.url()).toContain(`?k=${kParam}`);
  });

  test('should preserve ?k= param after drag-and-drop upload to link-visible tree', async ({ page }) => {
    test.slow(); // Upload operations can be slow under parallel load
    const kParam = await createTreeWithVisibility(page, 'linkvis-dnd', 'link-visible');
    expect(kParam).toBeTruthy();

    // Create a buffer for the file content
    const buffer = Buffer.from('Drag and drop test content');

    // Use Playwright's setInputFiles on the hidden file input if there is one
    // Or simulate drag and drop via the DataTransfer API
    const dataTransfer = await page.evaluateHandle(() => new DataTransfer());
    await page.evaluate(([dt, content]) => {
      const file = new File([new Uint8Array(content)], 'dropped.txt', { type: 'text/plain' });
      (dt as DataTransfer).items.add(file);
    }, [dataTransfer, [...buffer]] as const);

    // Find the drop target and dispatch events
    const dropTarget = page.getByTestId('file-list');
    await expect(dropTarget).toBeVisible({ timeout: 30000 });
    await dropTarget.dispatchEvent('dragenter', { dataTransfer });
    await dropTarget.dispatchEvent('dragover', { dataTransfer });
    await dropTarget.dispatchEvent('drop', { dataTransfer });

    // Check if file appeared
    const droppedFile = page.getByText('dropped.txt');
    await expect(droppedFile).toBeVisible({ timeout: 30000 });

    // Check URL still has ?k= param
    const urlAfterDrop = page.url();
    console.log('URL after drop:', urlAfterDrop);
    expect(urlAfterDrop).toContain(`?k=${kParam}`);
  });

  test('link-visible tree should remain linkvis after file upload (not become public)', async ({ page }) => {
    test.slow(); // File operations can be slow under parallel load
    // This test verifies that uploading files to an link-visible tree doesn't
    // accidentally change its visibility to public (regression test for
    // autosaveIfOwn not preserving visibility)

    const kParam = await createTreeWithVisibility(page, 'linkvis-stays-linkvis', 'link-visible');
    expect(kParam).toBeTruthy();

    // Verify the tree shows link icon (linkvis)
    const currentDirRow = page.locator('a:has-text("linkvis-stays-linkvis")').first();
    await expect(currentDirRow).toBeVisible({ timeout: 30000 });
    await expect(currentDirRow.locator('span.i-lucide-link')).toBeVisible();

    await createFileWithContent(page, 'visibility-test.txt', 'Test content for visibility check');

    // Go back to tree list
    await goToTreeList(page);

    // CRITICAL: Verify the tree still has link icon (linkvis), NOT globe icon (public)
    const treeRow = page.getByTestId('file-list').locator('a:has-text("linkvis-stays-linkvis")').first();
    await expect(treeRow).toBeVisible({ timeout: 30000 });

    // Should have link icon (linkvis), not globe icon (public)
    await expect(treeRow.locator('span.i-lucide-link')).toBeVisible();

    // Should NOT have globe icon (public)
    const globeIcon = treeRow.locator('span.i-lucide-globe');
    await expect(globeIcon).not.toBeVisible();

    // Click on the tree and verify ?k= param is still in URL
    await treeRow.click();
    await expect(page).toHaveURL(new RegExp(`linkvis-stays-linkvis.*\\?k=${kParam}`), { timeout: 30000 });
  });

  test('should show correct visibility icons for different tree types', async ({ page }) => {
    test.slow(); // Creates multiple trees, can be slow under parallel load
    await createTreeWithVisibility(page, 'public-tree', 'public');
    await createTreeWithVisibility(page, 'link-visible-tree', 'link-visible');
    await createTreeWithVisibility(page, 'private-tree', 'private');
    await goToTreeList(page);

    // Verify icons for each tree type (use file-list testid to avoid matching recent folders)
    const fileList = page.getByTestId('file-list');

    // Public tree should be visible but have NO icon (public is default, no indicator needed)
    const publicRow = fileList.locator('a:has-text("public-tree")').first();
    await expect(publicRow).toBeVisible({ timeout: 30000 });
    // Public trees intentionally don't show any visibility icon - verify it's absent
    await expect(publicRow.locator('span.i-lucide-globe')).not.toBeVisible();

    // Link-visible tree should have link icon
    const linkvisRow = fileList.locator('a:has-text("link-visible-tree")').first();
    await expect(linkvisRow).toBeVisible({ timeout: 30000 });
    await expect(linkvisRow.locator('span.i-lucide-link')).toBeVisible({ timeout: 30000 });

    // Private tree should have lock icon
    const privateRow = fileList.locator('a:has-text("private-tree")').first();
    await expect(privateRow).toBeVisible({ timeout: 30000 });
    await expect(privateRow.locator('span.i-lucide-lock')).toBeVisible({ timeout: 30000 });
  });

  test('files in link-visible trees should be encrypted (have CHK)', async ({ page }) => {
    test.slow(); // File operations can be slow under parallel load
    // This test verifies that files uploaded to link-visible trees are properly encrypted
    // and have CHK (Content Hash Key) in the permalink

    await createTreeWithVisibility(page, 'linkvis-encrypted', 'link-visible');
    await createFileWithContent(page, 'encrypted-file.txt', 'This content should be encrypted');

    // Wait for file viewer to load (may take time under parallel load)
    // Look for the content text first as it's more reliable than the pre element
    await expect(page.getByText('This content should be encrypted')).toBeVisible({ timeout: 30000 });

    // Look for the file's Permalink link (the one with visible text, not just icon)
    const permalinkLink = page.getByRole('link', { name: 'Permalink' });
    await expect(permalinkLink).toBeVisible({ timeout: 15000 });

    // Get the href of the permalink
    const permalinkHref = await permalinkLink.getAttribute('href');
    console.log('Permalink href:', permalinkHref);
    expect(permalinkHref).toBeTruthy();

    // The nhash should be longer than 32 bytes (simple hash) if it includes a key
    // Simple nhash (32 bytes hash) = ~58 chars (nhash1 + bech32 of 32 bytes)
    // TLV nhash with key should be longer since it includes hash TLV + key TLV
    const nhashMatch = permalinkHref!.match(/nhash1[a-z0-9]+/);
    expect(nhashMatch).toBeTruthy();
    const nhash = nhashMatch![0];
    console.log('nhash:', nhash);
    console.log('nhash length:', nhash.length);

    // A simple 32-byte hash encoded in bech32 is about 58 chars
    // With TLV (hash + key), it should be longer (around 115+ chars)
    // If the file is encrypted, the nhash should include the decrypt key
    expect(nhash.length).toBeGreaterThan(70); // Should have TLV encoding with key
  });

  test('owner can create and write to private folder', async ({ page }) => {
    test.slow();
    await createTreeWithVisibility(page, 'my-private', 'private');

    // Should be inside the private tree now, not showing "Link Required"
    // The owner should be able to see the folder contents
    await expect(page.locator('text="Link Required"')).not.toBeVisible({ timeout: 30000 });
    await expect(page.locator('text="Private Folder"')).not.toBeVisible({ timeout: 30000 });

    await createFileWithContent(page, 'secret.txt', 'My secret content');

    // Verify content is visible
    await expect(page.locator('pre')).toHaveText('My secret content', { timeout: 30000 });

    // Navigate away and back to verify persistence
    await goToTreeList(page);

    // Click on the private tree
    const privateTree = page.getByTestId('file-list').locator('a:has-text("my-private")').first();
    await expect(privateTree).toBeVisible({ timeout: 30000 });
    await privateTree.click();

    // Should still not show the locked message
    await expect(page.locator('text="Link Required"')).not.toBeVisible({ timeout: 30000 });
    await expect(page.locator('text="Private Folder"')).not.toBeVisible({ timeout: 30000 });

    // The file should be visible
    await expect(page.locator('text="secret.txt"')).toBeVisible({ timeout: 30000 });
  });
});
