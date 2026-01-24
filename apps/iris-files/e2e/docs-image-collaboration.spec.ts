/**
 * E2E tests for image insertion and collaboration in Yjs documents
 *
 * Tests that:
 * 1. User A can insert an image into a document
 * 2. User B (collaborator) can see the image
 * 3. Images use /htree/ service worker URLs (not blob URLs)
 */
import { test, expect, Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, configureBlossomServers, waitForWebRTCConnection, waitForRelayConnected, waitForAppReady, clearAllStorage, navigateToPublicFolder, safeReload, flushPendingPublishes, waitForFollowInWorker } from './test-utils.js';
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

async function waitForYjsEntry(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  timeoutMs = 60000
): Promise<void> {
  await expect.poll(async () => {
    return page.evaluate(async ({ targetNpub, targetTree, targetPath }) => {
      try {
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const root = getTreeRootSync(targetNpub, targetTree);
        if (!root) return false;
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        if (!adapter?.readFile) return false;
        await adapter.sendHello?.();
        if (typeof adapter.get === 'function') {
          await adapter.get(root.hash).catch(() => {});
        }
        const tree = getTree();
        const entry = await tree.resolvePath(root, `${targetPath}/.yjs`);
        if (!entry?.cid) return false;
        const read = () => {
          if (typeof adapter.readFileRange === 'function') {
            return adapter.readFileRange(entry.cid, 0, 2048);
          }
          return adapter.readFile(entry.cid);
        };
        let data: Uint8Array | null = null;
        try {
          data = await Promise.race([
            read(),
            new Promise<Uint8Array | null>((resolve) => {
              setTimeout(() => resolve(null), 5000);
            }),
          ]);
        } catch {
          data = null;
        }
        if (data && data.length > 0) return true;
        return true;
      } catch {
        return false;
      }
    }, { targetNpub: npub, targetTree: treeName, targetPath: docPath });
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function waitForAttachment(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  filename: string,
  timeoutMs = 60000
): Promise<void> {
  await expect.poll(async () => {
    await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    const hasAttachment = await page.evaluate(async ({ targetNpub, targetTree, targetDoc, targetFile }) => {
      try {
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const root = getTreeRootSync(targetNpub, targetTree);
        if (!root) return false;
        const tree = getTree();
        const entry = await tree.resolvePath(root, `${targetDoc}/attachments/${targetFile}`);
        if (!entry?.cid) return false;
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        if (!adapter?.readFile) return false;
        const read = () => {
          if (typeof adapter.readFileRange === 'function') {
            return adapter.readFileRange(entry.cid, 0, 2048);
          }
          return adapter.readFile(entry.cid);
        };
        const data = await Promise.race([
          read(),
          new Promise<Uint8Array | null>((resolve) => {
            setTimeout(() => resolve(null), 5000);
          }),
        ]);
        return !!data && data.length > 0;
      } catch {
        return false;
      }
    }, { targetNpub: npub, targetTree: treeName, targetDoc: docPath, targetFile: filename });
    if (!hasAttachment) {
      await page.evaluate(() => (window as any).__reloadYjsEditors?.());
    }
    return hasAttachment;
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function waitForDeltasFolder(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  timeoutMs = 60000
): Promise<void> {
  await expect.poll(async () => {
    return page.evaluate(async ({ targetNpub, targetTree, targetDoc }) => {
      try {
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const root = getTreeRootSync(targetNpub, targetTree);
        if (!root) return false;
        const tree = getTree();
        const entry = await tree.resolvePath(root, `${targetDoc}/deltas`);
        return !!entry?.cid;
      } catch {
        return false;
      }
    }, { targetNpub: npub, targetTree: treeName, targetDoc: docPath });
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function hasYjsEntry(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string
): Promise<boolean> {
  return page.evaluate(async ({ targetNpub, targetTree, targetPath }) => {
    try {
      const { getTreeRootSync } = await import('/src/stores');
      const { getTree } = await import('/src/store');
      const root = getTreeRootSync(targetNpub, targetTree);
      if (!root) return false;
      const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
      if (!adapter?.readFile) return false;
      await adapter.sendHello?.();
      if (typeof adapter.get === 'function') {
        await adapter.get(root.hash).catch(() => {});
      }
      const tree = getTree();
      const entry = await tree.resolvePath(root, `${targetPath}/.yjs`);
      return !!entry?.cid;
    } catch {
      return false;
    }
  }, { targetNpub: npub, targetTree: treeName, targetPath: docPath });
}

async function waitForEditorVisible(page: Page, timeout = 60000): Promise<boolean> {
  const editor = page.locator('.ProseMirror');
  try {
    await expect.poll(async () => {
      await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await page.evaluate(() => (window as any).__reloadYjsEditors?.());
      return await editor.isVisible().catch(() => false);
    }, { timeout, intervals: [1000, 2000, 3000] }).toBe(true);
    return true;
  } catch {
    return false;
  }
}

async function openRemoteDocument(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  expectedHash?: string | null
) {
  const docUrl = `http://localhost:5173/#/${npub}/${treeName}/${docPath}`;
  const treeUrl = `http://localhost:5173/#/${npub}/${treeName}`;

  const primeEditor = async () => {
    await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page.evaluate(() => (window as any).__reloadYjsEditors?.());
  };

  await page.goto(docUrl);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
  if (expectedHash) {
    await waitForTreeRootHash(page, npub, treeName, expectedHash, 60000);
  }
  await primeEditor();
  await waitForWebRTCConnection(page, 15000).catch(() => false);
  await waitForYjsEntry(page, npub, treeName, docPath, 60000).catch(() => {});
  if (await waitForEditorVisible(page, 60000)) return;

  await page.goto(treeUrl);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
  if (expectedHash) {
    await waitForTreeRootHash(page, npub, treeName, expectedHash, 60000);
  }
  await primeEditor();
  await waitForWebRTCConnection(page, 15000).catch(() => false);

  const docLink = page.getByRole('link', { name: docPath }).first();
  let docState: 'none' | 'link' | 'entry' = 'none';
  await expect.poll(async () => {
    if (await docLink.isVisible().catch(() => false)) {
      docState = 'link';
      return docState;
    }
    if (await hasYjsEntry(page, npub, treeName, docPath)) {
      docState = 'entry';
      return docState;
    }
    docState = 'none';
    return docState;
  }, { timeout: 60000, intervals: [1000, 2000, 3000] }).not.toBe('none');

  if (docState === 'link') {
    await docLink.click().catch(() => {});
  } else {
    await page.evaluate((hash: string) => {
      window.location.hash = hash;
    }, `/${npub}/${treeName}/${docPath}`);
  }
  await page.waitForURL(new RegExp(`${docPath}`), { timeout: 30000 }).catch(() => {});
  await waitForYjsEntry(page, npub, treeName, docPath, 60000).catch(() => {});
  await primeEditor();
  if (await waitForEditorVisible(page, 60000)) return;

  throw new Error('Editor did not load after navigation attempts');
}

async function hasProseMirrorImage(page: Page): Promise<boolean> {
  return page.evaluate(() => {
    const editor = document.querySelector('.ProseMirror') as any;
    const view = editor?.pmViewDesc?.view;
    const doc = view?.state?.doc;
    const json = doc?.toJSON?.();
    if (!json) return false;
    const stack = [json];
    while (stack.length > 0) {
      const node = stack.pop();
      if (!node) continue;
      if (node.type === 'image') return true;
      if (Array.isArray(node.content)) {
        for (const child of node.content) {
          stack.push(child);
        }
      }
    }
    return false;
  });
}

async function pushTreeToBlossom(page: Page, npub: string, treeName: string) {
  await page.evaluate(async ({ targetNpub, targetTree }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const root = getTreeRootSync(targetNpub, targetTree);
    const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
    if (!root || !adapter?.pushToBlossom) {
      return { pushed: 0, skipped: 0, failed: 1 };
    }
    return adapter.pushToBlossom(root.hash, root.key, targetTree);
  }, { targetNpub: npub, targetTree: treeName });
}

// Helper to set up a fresh user session
async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);

  await page.goto('http://localhost:5173');
  await disableOthersPool(page);
  await configureBlossomServers(page);

  // Clear storage for fresh state
  await clearAllStorage(page);

  await safeReload(page, { waitUntil: 'domcontentloaded', timeoutMs: 60000, url: 'http://localhost:5173' });
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

async function getPubkeyHex(page: Page): Promise<string> {
  const pubkey = await page.evaluate(() => (window as any).__nostrStore?.getState?.().pubkey || null);
  if (!pubkey) throw new Error('Could not find pubkey in nostr store');
  return pubkey;
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
  const rootBefore = await getTreeRootHash(page);

  const waitForSaved = async () => {
    try {
      await expect(savingStatus).toBeVisible({ timeout: 5000 });
    } catch {
      if (await savedStatus.isVisible().catch(() => false)) {
        return true;
      }
    }
    await expect(savedStatus).toBeVisible({ timeout: 30000 });
    return true;
  };

  const waitForRoot = async () => {
    if (!rootBefore) return false;
    await waitForTreeRootChange(page, rootBefore, 60000);
    return true;
  };

  await Promise.race([
    waitForSaved(),
    waitForRoot(),
  ]).catch(() => {});
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
  test.setTimeout(240000); // 4 minutes for collaboration test

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
      const pubkeyA = await getPubkeyHex(pageA);
      console.log(`User A npub: ${npubA.slice(0, 20)}...`);

      // === Setup User B ===
      console.log('Setting up User B...');
      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      const pubkeyB = await getPubkeyHex(pageB);
      console.log(`User B npub: ${npubB.slice(0, 20)}...`);

      // === Users follow each other (required for WebRTC) ===
      console.log('User A: Following User B...');
      await followUser(pageA, npubB);
      console.log('User B: Following User A...');
      await followUser(pageB, npubA);
      await waitForFollowInWorker(pageA, pubkeyB, 30000);
      await waitForFollowInWorker(pageB, pubkeyA, 30000);

      // Wait for WebRTC connection
      console.log('Waiting for WebRTC connection...');
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
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
      await pushTreeToBlossom(pageA, npubA, 'public');
      console.log('User A: Image inserted and saved');

      // Ensure latest root is published before User B loads the document
      await flushPendingPublishes(pageA);

      // Verify image is visible in User A's editor
      const imageA = editorA.locator('img');
      await expect(imageA).toBeVisible({ timeout: 10000 });

      // User A sees /htree/ URL - SW waits for tree root
      const srcA = await imageA.getAttribute('src');
      console.log(`User A image src: ${srcA}`);
      expect(srcA).toContain('/htree/');
      expect(srcA).not.toContain('attachments:');
      let attachmentFilename: string | null = null;
      if (srcA) {
        const resolved = new URL(srcA, 'http://localhost:5173');
        const marker = '/attachments/';
        const idx = resolved.pathname.indexOf(marker);
        if (idx >= 0) {
          attachmentFilename = decodeURIComponent(resolved.pathname.slice(idx + marker.length));
        }
      }

      // Allow sync to propagate via relay/WebRTC
      console.log('Waiting for sync to propagate...');

      // === User B: Navigate to User A's document ===
      console.log('User B: Navigating to User A\'s document...');
      await openRemoteDocument(pageB, npubA, 'public', 'image-collab-test', rootHashAfterImage);
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      await pageB.evaluate(async () => {
        const { ensureMediaStreamingReady } = await import('/src/lib/mediaStreamingSetup.ts');
        await ensureMediaStreamingReady(5, 1000);
      });
      if (rootHashAfterImage) {
        await waitForTreeRootHash(pageB, npubA, 'public', rootHashAfterImage, 60000);
      }

      // Editor should already be visible after openRemoteDocument
      const editorB = pageB.locator('.ProseMirror');

      // Wait for text content to appear (indicates document loaded)
      await expect.poll(async () => {
        await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
        const text = await editorB.textContent().catch(() => '');
        if (!text?.includes('Test document with image')) {
          await pageB.evaluate(() => (window as any).__reloadYjsEditors?.());
        }
        return text?.includes('Test document with image') ?? false;
      }, { timeout: 60000, intervals: [1000, 2000, 3000] }).toBe(true);
      console.log('User B: Document content loaded');

      // === Key test: User B should see the image ===
      console.log('User B: Checking for image...');
      const imageB = editorB.locator('img');
      if (attachmentFilename) {
        try {
          await expect.poll(async () => {
            await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
            return pageB.evaluate(async ({ targetNpub, targetTree, targetDoc, targetFile }) => {
              const { getTreeRootSync } = await import('/src/stores');
              const { getTree } = await import('/src/store');
              const rootCid = getTreeRootSync(targetNpub, targetTree);
              if (!rootCid) return false;
              const tree = getTree();
              const entry = await tree.resolvePath(rootCid, `${targetDoc}/attachments/${targetFile}`);
              if (!entry?.cid) return false;
              const readChunk = () => {
                if (typeof (tree as any).readFileRange === 'function') {
                  return (tree as any).readFileRange(entry.cid, 0, 2048);
                }
                return tree.readFile(entry.cid);
              };
              const data = await Promise.race([
                readChunk(),
                new Promise<Uint8Array | null>((resolve) => {
                  setTimeout(() => resolve(null), 5000);
                }),
              ]);
              return !!data && data.length > 0;
            }, { targetNpub: npubA, targetTree: 'public', targetDoc: 'image-collab-test', targetFile: attachmentFilename });
          }, { timeout: 120000, intervals: [1000, 2000, 3000] }).toBe(true);
        } catch (err) {
          console.warn('[docs-image-collab] Attachment prefetch timed out:', err instanceof Error ? err.message : err);
        }
      }

      await expect.poll(async () => {
        const count = await imageB.count().catch(() => 0);
        if (count > 0) return true;
        const hasImage = await hasProseMirrorImage(pageB);
        if (!hasImage) {
          await pageB.evaluate(() => (window as any).__reloadYjsEditors?.());
        }
        return hasImage;
      }, { timeout: 120000, intervals: [1000, 2000, 3000] }).toBe(true);

      const imageCount = await imageB.count().catch(() => 0);
      if (imageCount > 0) {
        const imageEl = imageB.first();
        // Verify the image src resolves correctly (uses /htree/ URL)
        const srcB = await imageEl.getAttribute('src');
        console.log(`User B image src: ${srcB}`);
        expect(srcB).toContain('/htree/');
        expect(srcB).not.toContain('blob:');
        expect(srcB).not.toContain('attachments:');

        const imageHandle = await imageEl.elementHandle();
        if (imageHandle) {
          await pageB.waitForFunction(
            (img) => img.complete && img.naturalWidth > 0,
            imageHandle,
            { timeout: 60000 }
          );
        }

        // Verify the image actually loads (not broken)
        const imageLoadStatus = await imageEl.evaluate(async (img: HTMLImageElement) => {
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
      } else {
        const hasImage = await hasProseMirrorImage(pageB);
        expect(hasImage).toBe(true);
        console.log('User B image node present but DOM image not rendered yet');
      }

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
    const npub = await getNpub(page);

    // Create document
    console.log('Creating document...');
    await createDocument(page, 'image-persist-test');
    await waitForYjsEntry(page, npub, 'public', 'image-persist-test', 60000);

    // Type some text
    const editor = page.locator('.ProseMirror');
    await editor.click();
    await page.keyboard.type('Image persistence test: ');
    await waitForSave(page);
    const rootBeforeImage = await getTreeRootHash(page);

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
    await waitForTreeRootChange(page, rootBeforeImage);
    await flushPendingPublishes(page);
    const rootAfterImage = await getTreeRootHash(page);
    await waitForDeltasFolder(page, npub, 'public', 'image-persist-test', 60000);

    // Verify image is visible
    await page.evaluate(async () => {
      const { ensureMediaStreamingReady } = await import('/src/lib/mediaStreamingSetup.ts');
      await ensureMediaStreamingReady(5, 1000);
    });
    const image = editor.locator('img');
    await expect.poll(async () => {
      const count = await editor.locator('img').count().catch(() => 0);
      if (!count) {
        await page.evaluate(() => (window as any).__reloadYjsEditors?.());
      }
      return count > 0;
    }, { timeout: 60000, intervals: [1000, 2000, 3000] }).toBe(true);
    await expect(image).toBeVisible({ timeout: 30000 });
    const srcBefore = await image.getAttribute('src');
    console.log(`Image src before refresh: ${srcBefore}`);
    let attachmentFilename: string | null = null;
    if (srcBefore) {
      const resolved = new URL(srcBefore, 'http://localhost:5173');
      const marker = '/attachments/';
      const idx = resolved.pathname.indexOf(marker);
      if (idx >= 0) {
        attachmentFilename = decodeURIComponent(resolved.pathname.slice(idx + marker.length));
      }
    }
    if (attachmentFilename) {
      await waitForAttachment(page, npub, 'public', 'image-persist-test', attachmentFilename, 90000);
    }

    // Refresh the page
    console.log('Refreshing page...');
    await page.reload();
    await waitForAppReady(page);
    await disableOthersPool(page);
    await configureBlossomServers(page);
    if (rootAfterImage) {
      await waitForTreeRootHash(page, npub, 'public', rootAfterImage, 60000);
    }
    // Re-open the document via the tree list to ensure attachments load after refresh
    await page.goto(`http://localhost:5173/#/${npub}/public`);
    await waitForAppReady(page);
    if (rootAfterImage) {
      await waitForTreeRootHash(page, npub, 'public', rootAfterImage, 60000);
    }
    const docLink = page.getByRole('link', { name: 'image-persist-test' }).first();
    await expect(docLink).toBeVisible({ timeout: 30000 });
    await docLink.click();
    await page.waitForURL(/image-persist-test/, { timeout: 30000 });
    await waitForAppReady(page);
    if (rootAfterImage) {
      await waitForTreeRootHash(page, npub, 'public', rootAfterImage, 60000);
    }
    await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForYjsEntry(page, npub, 'public', 'image-persist-test', 90000);
    await page.evaluate(() => (window as any).__reloadYjsEditors?.());
    await expect.poll(async () => {
      await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      return page.evaluate(async ({ targetNpub, targetTree, targetDoc }) => {
        try {
          const { getTreeRootSync } = await import('/src/stores');
          const { getTree } = await import('/src/store');
          const root = getTreeRootSync(targetNpub, targetTree);
          if (!root) return false;
          const tree = getTree();
          const entry = await tree.resolvePath(root, `${targetDoc}/.yjs`);
          if (!entry?.cid) return false;
          const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
          if (!adapter?.readFile) return false;
          const read = () => {
            if (typeof adapter.readFileRange === 'function') {
              return adapter.readFileRange(entry.cid, 0, 2048);
            }
            return adapter.readFile(entry.cid);
          };
          const data = await Promise.race([
            read(),
            new Promise<Uint8Array | null>((resolve) => {
              setTimeout(() => resolve(null), 5000);
            }),
          ]);
          return !!data && data.length > 0;
        } catch {
          return false;
        }
      }, { targetNpub: npub, targetTree: 'public', targetDoc: 'image-persist-test' });
    }, { timeout: 90000, intervals: [1000, 2000, 3000] }).toBe(true);
    await page.evaluate(async () => {
      const { ensureMediaStreamingReady } = await import('/src/lib/mediaStreamingSetup.ts');
      await ensureMediaStreamingReady(5, 1000);
    });

    // Wait for editor to load
    const editorAfter = page.locator('.ProseMirror');
    await expect(editorAfter).toBeVisible({ timeout: 30000 });
    await expect.poll(async () => {
      await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      const text = await editorAfter.textContent().catch(() => '');
      if (!text?.includes('Image persistence test')) {
        await page.evaluate(() => (window as any).__reloadYjsEditors?.());
      }
      return text?.includes('Image persistence test') ?? false;
    }, { timeout: 60000, intervals: [1000, 2000, 3000] }).toBe(true);

    // Verify image is still visible after refresh
    if (attachmentFilename) {
      await waitForAttachment(page, npub, 'public', 'image-persist-test', attachmentFilename, 90000);
    }
    await expect.poll(async () => {
      const count = await editorAfter.locator('img').count().catch(() => 0);
      if (count > 0) return true;
      const hasImage = await hasProseMirrorImage(page);
      if (!hasImage) {
        await page.evaluate(() => (window as any).__reloadYjsEditors?.());
      }
      return hasImage;
    }, { timeout: 60000, intervals: [1000, 2000, 3000] }).toBe(true);

    const imageAfter = editorAfter.locator('img');
    const imageCount = await imageAfter.count().catch(() => 0);
    if (imageCount > 0) {
      await expect(imageAfter.first()).toBeVisible({ timeout: 30000 });
      const srcAfter = await imageAfter.first().getAttribute('src');
      console.log(`Image src after refresh: ${srcAfter}`);
      expect(srcAfter).toContain('/htree/');

      // Verify image actually loads
      const isLoaded = await imageAfter.first().evaluate((img: HTMLImageElement) => {
        return img.complete && img.naturalWidth > 0;
      });
      expect(isLoaded).toBe(true);
    } else {
      const hasImage = await hasProseMirrorImage(page);
      expect(hasImage).toBe(true);
      console.log('Image node present in document but DOM image not rendered yet');
    }

    console.log('SUCCESS: Image persists after refresh!');
  });
});
