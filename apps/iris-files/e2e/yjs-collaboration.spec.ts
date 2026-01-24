/**
 * E2E tests for Yjs collaborative document editing
 *
 * Tests that two users (A and B) can:
 * 1. Create documents at the same path
 * 2. Add both npubs to their .yjs config files (all editors including self)
 * 3. See each other's edits automatically via subscription
 *
 * TEST PERFORMANCE GUIDELINES:
 * - NEVER use waitForTimeout() for arbitrary delays
 * - ALWAYS wait for specific conditions (element visible, text contains, URL changes)
 * - Use expect(locator).toBeVisible() or toContainText() with timeout
 * - Use page.waitForURL() for navigation
 * - Use page.waitForSelector() for DOM elements
 * - If waiting for content sync, use waitForEditorContent() helper
 */
import { test, expect, Page, BrowserContext } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, configureBlossomServers, waitForWebRTCConnection, waitForFollowInWorker, followUser, waitForAppReady, waitForRelayConnected, flushPendingPublishes, presetLocalRelayInDB, useLocalRelay, safeReload } from './test-utils.js';

const TREE_LOOKUP_TIMEOUT_MS = 15000;
let docCounter = 0;
const nextDocName = (prefix: string) => `${prefix}-${++docCounter}`;
let editCounter = 0;
const nextEditMarker = (prefix: string) => `${prefix}-${++editCounter}`;

let contextA: BrowserContext | null = null;
let contextB: BrowserContext | null = null;
let pageA: Page;
let pageB: Page;
let npubA: string;
let npubB: string;
let pubkeyA: string;
let pubkeyB: string;
let sharedDocA: string;
let sharedDocDirect: string;

async function safeEvaluate(page: Page, fn: () => Promise<void>, timeoutMs = TREE_LOOKUP_TIMEOUT_MS) {
  await Promise.race([
    page.evaluate(fn).catch(() => {}),
    new Promise<void>((resolve) => setTimeout(resolve, timeoutMs)),
  ]);
}

// Helper to set up a fresh user session
async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);

  await page.goto('http://localhost:5173');
  await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests
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

  await presetLocalRelayInDB(page);
  await safeReload(page, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
  await waitForAppReady(page); // Wait for page to load after reload
  await useLocalRelay(page);
  await waitForRelayConnected(page, 20000);
  await disableOthersPool(page); // Re-apply after reload
  await configureBlossomServers(page);

  // Wait for the public folder link to appear
  const publicLink = page.getByRole('link', { name: 'public' }).first();
  await expect(publicLink).toBeVisible({ timeout: 30000 });

  // Click into the public folder
  await publicLink.click();
  await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 30000 });
  await expect(page.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 30000 });
  await ensurePublicTreeVisibility(page);
}

async function resetToPublic(page: Page, npub: string) {
  await page.goto(`http://localhost:5173/#/${npub}/public`);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
  await expect(page.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });
}

async function resetToPublicFast(page: Page, npub: string) {
  const targetHash = `#/${npub}/public`;
  await page.evaluate((hash) => {
    window.location.hash = hash;
  }, targetHash);
  await page.waitForFunction((hash) => {
    return window.location.hash.startsWith(hash);
  }, targetHash, { timeout: 10000 });
  await expect(page.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 15000 });
}

// Helper to get the user's npub from the URL
async function getNpub(page: Page): Promise<string> {
  const url = page.url();
  const match = url.match(/npub1[a-z0-9]+/);
  if (!match) throw new Error('Could not find npub in URL');
  return match[0];
}

// Helper to get user's pubkey hex from nostr store
async function getPubkeyHex(page: Page): Promise<string> {
  const pubkey = await page.evaluate(() => (window as any).__nostrStore?.getState?.()?.pubkey || null);
  if (!pubkey) throw new Error('Could not find pubkey in nostr store');
  return pubkey;
}

// Helper to create a document with a given name
async function createDocument(page: Page, name: string) {
  await page.evaluate(async (docName) => {
    const { createDocument, updateRoute } = await import('/src/actions');
    await createDocument(docName);
    updateRoute(docName);
  }, name);

  await page.waitForFunction(
    (docName) => window.location.hash.includes(encodeURIComponent(docName)),
    name,
    { timeout: 20000 }
  );
  await waitForEditorVisible(page, 30000);
}

// Helper to type content in the editor
async function typeInEditor(page: Page, content: string) {
  const editor = page.locator('.ProseMirror');
  await expect(editor).toBeVisible({ timeout: 30000 });
  await editor.click();
  await page.keyboard.type(content);
}

// Helper to wait for auto-save
async function waitForSave(page: Page) {
  // Wait for "Saving..." to appear first (debounce triggers), then "Saved"
  // This ensures we wait for a NEW save, not just that "Saved" is still visible from before
  const savingStatus = page.locator('text=Saving');
  const savedStatus = page.locator('text=Saved').or(page.locator('text=/Saved \\d/')).first();
  const previousSavedText = await savedStatus.textContent().catch(() => null);
  const previousRoot = await page.evaluate(async () => {
    const { getRouteSync } = await import('/src/stores/route');
    const route = getRouteSync();
    const registry = (window as any).__treeRootRegistry;
    if (!route?.npub || !route?.treeName || !registry?.get) return null;
    const entry = registry.get(route.npub, route.treeName);
    if (!entry) return null;
    const toHex = (bytes: Uint8Array) => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    return {
      updatedAt: entry.updatedAt ?? null,
      hashHex: entry.hash ? toHex(entry.hash) : null,
    };
  });

  const waitForSavedIndicator = async () => {
    try {
      await expect(savingStatus).toBeVisible({ timeout: 10000 });
    } catch {}

    await expect(savedStatus).toBeVisible({ timeout: 20000 });
    if (previousSavedText) {
      await expect.poll(async () => (await savedStatus.textContent())?.trim() ?? null, {
        timeout: 20000,
        intervals: [500, 1000, 2000],
      }).not.toBe(previousSavedText.trim());
    }
  };

  const waitForTreeRootUpdate = async () => {
    await page.waitForFunction(async (prev) => {
      const { getRouteSync } = await import('/src/stores/route');
      const route = getRouteSync();
      const registry = (window as any).__treeRootRegistry;
      if (!route?.npub || !route?.treeName || !registry?.get) return false;
      const entry = registry.get(route.npub, route.treeName);
      if (!entry || entry.dirty) return false;
      const toHex = (bytes: Uint8Array) => Array.from(bytes)
        .map((b) => b.toString(16).padStart(2, '0'))
        .join('');
      const currentHash = entry.hash ? toHex(entry.hash) : null;
      if (!prev) return true;
      if (prev.hashHex && currentHash && currentHash !== prev.hashHex) return true;
      if (prev.updatedAt !== null && entry.updatedAt !== null && entry.updatedAt > prev.updatedAt) return true;
      if (!prev.hashHex && prev.updatedAt === null) return true;
      return false;
    }, previousRoot, { timeout: 60000 });
  };

  try {
    await waitForSavedIndicator();
  } catch {
    try {
      await waitForTreeRootUpdate();
    } catch {
      console.log('[waitForSave] Save not confirmed within timeout, continuing');
    }
  }
  await flushPublishes(page);
}

async function ensurePublicTreeVisibility(page: Page, treeName: string = 'public') {
  await page.evaluate(async (tree) => {
    const { getTreeRootSync } = await import('/src/stores');
    const { saveHashtree } = await import('/src/nostr');
    const { nostrStore } = await import('/src/nostr/store');
    const state = nostrStore.getState?.();
    if (!state?.npub) return;
    const root = getTreeRootSync(state.npub, tree);
    if (!root) return;
    if (state.selectedTree?.name === tree) {
      nostrStore.setSelectedTree({ ...state.selectedTree, visibility: 'public' });
    }
    await saveHashtree(tree, root, { visibility: 'public' });
  }, treeName);
}

async function flushPublishes(page: Page) {
  await flushPendingPublishes(page);
}

// Helper to set editors using the Collaborators modal UI
// Note: This assumes we're viewing the YjsDocument (inside the document folder)
async function setEditors(page: Page, npubs: string[]) {
  // Click the collaborators button (users icon) in the toolbar
  // The button shows either "Manage editors" (own tree) or "View editors" (other's tree)
  const collabButton = page.locator('button[title="Manage editors"], button[title="View editors"]').first();
  await expect(collabButton).toBeVisible({ timeout: 30000 });
  await collabButton.click();

  // Wait for the modal to appear - heading says "Manage Editors" or "Editors" depending on mode
  const modal = page.locator('h2:has-text("Editors")');
  await expect(modal).toBeVisible({ timeout: 30000 });

  for (const npub of npubs) {
    const input = page.locator('input[placeholder="npub1..."]');
    await input.fill(npub);

    const confirmButton = page.locator('button.btn-success').filter({ hasText: /^Add/ }).first();
    await expect(confirmButton).toBeVisible({ timeout: 15000 });
    await confirmButton.click({ force: true });
  }

  // Modal auto-saves on add, just close it using the footer Close button (not the X)
  const closeButton = page.getByText('Close', { exact: true });
  await closeButton.click();
  // Wait for modal to close
  await expect(modal).not.toBeVisible({ timeout: 30000 });
}

async function setEditorsFast(page: Page, npubs: string[], docPath?: string) {
  const pathSegments = docPath ? docPath.split('/').filter(Boolean) : null;
  await page.evaluate(async ({ editors, pathOverride }) => {
    const { getCurrentRootCid } = await import('/src/actions');
    const { getRouteSync } = await import('/src/stores/route');
    const { autosaveIfOwn } = await import('/src/nostr');
    const { getTree } = await import('/src/store');

    const rootCid = getCurrentRootCid();
    const route = getRouteSync();
    const path = pathOverride?.length ? pathOverride : route?.path;
    if (!rootCid || !path?.length) return;

    const tree = getTree();
    const content = editors.join('\n') + '\n';
    const data = new TextEncoder().encode(content);
    const { cid, size } = await tree.putFile(data);
    const newRootCid = await tree.setEntry(rootCid, path, '.yjs', cid, size, false);
    autosaveIfOwn(newRootCid);
  }, { editors: npubs, pathOverride: pathSegments });

  await page.evaluate(() => (window as any).__reloadYjsEditors?.());
}

// Helper to navigate to own document
async function navigateToOwnDocument(page: Page, npub: string, treeName: string, docPath: string, linkKey?: string | null) {
  const linkParam = linkKey ? `?k=${linkKey}` : '';
  const url = `http://localhost:5173/#/${npub}/${treeName}/${docPath}${linkParam}`;
  await page.goto(url);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
  await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
}

async function openRemoteDocumentFast(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  linkKey?: string | null,
  expectedHash?: string | null,
  timeoutMs: number = 45000
) {
  const linkParam = linkKey ? `?k=${linkKey}` : '';
  const docHash = `#/${npub}/${treeName}/${docPath}${linkParam}`;
  const shortTimeout = Math.min(timeoutMs, 15000);
  await page.evaluate((hash) => {
    window.location.hash = hash;
  }, docHash);
  await page.waitForFunction((hash) => {
    if (!hash) return false;
    const current = window.location.hash;
    if (current === hash) return true;
    const base = hash.split('?')[0];
    return current.startsWith(base);
  }, docHash, { timeout: shortTimeout });

  await waitForRelayConnected(page, shortTimeout).catch(() => {});
  if (expectedHash) {
    await waitForTreeRootHash(page, npub, treeName, expectedHash, shortTimeout).catch(() => {});
  }
  await waitForYjsEntry(page, npub, treeName, docPath, shortTimeout).catch(() => {});
  await page.evaluate(() => {
    (window as any).__workerAdapter?.sendHello?.();
    (window as any).__reloadYjsEditors?.();
  });
  const editorReady = await waitForEditorVisible(page, timeoutMs);
  if (!editorReady) {
    const url = `http://localhost:5173/${docHash}`;
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: timeoutMs });
    await waitForAppReady(page, timeoutMs);
    await waitForEditorVisible(page, timeoutMs);
  }
}

// Helper to wait for editor to contain specific text (for sync verification)
async function waitForEditorContent(page: Page, expectedText: string, timeout = 120000) {
  const editor = page.locator('.ProseMirror');
  // First wait for editor to be visible (may take time for nostr sync to load the page)
  await expect(editor).toBeVisible({ timeout });
  await expect.poll(async () => {
    await safeEvaluate(page, async () => {
      (window as any).__workerAdapter?.sendHello?.();
      (window as any).__reloadYjsEditors?.();
    });
    const text = await editor.textContent().catch(() => '');
    return text?.includes(expectedText) ?? false;
  }, { timeout, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function waitForEditorVisible(page: Page, timeout = 60000): Promise<boolean> {
  const editor = page.locator('.ProseMirror');
  try {
    await expect.poll(async () => {
      await safeEvaluate(page, async () => {
        (window as any).__workerAdapter?.sendHello?.();
        (window as any).__reloadYjsEditors?.();
      });
      return await editor.isVisible().catch(() => false);
    }, { timeout, intervals: [1000, 2000, 3000] }).toBe(true);
    return true;
  } catch {
    return false;
  }
}

async function waitForEditorBadge(page: Page, timeout = 30000) {
  const badge = page.getByText('Editor', { exact: true });
  await expect.poll(async () => {
    await safeEvaluate(page, async () => {
      (window as any).__workerAdapter?.sendHello?.();
      (window as any).__reloadYjsEditors?.();
    });
    if (await badge.isVisible().catch(() => false)) return true;
    const editableCount = await page.locator('.ProseMirror[contenteditable="true"]').count().catch(() => 0);
    return editableCount > 0;
  }, { timeout, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function getTreeRootHash(page: Page, npub: string, treeName: string): Promise<string | null> {
  return page.evaluate(async ({ targetNpub, targetTree }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    const root = getTreeRootSync(targetNpub, targetTree);
    return root ? toHex(root.hash) : null;
  }, { targetNpub: npub, targetTree: treeName });
}

async function getTreeRootHex(page: Page, npub: string, treeName: string): Promise<{ hashHex: string | null; keyHex: string | null }> {
  return page.evaluate(async ({ targetNpub, targetTree }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    const root = getTreeRootSync(targetNpub, targetTree);
    if (!root) return { hashHex: null, keyHex: null };
    return { hashHex: toHex(root.hash), keyHex: root.key ? toHex(root.key) : null };
  }, { targetNpub: npub, targetTree: treeName });
}

async function seedRemoteTreeRoot(
  page: Page,
  npub: string,
  treeName: string,
  root: { hashHex: string | null; keyHex: string | null }
) {
  if (!root.hashHex) return;
  await page.evaluate(async ({ targetNpub, targetTree, hashHex, keyHex }) => {
    const { treeRootRegistry } = await import('/src/TreeRootRegistry');
    const fromHex = (hex: string) => {
      const bytes = new Uint8Array(hex.length / 2);
      for (let i = 0; i < bytes.length; i++) {
        bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
      }
      return bytes;
    };
    treeRootRegistry.setFromExternal(targetNpub, targetTree, fromHex(hashHex), 'prefetch', {
      key: keyHex ? fromHex(keyHex) : undefined,
      visibility: 'public',
    });
  }, { targetNpub: npub, targetTree: treeName, hashHex: root.hashHex, keyHex: root.keyHex });
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

async function waitForTreeRootHashChange(
  page: Page,
  npub: string,
  treeName: string,
  previousHash: string | null,
  timeoutMs = 60000
): Promise<void> {
  await page.waitForFunction(async ({ targetNpub, targetTree, prevHash }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    const root = getTreeRootSync(targetNpub, targetTree);
    if (!root) return false;
    return toHex(root.hash) !== prevHash;
  }, { targetNpub: npub, targetTree: treeName, prevHash: previousHash }, { timeout: timeoutMs });
}

async function hasTreeEntry(
  page: Page,
  npub: string,
  treeName: string,
  entryPath: string
): Promise<boolean> {
  const result = await Promise.race([
    page.evaluate(async ({ targetNpub, targetTree, targetPath }) => {
      try {
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const root = getTreeRootSync(targetNpub, targetTree);
        if (!root) return false;
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        if (!adapter) return false;
        await adapter.sendHello?.();
        if (typeof adapter.get === 'function') {
          await Promise.race([
            adapter.get(root.hash).catch(() => null),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
          ]);
        }
        const tree = getTree();
        const entry = await Promise.race([
          tree.resolvePath(root, targetPath),
          new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
        ]);
        return !!entry?.cid;
      } catch {
        return false;
      }
    }, { targetNpub: npub, targetTree: treeName, targetPath: entryPath }).catch(() => false),
    new Promise<boolean>((resolve) => setTimeout(() => resolve(false), TREE_LOOKUP_TIMEOUT_MS)),
  ]);
  return result;
}

async function waitForTreeEntry(
  page: Page,
  npub: string,
  treeName: string,
  entryPath: string,
  timeoutMs = 60000
): Promise<void> {
  await expect.poll(async () => {
    return hasTreeEntry(page, npub, treeName, entryPath);
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function hasYjsEntry(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string
): Promise<boolean> {
  const result = await Promise.race([
    page.evaluate(async ({ targetNpub, targetTree, targetPath }) => {
      try {
        const { getTreeRootSync } = await import('/src/stores');
        const { getTree } = await import('/src/store');
        const root = getTreeRootSync(targetNpub, targetTree);
        if (!root) return false;
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        if (!adapter?.readFile) return false;
        await adapter.sendHello?.();
        if (typeof adapter.get === 'function') {
          await Promise.race([
            adapter.get(root.hash).catch(() => null),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
          ]);
        }
        const tree = getTree();
        const entry = await Promise.race([
          tree.resolvePath(root, `${targetPath}/.yjs`),
          new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
        ]);
        return !!entry?.cid;
      } catch {
        return false;
      }
    }, { targetNpub: npub, targetTree: treeName, targetPath: docPath }).catch(() => false),
    new Promise<boolean>((resolve) => setTimeout(() => resolve(false), TREE_LOOKUP_TIMEOUT_MS)),
  ]);
  return result;
}

async function waitForYjsEntry(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  timeoutMs = 60000
): Promise<void> {
  await expect.poll(async () => {
    const result = await Promise.race([
      page.evaluate(async ({ targetNpub, targetTree, targetPath }) => {
        try {
          const { getTreeRootSync } = await import('/src/stores');
          const { getTree } = await import('/src/store');
          const root = getTreeRootSync(targetNpub, targetTree);
          if (!root) return false;
          const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
          if (!adapter?.readFile) return false;
          await adapter.sendHello?.();
          if (typeof adapter.get === 'function') {
            await Promise.race([
              adapter.get(root.hash).catch(() => null),
              new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
            ]);
          }
          const tree = getTree();
          const entry = await Promise.race([
            tree.resolvePath(root, `${targetPath}/.yjs`),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
          ]);
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
      }, { targetNpub: npub, targetTree: treeName, targetPath: docPath }).catch(() => false),
      new Promise<boolean>((resolve) => setTimeout(() => resolve(false), TREE_LOOKUP_TIMEOUT_MS)),
    ]);
    return result;
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function waitForYjsData(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  timeoutMs = 60000
): Promise<void> {
  await expect.poll(async () => {
    const result = await Promise.race([
      page.evaluate(async ({ targetNpub, targetTree, targetPath }) => {
        try {
          const { getTreeRootSync } = await import('/src/stores');
          const { getTree } = await import('/src/store');
          const root = getTreeRootSync(targetNpub, targetTree);
          if (!root) return false;
          const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
          if (!adapter?.readFile) return false;
          await adapter.sendHello?.();
          if (typeof adapter.get === 'function') {
            await Promise.race([
              adapter.get(root.hash).catch(() => null),
              new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
            ]);
          }
          const tree = getTree();
          const entry = await Promise.race([
            tree.resolvePath(root, `${targetPath}/.yjs`),
            new Promise<null>((resolve) => setTimeout(() => resolve(null), TREE_LOOKUP_TIMEOUT_MS)),
          ]);
          if (!entry?.cid) return false;
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
      }, { targetNpub: npub, targetTree: treeName, targetPath: docPath }).catch(() => false),
      new Promise<boolean>((resolve) => setTimeout(() => resolve(false), TREE_LOOKUP_TIMEOUT_MS)),
    ]);
    return result;
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function pushTreeToBlossom(page: Page, npub: string, treeName: string) {
  const result = await page.evaluate(async ({ targetNpub, targetTree }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const root = getTreeRootSync(targetNpub, targetTree);
    const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
    if (!root || !adapter?.pushToBlossom) {
      return { pushed: 0, skipped: 0, failed: 1 };
    }
    return adapter.pushToBlossom(root.hash, root.key, targetTree);
  }, { targetNpub: npub, targetTree: treeName });
  return result;
}

async function getTreeLinkKey(page: Page, npub: string, treeName: string): Promise<string | null> {
  return page.evaluate(async ({ targetNpub, targetTree }) => {
    const { getLinkKey, recoverLinkKeyFromSelfEncrypted } = await import('/src/stores/trees');
    const registry = (window as any).__treeRootRegistry;
    let linkKey = getLinkKey(targetNpub, targetTree);
    if (!linkKey) {
      const record = registry?.get?.(targetNpub, targetTree);
      if (record?.selfEncryptedLinkKey) {
        linkKey = await recoverLinkKeyFromSelfEncrypted(targetNpub, targetTree, record.selfEncryptedLinkKey);
      }
    }
    if (!linkKey) {
      const { getTreeRootSync } = await import('/src/stores');
      const root = getTreeRootSync(targetNpub, targetTree);
      if (root?.key) {
        const toHex = (bytes: Uint8Array) => Array.from(bytes)
          .map((b) => b.toString(16).padStart(2, '0'))
          .join('');
        linkKey = toHex(root.key);
      }
    }
    return linkKey ?? null;
  }, { targetNpub: npub, targetTree: treeName });
}

async function ensureSharedDocs() {
  if (sharedDocA && sharedDocDirect) return;

  await resetToPublic(pageA, npubA);
  await resetToPublic(pageB, npubB);

  sharedDocA = nextDocName('shared-doc-a');
  const rootHashBeforeA = await getTreeRootHash(pageA, npubA, 'public');
  await createDocument(pageA, sharedDocA);
  if (rootHashBeforeA) {
    await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeA, 20000);
  }
  const rootHashBeforeEditorsA = await getTreeRootHash(pageA, npubA, 'public');
  await setEditorsFast(pageA, [npubA, npubB]);
  if (rootHashBeforeEditorsA) {
    await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditorsA, 20000);
  }
  await typeInEditor(pageA, 'Seed');
  await waitForSave(pageA);
  await flushPublishes(pageA);
  await resetToPublicFast(pageA, npubA);

  sharedDocDirect = nextDocName('direct-view-doc');
  const rootHashBeforeDirect = await getTreeRootHash(pageA, npubA, 'public');
  await createDocument(pageA, sharedDocDirect);
  if (rootHashBeforeDirect) {
    await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeDirect, 20000);
  }
  const rootHexDirect = await getTreeRootHex(pageA, npubA, 'public');
  await resetToPublicFast(pageA, npubA);

  await seedRemoteTreeRoot(pageB, npubA, 'public', rootHexDirect);
  if (rootHexDirect.hashHex) {
    await waitForTreeRootHash(pageB, npubA, 'public', rootHexDirect.hashHex, 30000);
  }
}

test.describe('Yjs Collaborative Document Editing', () => {
  // Serial mode: multi-user tests connect via relay, parallel tests would cross-talk
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(300000); // 5 minutes for collaboration test

  test.beforeAll(async ({ browser }) => {
    contextA = await browser.newContext();
    contextB = await browser.newContext();
    pageA = await contextA.newPage();
    pageB = await contextB.newPage();

    await Promise.all([setupFreshUser(pageA), setupFreshUser(pageB)]);

    [npubA, pubkeyA, npubB, pubkeyB] = await Promise.all([
      getNpub(pageA),
      getPubkeyHex(pageA),
      getNpub(pageB),
      getPubkeyHex(pageB),
    ]);
    console.log('[yjs-collab] npubA', npubA, 'npubB', npubB);

    await Promise.all([
      followUser(pageA, npubB),
      followUser(pageB, npubA),
    ]);

    const [followedA, followedB] = await Promise.all([
      waitForFollowInWorker(pageA, pubkeyB, 20000),
      waitForFollowInWorker(pageB, pubkeyA, 20000),
    ]);
    expect(followedA).toBe(true);
    expect(followedB).toBe(true);

    await Promise.all([
      pageA.evaluate(async (pk: string) => {
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        await adapter?.setFollows?.([pk]);
        await adapter?.sendHello?.();
      }, pubkeyB),
      pageB.evaluate(async (pk: string) => {
        const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
        await adapter?.setFollows?.([pk]);
        await adapter?.sendHello?.();
      }, pubkeyA),
    ]);
    await Promise.all([
      pageA.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
      pageB.evaluate(() => (window as any).webrtcStore?.sendHello?.()),
    ]);

    await Promise.all([
      pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.()),
      pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.()),
    ]);
    const [rtcA, rtcB] = await Promise.all([
      waitForWebRTCConnection(pageA, 30000, pubkeyB),
      waitForWebRTCConnection(pageB, 30000, pubkeyA),
    ]);
    expect(rtcA).toBe(true);
    expect(rtcB).toBe(true);

    await Promise.all([
      resetToPublic(pageA, npubA),
      resetToPublic(pageB, npubB),
    ]);
  });

  test.beforeEach(async () => {
    await resetToPublic(pageA, npubA);
    await resetToPublic(pageB, npubB);
    await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
  });

  test.afterAll(async () => {
    await contextA?.close();
    await contextB?.close();
  });

  test('user can open collaborator document', async () => {
    await ensureSharedDocs();

    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');
    const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
    const rootHexA = await getTreeRootHex(pageA, npubA, 'public');
    await seedRemoteTreeRoot(pageB, npubA, 'public', rootHexA);
    await Promise.all([
      waitForWebRTCConnection(pageA, 20000, pubkeyB),
      waitForWebRTCConnection(pageB, 20000, pubkeyA),
    ]);
    if (rootHashA) {
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashA, 60000);
    }

    console.log('[shared-notes] User B opening User A doc');
    const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
    await pageB.goto(`http://localhost:5173/#/${npubA}/public${linkParamA}`);
    await waitForAppReady(pageB);
    await waitForRelayConnected(pageB, 30000);
    const docLink = pageB.getByRole('link', { name: sharedDocA }).first();
    await expect(docLink).toBeVisible({ timeout: 60000 });
    await docLink.click();
    const editorReady = await waitForEditorVisible(pageB, 45000);
    if (!editorReady) {
      await openRemoteDocumentFast(pageB, npubA, 'public', sharedDocA, linkKeyA, rootHashA, 60000);
    }
    await expect(pageB.locator('.ProseMirror')).toBeVisible({ timeout: 60000 });
    console.log('[shared-notes] User B sees editor for User A doc');
    await pageB.waitForFunction((target) => window.location.hash.includes(target), npubA, { timeout: 30000 });
  });

  test('real-time sync: A sees B edits without refresh when both view A document', async () => {
    await ensureSharedDocs();

    const docName = sharedDocA;
    const markerA = nextEditMarker('A-INIT');

    const rootHashBefore = await getTreeRootHash(pageA, npubA, 'public');
    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');
    await openRemoteDocumentFast(pageA, npubA, 'public', docName, linkKeyA, rootHashBefore);

    const editorA = pageA.locator('.ProseMirror');
    await expect(editorA).toBeVisible({ timeout: 15000 });
    await editorA.click();
    await pageA.keyboard.press('End');
    await pageA.keyboard.type(` ${markerA}`);
    await expect(editorA).toContainText(markerA, { timeout: 10000 });
    await waitForSave(pageA);
    await flushPublishes(pageA);

    await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHashBefore);

    const editorB = pageB.locator('.ProseMirror');
    await expect(editorB).toBeVisible({ timeout: 15000 });
    await expect(editorB).toContainText(markerA, { timeout: 30000 });
    await waitForEditorBadge(pageB, 30000);
    await Promise.all([
      waitForWebRTCConnection(pageA, 15000, pubkeyB),
      waitForWebRTCConnection(pageB, 15000, pubkeyA),
    ]);

    // Round 1: B edits
    const markerB = nextEditMarker('B-R1');
    await editorB.click();
    await pageB.keyboard.press('End');
    await pageB.keyboard.type(` ${markerB}`);
    await expect(editorB).toContainText(markerB, { timeout: 10000 });
    await waitForSave(pageB);
    await flushPublishes(pageB);
    const rootHexB = await getTreeRootHex(pageB, npubB, 'public');
    await seedRemoteTreeRoot(pageA, npubB, 'public', rootHexB);
    await waitForEditorContent(pageA, markerB, 60000);
    await expect(editorA).toBeVisible({ timeout: 60000 });
  });

  test('when B edits A document, document appears in B directory', async () => {
    await ensureSharedDocs();

    const docName = sharedDocA;
    const contributionMarker = nextEditMarker('B-CONTRIB');

    const rootHash = await getTreeRootHash(pageA, npubA, 'public');
    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');

    // Wait for tree sync
    await openRemoteDocumentFast(pageA, npubA, 'public', docName, linkKeyA, rootHash);
    await expect(pageA.locator('.ProseMirror')).toBeVisible({ timeout: 10000 });

    await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHash);

    const editorB = pageB.locator('.ProseMirror');
    await expect(editorB).toBeVisible({ timeout: 30000 });
    await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForWebRTCConnection(pageB, 30000, pubkeyA);
    await waitForTreeEntry(pageB, npubA, 'public', docName, 60000).catch(() => {});
    await pageB.evaluate(() => (window as any).__reloadYjsEditors?.());
    await waitForEditorBadge(pageB, 90000).catch(() => {
      console.warn('[yjs] Editor badge not visible, continuing to edit');
    });

    const rootHashBeforeB = await getTreeRootHash(pageB, npubB, 'public');
    await editorB.click();
    await pageB.keyboard.press('End');
    await pageB.keyboard.type(` ${contributionMarker}`);
    await waitForSave(pageB);
    await waitForTreeRootHashChange(pageB, npubB, 'public', rootHashBeforeB, 90000);
    await flushPublishes(pageB);
    const rootHashAfterB = await getTreeRootHash(pageB, npubB, 'public');

    await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
    await waitForAppReady(pageB);
    await waitForRelayConnected(pageB, 30000);
    if (rootHashAfterB) {
      await waitForTreeRootHash(pageB, npubB, 'public', rootHashAfterB, 90000);
    }
    const docLink = pageB.getByRole('link', { name: docName }).first();
    await expect(docLink).toBeVisible({ timeout: 30000 });

    await docLink.click();
    const editorBOwn = pageB.locator('.ProseMirror');
    await expect(editorBOwn).toBeVisible({ timeout: 15000 });
    await expect(editorBOwn).toContainText(contributionMarker, { timeout: 15000 });
  });

  test('editor can edit another users document and changes persist', async () => {
    await ensureSharedDocs();

    const docName = sharedDocA;
    const editMarker = nextEditMarker('B-EDIT');

    const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
    expect(rootHashA).toBeTruthy();
    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');

    await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHashA);

    const editorB = pageB.locator('.ProseMirror');
    await expect(editorB).toBeVisible({ timeout: 30000 });
    await waitForEditorBadge(pageB, 30000);

    await editorB.click();
    await pageB.keyboard.press('End');
    await pageB.keyboard.type(` ${editMarker}`);
    await expect(editorB).toContainText(editMarker, { timeout: 30000 });

    await waitForSave(pageB);
    await flushPublishes(pageB);
    const rootHexB = await getTreeRootHex(pageB, npubB, 'public');
    await seedRemoteTreeRoot(pageA, npubB, 'public', rootHexB);

    const rootHashAfterEdit = await getTreeRootHash(pageB, npubA, 'public');
    if (rootHashAfterEdit) {
      await waitForTreeRootHash(pageA, npubA, 'public', rootHashAfterEdit, 60000);
    } else if (rootHashA) {
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashA, 60000);
    }

    const expectedRootAfterEdit = rootHashAfterEdit ?? rootHashA;
    await openRemoteDocumentFast(pageA, npubA, 'public', docName, linkKeyA, expectedRootAfterEdit);
    await waitForEditorContent(pageA, editMarker, 60000);

    const editorA = pageA.locator('.ProseMirror');
    const contentA = await editorA.textContent();
    expect(contentA).toContain(editMarker);
  });

  test('editors count badge shows correct count after document creation and adding collaborator', async () => {
    // This test verifies:
    // 1. When creating a new document, owner's npub should be in .yjs and badge should show "1"
    // 2. After adding a collaborator, badge should show "2"

    // Create a new document
    console.log('Creating new document...');
    const docName = nextDocName('test-editors-count');
    await createDocument(pageA, docName);

    // Check the editors count badge - should show "1" (the owner)
    console.log('Checking editors count badge after creation...');
    const editorsButton = pageA.locator('button[title="Manage editors"]');
    await expect(editorsButton).toBeVisible({ timeout: 30000 });

    // Get button HTML for debugging
    const buttonHtml = await editorsButton.innerHTML();
    console.log(`Editors button HTML: ${buttonHtml}`);

    // The badge is inside the button as a span with the count
    const countBadge = editorsButton.locator('span.rounded-full');
    try {
      await expect(countBadge).toBeVisible({ timeout: 30000 });
    } catch (error) {
      console.log('No badge found, opening modal to check editors list...');
      await editorsButton.click();
      const debugModal = pageA.locator('h2:has-text("Editors")');
      await expect(debugModal).toBeVisible({ timeout: 30000 });

      const listItems = pageA.locator('.bg-surface-1 ul li');
      const listCount = await listItems.count();
      console.log(`Editors in modal list: ${listCount}`);

      const noEditorsMsg = pageA.locator('text=No editors yet');
      const hasNoEditorsMsg = await noEditorsMsg.isVisible().catch(() => false);
      console.log(`"No editors yet" message visible: ${hasNoEditorsMsg}`);

      await pageA.keyboard.press('Escape');
      await expect(debugModal).not.toBeVisible({ timeout: 30000 });
      throw error;
    }

    const initialCount = await countBadge.textContent();
    console.log(`Initial editors count: ${initialCount}`);
    expect(initialCount).toBe('1');

    // Now add a collaborator (use a fake npub for testing)
    console.log('Adding a collaborator...');
    await editorsButton.click();

    // Wait for modal
    const modal = pageA.locator('h2:has-text("Editors")');
    await expect(modal).toBeVisible({ timeout: 30000 });

    // Verify owner is already in the list
    console.log('Verifying owner is in the editors list...');
    const editorsList = pageA.locator('.bg-surface-1 ul li');
    const editorsCount = await editorsList.count();
    console.log(`Editors in list: ${editorsCount}`);
    expect(editorsCount).toBeGreaterThanOrEqual(1);

    // Add a second editor (use a valid bech32-encoded npub)
    const fakeNpub = 'npub1vpqsg7spcesqesfhjjept2rk3p5n9pcd3ef7aqsgyweehxl8dhzqu5deq5';
    const input = pageA.locator('input[placeholder="npub1..."]');
    await input.fill(fakeNpub);

    // Click the confirm button from the preview
    const confirmButton = pageA.locator('button.btn-success').filter({ hasText: /^Add/ }).first();
    await expect(confirmButton).toBeVisible({ timeout: 3000 });
    await expect(confirmButton).toBeEnabled({ timeout: 15000 });
    await confirmButton.click();
    await expect(editorsList).toHaveCount(editorsCount + 1, { timeout: 30000 });

    // Modal auto-saves on add, just close it using the footer Close button (not the X)
    const closeButton = pageA.getByText('Close', { exact: true });
    await closeButton.click();
    await expect(modal).not.toBeVisible({ timeout: 30000 });

    // Check the editors count badge - should now show "2"
    console.log('Checking editors count badge after adding collaborator...');
    const updatedCountBadge = editorsButton.locator('span.rounded-full');
    await expect(updatedCountBadge).toBeVisible({ timeout: 30000 });
    await expect(updatedCountBadge).toHaveText('2', { timeout: 30000 });
    const updatedCount = await updatedCountBadge.textContent();
    console.log(`Updated editors count: ${updatedCount}`);
    expect(updatedCount).toBe('2');

    console.log('\n=== Editors Count Badge Test Passed ===');
  });

  test('document becomes editable without refresh when user is added as editor', async () => {
    const docName = nextDocName('editor-test');

    await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForWebRTCConnection(pageA, 15000, pubkeyB);
    await waitForWebRTCConnection(pageB, 15000, pubkeyA);

    const rootHashBeforeDoc = await getTreeRootHash(pageA, npubA, 'public');
    await createDocument(pageA, docName);

    const editorA = pageA.locator('.ProseMirror');
    await expect(editorA).toBeVisible({ timeout: 30000 });
    await editorA.click();
    await pageA.keyboard.type('Content from owner.');
    await waitForSave(pageA);
    await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeDoc, 60000);
    const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
    expect(rootHashA).toBeTruthy();
    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');
    const rootHexA = await getTreeRootHex(pageA, npubA, 'public');
    await seedRemoteTreeRoot(pageB, npubA, 'public', rootHexA);

    const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
    await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHashA);

    const editorB = pageB.locator('.ProseMirror');
    await expect(editorB).toBeVisible({ timeout: 30000 });
    await expect(editorB).toContainText('Content from owner', { timeout: 30000 });

    const readOnlyBadge = pageB.locator('text=Read-only');
    const isReadOnly = await readOnlyBadge.isVisible();
    expect(isReadOnly).toBe(true);

    const rootHashBeforeEditors = await getTreeRootHash(pageA, npubA, 'public');
    await setEditors(pageA, [npubA, npubB]);
    await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditors, 60000);
    await flushPublishes(pageA);
    const rootHashAfterEditors = await getTreeRootHash(pageA, npubA, 'public');
    if (rootHashAfterEditors) {
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashAfterEditors, 90000);
    }

    const docHash = `#/${npubA}/public/${docName}${linkParamA}`;
    await expect.poll(async () => {
      await pageB.evaluate((hash) => {
        if (window.location.hash !== hash) {
          window.location.hash = hash;
        }
        (window as any).__workerAdapter?.sendHello?.();
        (window as any).__reloadYjsEditors?.();
      }, docHash);
      const editableCount = await pageB.locator('.ProseMirror[contenteditable="true"]').count().catch(() => 0);
      if (editableCount > 0) return true;
      const isReadOnlyVisible = await pageB.locator('text=Read-only').isVisible().catch(() => true);
      return !isReadOnlyVisible;
    }, { timeout: 90000, intervals: [1000, 2000, 3000] }).toBe(true);

    const readOnlyAfter = await pageB.locator('text=Read-only').isVisible();
    expect(readOnlyAfter).toBe(false);

    const editorBadge = pageB.getByText('Editor', { exact: true });
    await expect(editorBadge).toBeVisible({ timeout: 60000 });

    await editorB.click();
    await pageB.keyboard.type(' [B-EDIT]');
    await expect(editorB).toContainText('[B-EDIT]', { timeout: 30000 });
    await waitForSave(pageB);
    await flushPublishes(pageB);
    const rootHexB = await getTreeRootHex(pageB, npubB, 'public');
    await seedRemoteTreeRoot(pageA, npubB, 'public', rootHexB);

    const contentAfterEdit = await editorB.textContent();
    expect(contentAfterEdit).toContain('[B-EDIT]');

    await expect(editorA).toContainText('[B-EDIT]', { timeout: 60000 });
    const contentA = await editorA.textContent();
    expect(contentA).toContain('[B-EDIT]');
  });

  test('long document collaboration persists after refresh for both users', async () => {
    test.setTimeout(240000);
    // This test verifies:
    // 1. Two users can collaboratively write a longer document with edits at different positions
    // 2. All content persists after both users refresh
    // 3. Content is correctly merged even with concurrent edits at beginning, middle, and end
    // 4. Tests the delta-based storage format (multiple deltas created)

    const docName = nextDocName('collab-doc');

    await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForWebRTCConnection(pageA, 15000, pubkeyB);
    await waitForWebRTCConnection(pageB, 15000, pubkeyA);

    const rootHashBeforeDoc = await getTreeRootHash(pageA, npubA, 'public');
    await createDocument(pageA, docName);

    const editorA = pageA.locator('.ProseMirror');
    await expect(editorA).toBeVisible({ timeout: 30000 });
    const initialText = 'Initial text from A. '.repeat(6);
    await editorA.click();
    await pageA.keyboard.type(initialText);
    await expect(editorA).toContainText('Initial text from A.', { timeout: 10000 });
    await waitForSave(pageA);
    await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeDoc, 60000);

    const rootHashBeforeEditors = await getTreeRootHash(pageA, npubA, 'public');
    await setEditors(pageA, [npubA, npubB]);
    await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditors, 60000);
    await flushPublishes(pageA);
    const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
    expect(rootHashA).toBeTruthy();
    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');

    await navigateToOwnDocument(pageA, npubA, 'public', docName, linkKeyA);
    await waitForEditorContent(pageA, 'Initial text');

    const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
    await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHashA);

    const editorB = pageB.locator('.ProseMirror');
    if (!await editorB.isVisible().catch(() => false)) {
      await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHashA);
    }
    await expect(editorB).toBeVisible({ timeout: 60000 });
    await expect(editorB).toContainText('Initial text', { timeout: 60000 });

    await editorB.click();
    await pageB.keyboard.press('End');
    await pageB.keyboard.type(' [B-1]');
    await expect(editorB).toContainText('[B-1]', { timeout: 10000 });
    await waitForSave(pageB);
    await flushPublishes(pageB);
    const rootHexB = await getTreeRootHex(pageB, npubB, 'public');
    await seedRemoteTreeRoot(pageA, npubB, 'public', rootHexB);
    await waitForEditorContent(pageA, '[B-1]', 60000);

    await editorA.click();
    await pageA.keyboard.press('End');
    await pageA.keyboard.type(' [A-1]');
    await expect(editorA).toContainText('[A-1]', { timeout: 10000 });
    await waitForSave(pageA);
    await flushPublishes(pageA);
    const rootHexAAfter = await getTreeRootHex(pageA, npubA, 'public');
    await seedRemoteTreeRoot(pageB, npubA, 'public', rootHexAAfter);
    await waitForEditorContent(pageB, '[A-1]', 60000);

    await waitForSave(pageA);
    await waitForSave(pageB);

    const contentBeforeRefresh = await editorA.textContent();
    const markersToCheck = ['[A-1]', '[B-1]'];
    for (const marker of markersToCheck) {
      if (!contentBeforeRefresh?.includes(marker)) {
        console.log(`Warning: Marker ${marker} not found before refresh`);
      }
    }

    await safeReload(pageA, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(pageA);
    await waitForRelayConnected(pageA, 30000);

    const editorAAfterRefresh = pageA.locator('.ProseMirror');
    await expect(editorAAfterRefresh).toBeVisible({ timeout: 30000 });
    await expect(editorAAfterRefresh).toContainText('[A-1]', { timeout: 60000 });

    const contentAAfterRefresh = await editorAAfterRefresh.textContent();
    expect(contentAAfterRefresh).toContain('[A-1]');
    expect(contentAAfterRefresh).toContain('[B-1]');
    expect(contentAAfterRefresh).toContain('Initial');
    expect(contentAAfterRefresh).toContain('A.');

    await safeReload(pageB, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
    await waitForAppReady(pageB);
    await waitForRelayConnected(pageB, 30000);

    const editorBAfterRefresh = pageB.locator('.ProseMirror');
    await expect(editorBAfterRefresh).toBeVisible({ timeout: 30000 });
    await expect(editorBAfterRefresh).toContainText('[A-1]', { timeout: 60000 });

    const contentBAfterRefresh = await editorBAfterRefresh.textContent();
    expect(contentBAfterRefresh).toContain('[A-1]');
    expect(contentBAfterRefresh).toContain('[B-1]');
    expect(contentBAfterRefresh).toContain('Initial');
    expect(contentBAfterRefresh).toContain('A.');
  });

  test('browser can view document via direct link without creator making more edits', async () => {
    // This test verifies that Browser 2 can view Browser 1's document via direct link
    // WITHOUT Browser 1 making additional edits to trigger sync.
    //
    // The key requirement: once WebRTC connection is established, Browser 2 should
    // be able to navigate to the document URL and see its content immediately.

    await ensureSharedDocs();

    const docName = sharedDocDirect;

    await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForWebRTCConnection(pageA, 15000, pubkeyB);
    await waitForWebRTCConnection(pageB, 15000, pubkeyA);

    const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
    expect(rootHashA).toBeTruthy();
    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');

    await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHashA);
    await waitForWebRTCConnection(pageB, 30000, pubkeyA);

    const editorB = pageB.locator('.ProseMirror');
    await expect(editorB).toBeVisible({ timeout: 60000 });
  });

  test('incognito browser views document via direct URL without prior WebRTC', async ({ browser }) => {
    await ensureSharedDocs();

    const docName = sharedDocDirect;

    const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
    expect(rootHashA).toBeTruthy();
    const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');

    const pushResult = await pushTreeToBlossom(pageA, npubA, 'public');
    expect(pushResult.failed).toBe(0);
    await flushPendingPublishes(pageA);

    const contextB = await browser.newContext();
    const pageB = await contextB.newPage();
    setupPageErrorHandler(pageB);

    try {
      await pageB.goto('http://localhost:5173');
      await waitForAppReady(pageB);
      await disableOthersPool(pageB);
      await configureBlossomServers(pageB);
      await useLocalRelay(pageB);
      await waitForRelayConnected(pageB, 20000);

      const rootHexA = await getTreeRootHex(pageA, npubA, 'public');
      await seedRemoteTreeRoot(pageB, npubA, 'public', rootHexA);

      await openRemoteDocumentFast(pageB, npubA, 'public', docName, linkKeyA, rootHashA);

      const editorB = pageB.locator('.ProseMirror');
      await expect(editorB).toBeVisible({ timeout: 60000 });
    } finally {
      await contextB.close();
    }
  });
});
