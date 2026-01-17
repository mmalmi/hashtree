/**
 * E2E tests for image insertion and collaboration in Yjs documents
 *
 * Tests that:
 * 1. User A can insert an image into a document
 * 2. User B (collaborator) can see the image
 * 3. Images use /htree/ service worker URLs (not blob URLs)
 */
import { test, expect, Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, configureBlossomServers, waitForWebRTCConnection, waitForRelayConnected, waitForAppReady, clearAllStorage, navigateToPublicFolder } from './test-utils.js';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';

// Minimal valid 1x1 red PNG as byte array
const PNG_BYTES = new Uint8Array([
  0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
  0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
  0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
  0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // RGB, no interlace, CRC
  0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
  0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, // compressed data (red pixel)
  0x03, 0x00, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB4, // CRC
  0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND
  0xAE, 0x42, 0x60, 0x82 // CRC
]);

// Helper to create a temp PNG file and return its path
function createTempPngFile(): string {
  const tmpDir = os.tmpdir();
  const tmpFile = path.join(tmpDir, `test-image-${Date.now()}.png`);
  fs.writeFileSync(tmpFile, PNG_BYTES);
  return tmpFile;
}

async function getTreeRootHash(page: Page): Promise<string | null> {
  return page.evaluate(async () => {
    const { getTreeRootSync } = await import('/src/stores');
    const { getRouteSync } = await import('/src/stores/route');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');

    const route = getRouteSync();
    if (!route.npub || !route.treeName) return null;
    const root = getTreeRootSync(route.npub, route.treeName);
    return root ? toHex(root.hash) : null;
  });
}

async function waitForTreeRootChange(page: Page, previousHash: string | null, timeoutMs = 60000): Promise<void> {
  await page.waitForFunction(async (prevHash) => {
    const { getTreeRootSync } = await import('/src/stores');
    const { getRouteSync } = await import('/src/stores/route');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');

    const route = getRouteSync();
    if (!route.npub || !route.treeName) return false;
    const root = getTreeRootSync(route.npub, route.treeName);
    if (!root) return false;
    return toHex(root.hash) !== prevHash;
  }, previousHash, { timeout: timeoutMs });
}

async function waitForTreeRootHash(
  page: Page,
  npub: string,
  treeName: string,
  expectedHash: string,
  timeoutMs = 60000
): Promise<void> {
  await page.waitForFunction(async ({ targetNpub, targetTree, targetHash }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');

    const root = getTreeRootSync(targetNpub, targetTree);
    if (!root) return false;
    return toHex(root.hash) === targetHash;
  }, { targetNpub: npub, targetTree: treeName, targetHash: expectedHash }, { timeout: timeoutMs });
}

// Helper to set up a fresh user session
async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);

  await page.goto('http://localhost:5173');
  await disableOthersPool(page);
  await configureBlossomServers(page);

  // Clear storage for fresh state (including OPFS)
  await clearAllStorage(page, { clearOpfs: true });

  try {
    await page.reload();
  } catch {
    await page.goto('http://localhost:5173', { waitUntil: 'domcontentloaded' });
  }
  await waitForAppReady(page); // Wait for page to load after reload
  await disableOthersPool(page);
  await configureBlossomServers(page);

  await navigateToPublicFolder(page, { timeoutMs: 60000 });
}

// Helper to get the user's npub from the URL
async function getNpub(page: Page): Promise<string> {
  const url = page.url();
  const match = url.match(/npub1[a-z0-9]+/);
  if (!match) throw new Error('Could not find npub in URL');
  return match[0];
}

// Helper to create a document with a given name
async function createDocument(page: Page, name: string) {
  const newDocButton = page.getByRole('button', { name: 'New Document' });
  await expect(newDocButton).toBeVisible({ timeout: 30000 });
  await newDocButton.click();

  const input = page.locator('input[placeholder="Document name..."]');
  await expect(input).toBeVisible({ timeout: 30000 });
  await input.fill(name);

  const createButton = page.getByRole('button', { name: 'Create' });
  await expect(createButton).toBeVisible({ timeout: 30000 });
  await createButton.click();

  await page.waitForURL(`**/${name}**`, { timeout: 20000 });

  const editor = page.locator('.ProseMirror');
  await expect(editor).toBeVisible({ timeout: 30000 });
}

// Helper to wait for auto-save
async function waitForSave(page: Page) {
  const savingStatus = page.locator('text=Saving');
  const savedStatus = page.locator('text=Saved').or(page.locator('text=/Saved \\d/'));

  try {
    await expect(savingStatus).toBeVisible({ timeout: 5000 });
  } catch {
    if (await savedStatus.isVisible()) {
      return;
    }
  }

  await expect(savedStatus).toBeVisible({ timeout: 30000 });
}

// Helper to set editors using the Collaborators modal UI
async function setEditors(page: Page, npubs: string[]) {
  const collabButton = page.locator('button[title="Manage editors"], button[title="View editors"]').first();
  await expect(collabButton).toBeVisible({ timeout: 30000 });
  await collabButton.click();

  const modal = page.locator('h2:has-text("Editors")');
  await expect(modal).toBeVisible({ timeout: 30000 });

  for (const npub of npubs) {
    const input = page.locator('input[placeholder="npub1..."]');
    await input.fill(npub);

    const confirmButton = page.locator('button.btn-success').filter({ hasText: /^Add/ }).first();
    await expect(confirmButton).toBeVisible({ timeout: 30000 });
    await confirmButton.click({ force: true });
  }

  const closeButton = page.getByText('Close', { exact: true });
  await closeButton.click();
  await expect(modal).not.toBeVisible({ timeout: 30000 });
}

// Helper to follow a user by their npub
async function followUser(page: Page, targetNpub: string) {
  await page.goto(`http://localhost:5173/#/${targetNpub}`);

  const followButton = page.getByRole('button', { name: 'Follow', exact: true });
  await expect(followButton).toBeVisible({ timeout: 30000 });
  await followButton.click();

  await expect(
    page.getByRole('button', { name: 'Following' })
      .or(page.getByRole('button', { name: 'Unfollow' }))
      .or(followButton.and(page.locator('[disabled]')))
  ).toBeVisible({ timeout: 30000 });
}

// Helper to navigate to another user's document
async function navigateToUserDocument(page: Page, npub: string, treeName: string, docPath: string) {
  const url = `http://localhost:5173/#/${npub}/${treeName}/${docPath}`;
  await page.goto(url);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
}

test.describe('Document Image Collaboration', () => {
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(180000); // 3 minutes for collaboration test

  test('User A inserts image, User B (collaborator) sees it', async ({ browser }) => {
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();

    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    // Enable console logging for debugging
    pageA.on('console', msg => {
      const text = msg.text();
      if (msg.type() === 'error') console.log(`[User A Error] ${text}`);
      if (text.includes('[YjsDoc')) console.log(`[User A] ${text}`);
    });
    pageB.on('console', msg => {
      const text = msg.text();
      if (msg.type() === 'error') console.log(`[User B Error] ${text}`);
      if (text.includes('[YjsDoc')) console.log(`[User B] ${text}`);
    });

    try {
      // === Setup User A ===
      console.log('Setting up User A...');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      console.log(`User A npub: ${npubA.slice(0, 20)}...`);

      // === Setup User B ===
      console.log('Setting up User B...');
      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      console.log(`User B npub: ${npubB.slice(0, 20)}...`);

      // === Users follow each other (required for WebRTC) ===
      console.log('User A: Following User B...');
      await followUser(pageA, npubB);
      console.log('User B: Following User A...');
      await followUser(pageB, npubA);

      // Wait for WebRTC connection
      console.log('Waiting for WebRTC connection...');
      await waitForWebRTCConnection(pageA, 30000);
      await waitForWebRTCConnection(pageB, 30000);
      console.log('WebRTC connected');

      // Navigate back to public folders
      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await expect(pageA.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });
      await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
      await expect(pageB.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

      // === User A: Create document ===
      console.log('User A: Creating document...');
      await createDocument(pageA, 'image-collab-test');

      // === User A: Add B as editor ===
      console.log('User A: Setting editors (A and B)...');
      await setEditors(pageA, [npubA, npubB]);
      console.log('User A: Editors set');

      // === User A: Type some text first ===
      const editorA = pageA.locator('.ProseMirror');
      await expect(editorA).toBeVisible({ timeout: 30000 });
      await editorA.click();
      await pageA.keyboard.type('Test document with image: ');
      await waitForSave(pageA);
      console.log('User A: Text typed and saved');
      const rootHashBeforeImage = await getTreeRootHash(pageA);
      expect(rootHashBeforeImage).toBeTruthy();

      // === User A: Insert image via the toolbar button ===
      console.log('User A: Inserting image...');

      // Create a temp PNG file
      const tmpFile = createTempPngFile();

      // Click the "Insert Image" button in the toolbar
      const imageButton = pageA.locator('button[title="Insert Image"]');
      await expect(imageButton).toBeVisible({ timeout: 10000 });

      // Get the hidden file input
      const fileInput = pageA.locator('input[type="file"][accept="image/*"]');

      // Upload the file via the hidden input
      await fileInput.setInputFiles(tmpFile);

      // Clean up temp file
      fs.unlinkSync(tmpFile);

      await waitForSave(pageA);
      await waitForTreeRootChange(pageA, rootHashBeforeImage);
      const rootHashAfterImage = await getTreeRootHash(pageA);
      expect(rootHashAfterImage).toBeTruthy();
      console.log('User A: Image inserted and saved');

      // Ensure latest root is published before User B loads the document
      await pageA.evaluate(async () => {
        const { flushPendingPublishes } = await import('/src/treeRootCache');
        await flushPendingPublishes();
      });

      // Verify image is visible in User A's editor
      const imageA = editorA.locator('img');
      await expect(imageA).toBeVisible({ timeout: 10000 });

      // User A sees /htree/ URL - SW waits for tree root
      const srcA = await imageA.getAttribute('src');
      console.log(`User A image src: ${srcA}`);
      expect(srcA).toContain('/htree/');
      expect(srcA).not.toContain('attachments:');

      // Allow sync to propagate via relay/WebRTC
      console.log('Waiting for sync to propagate...');

      // === User B: Navigate to User A's document ===
      console.log('User B: Navigating to User A\'s document...');
      await navigateToUserDocument(pageB, npubA, 'public', 'image-collab-test');
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashAfterImage!);

      // Wait for editor to load
      const editorB = pageB.locator('.ProseMirror');
      await expect(editorB).toBeVisible({ timeout: 60000 });

      // Wait for text content to appear (indicates document loaded)
      await expect(editorB).toContainText('Test document with image', { timeout: 60000 });
      console.log('User B: Document content loaded');

      // === Key test: User B should see the image ===
      console.log('User B: Checking for image...');
      const imageB = editorB.locator('img');
      await expect(imageB).toBeVisible({ timeout: 60000 });

      // Verify the image src resolves correctly (uses /htree/ URL)
      const srcB = await imageB.getAttribute('src');
      console.log(`User B image src: ${srcB}`);
      expect(srcB).toContain('/htree/');
      expect(srcB).not.toContain('blob:');
      expect(srcB).not.toContain('attachments:');

      const imageHandle = await imageB.elementHandle();
      if (imageHandle) {
        await pageB.waitForFunction(
          (img) => img.complete && img.naturalWidth > 0,
          imageHandle,
          { timeout: 60000 }
        );
      }

      // Verify the image actually loads (not broken)
      const imageLoadStatus = await imageB.evaluate(async (img: HTMLImageElement) => {
        // If not complete, wait for load event
        if (!img.complete) {
          await new Promise<void>((resolve, reject) => {
            img.onload = () => resolve();
            img.onerror = () => reject(new Error('Image failed to load'));
            setTimeout(() => reject(new Error('Image load timeout')), 10000);
          });
        }

        return {
          complete: img.complete,
          naturalWidth: img.naturalWidth,
          naturalHeight: img.naturalHeight,
          src: img.src,
          // Try to fetch the image directly to check if it's accessible
          fetchable: await fetch(img.src).then(r => ({ ok: r.ok, status: r.status, contentType: r.headers.get('content-type') })).catch(e => ({ error: e.message }))
        };
      });

      console.log('User B image load status:', JSON.stringify(imageLoadStatus, null, 2));

      expect(imageLoadStatus.complete).toBe(true);
      expect(imageLoadStatus.naturalWidth).toBeGreaterThan(0);
      expect(imageLoadStatus.naturalHeight).toBeGreaterThan(0);

      console.log('SUCCESS: User B can see the image from User A\'s document!');

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('Image persists after document refresh', async ({ page }) => {
    setupPageErrorHandler(page);

    await page.goto('http://localhost:5173');
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

      try {
        const root = await navigator.storage.getDirectory();
        for await (const name of root.keys()) {
          await root.removeEntry(name, { recursive: true });
        }
      } catch {
        // OPFS might not be available
      }
    });

    await page.reload();
    await waitForAppReady(page); // Wait for page to load after reload
    await disableOthersPool(page);
    await configureBlossomServers(page);

    // Navigate to public folder
    const publicLink = page.getByRole('link', { name: 'public' }).first();
    await expect(publicLink).toBeVisible({ timeout: 30000 });
    await publicLink.click();
    await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 30000 });

    // Create document
    console.log('Creating document...');
    await createDocument(page, 'image-persist-test');

    // Type some text
    const editor = page.locator('.ProseMirror');
    await editor.click();
    await page.keyboard.type('Image persistence test: ');
    await waitForSave(page);

    // Insert image via toolbar button
    console.log('Inserting image...');
    const tmpFile = createTempPngFile();

    // Get the hidden file input and upload
    const fileInput = page.locator('input[type="file"][accept="image/*"]');
    await fileInput.setInputFiles(tmpFile);

    // Clean up temp file
    fs.unlinkSync(tmpFile);
    console.log('Image uploaded via file input');

    await waitForSave(page);

    // Verify image is visible
    const image = editor.locator('img');
    await expect(image).toBeVisible({ timeout: 10000 });
    const srcBefore = await image.getAttribute('src');
    console.log(`Image src before refresh: ${srcBefore}`);

    // Get current URL
    const docUrl = page.url();

    // Refresh the page
    console.log('Refreshing page...');
    await page.reload();
    await waitForAppReady(page);

    // Wait for editor to load
    await expect(editor).toBeVisible({ timeout: 30000 });
    await expect(editor).toContainText('Image persistence test', { timeout: 30000 });

    // Verify image is still visible after refresh
    const imageAfter = editor.locator('img');
    await expect(imageAfter).toBeVisible({ timeout: 30000 });

    const srcAfter = await imageAfter.getAttribute('src');
    console.log(`Image src after refresh: ${srcAfter}`);
    expect(srcAfter).toContain('/htree/');

    // Verify image actually loads
    const isLoaded = await imageAfter.evaluate((img: HTMLImageElement) => {
      return img.complete && img.naturalWidth > 0;
    });
    expect(isLoaded).toBe(true);

    console.log('SUCCESS: Image persists after refresh!');
  });
});
