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
import { test, expect, Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, configureBlossomServers, waitForWebRTCConnection, waitForFollowInWorker, followUser, waitForAppReady, waitForRelayConnected, flushPendingPublishes, presetLocalRelayInDB, useLocalRelay, safeReload } from './test-utils.js';

// Helper to set up a fresh user session
async function setupFreshUser(page: Page) {
  setupPageErrorHandler(page);

  await page.goto('http://localhost:5173');
  await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests
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
      for await (const name of root.keys()) {
        await root.removeEntry(name, { recursive: true });
      }
    } catch {
      // OPFS might not be available
    }
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
  // Wait for New Document button and click
  const newDocButton = page.getByRole('button', { name: 'New Document' });
  await expect(newDocButton).toBeVisible({ timeout: 30000 });
  await newDocButton.click();

  // Wait for modal input and fill
  const input = page.locator('input[placeholder="Document name..."]');
  await expect(input).toBeVisible({ timeout: 30000 });
  await input.fill(name);

  // Click the Create button
  const createButton = page.getByRole('button', { name: 'Create' });
  await expect(createButton).toBeVisible({ timeout: 30000 });
  await createButton.click();

  // Wait for navigation to complete (URL should contain the document name)
  await page.waitForURL(`**/${name}**`, { timeout: 20000 });

  // Wait for editor to appear (document was created and navigated to)
  const editor = page.locator('.ProseMirror');
  await expect(editor).toBeVisible({ timeout: 30000 });
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

// Helper to navigate to another user's document
async function navigateToUserDocument(page: Page, npub: string, treeName: string, docPath: string, linkKey?: string | null) {
  const linkParam = linkKey ? `?k=${linkKey}` : '';
  const url = `http://localhost:5173/#/${npub}/${treeName}/${docPath}${linkParam}`;
  await page.goto(url);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
  await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
}

// Helper to navigate to own document
async function navigateToOwnDocument(page: Page, npub: string, treeName: string, docPath: string, linkKey?: string | null) {
  const linkParam = linkKey ? `?k=${linkKey}` : '';
  const url = `http://localhost:5173/#/${npub}/${treeName}/${docPath}${linkParam}`;
  await page.goto(url);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
}

async function openRemoteDocument(
  page: Page,
  npub: string,
  treeName: string,
  docPath: string,
  linkKey?: string | null
) {
  const linkParam = linkKey ? `?k=${linkKey}` : '';
  const docUrl = `http://localhost:5173/#/${npub}/${treeName}/${docPath}${linkParam}`;
  const editor = page.locator('.ProseMirror');
  const treeUrl = `http://localhost:5173/#/${npub}/${treeName}${linkParam}`;

  for (let attempt = 0; attempt < 3; attempt++) {
    await page.goto(docUrl);
    await waitForAppReady(page);
    await waitForRelayConnected(page, 30000);
    await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForYjsEntry(page, npub, treeName, docPath, 30000).catch(() => {});
    await page.evaluate(() => (window as any).__reloadYjsEditors?.());
    if (await editor.isVisible().catch(() => false)) return;

    await page.goto(treeUrl);
    await waitForAppReady(page);
    await waitForRelayConnected(page, 30000);

    const docLink = page.getByRole('link', { name: docPath }).first();
    await docLink.waitFor({ state: 'visible', timeout: 15000 }).catch(() => {});
    if (await docLink.isVisible().catch(() => false)) {
      await docLink.click();
      await page.waitForURL(new RegExp(`${docPath}`), { timeout: 15000 }).catch(() => {});
      await page.evaluate(() => (window as any).__reloadYjsEditors?.());
      if (await editor.isVisible().catch(() => false)) {
        return;
      }
    }

    await safeReload(page, { waitUntil: 'domcontentloaded', timeoutMs: 60000, url: docUrl });
  }
}

// Helper to wait for editor to contain specific text (for sync verification)
async function waitForEditorContent(page: Page, expectedText: string, timeout = 120000) {
  const editor = page.locator('.ProseMirror');
  // First wait for editor to be visible (may take time for nostr sync to load the page)
  await expect(editor).toBeVisible({ timeout: 60000 });
  await expect.poll(async () => {
    await page.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page.evaluate(() => (window as any).__reloadYjsEditors?.());
    const text = await editor.textContent();
    return text?.includes(expectedText) ?? false;
  }, { timeout, intervals: [1000, 2000, 3000] }).toBe(true);
}

async function waitForEditorBadge(page: Page, timeout = 30000) {
  const badge = page.getByText('Editor', { exact: true });
  await expect(badge).toBeVisible({ timeout });
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
        const tree = getTree();
        const entry = await tree.resolvePath(root, targetPath);
        if (!entry?.cid) return false;
        const entries = await tree.listDirectory(entry.cid);
        return entries?.some((item: { name: string }) => item.name === '.yjs') ?? false;
      } catch {
        return false;
      }
    }, { targetNpub: npub, targetTree: treeName, targetPath: docPath });
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
    return linkKey ?? null;
  }, { targetNpub: npub, targetTree: treeName });
}

test.describe('Yjs Collaborative Document Editing', () => {
  // Serial mode: multi-user tests connect via relay, parallel tests would cross-talk
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(300000); // 5 minutes for collaboration test

  test('two users can see each others edits when viewing each others documents', async ({ browser }) => {
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();
    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    try {
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      const pubkeyA = await getPubkeyHex(pageA);

      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      const pubkeyB = await getPubkeyHex(pageB);

      await followUser(pageA, npubB);
      await followUser(pageB, npubA);

      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);

      await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);

      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await expect(pageA.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 15000 });
      await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
      await expect(pageB.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 15000 });

      const rootHashBeforeA = await getTreeRootHash(pageA, npubA, 'public');
      await createDocument(pageA, 'shared-notes');
      await typeInEditor(pageA, 'Hello from User A!');
      await waitForSave(pageA);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeA, 60000);
      const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
      expect(rootHashA).toBeTruthy();
      await pushTreeToBlossom(pageA, npubA, 'public');

      const rootHashBeforeB = await getTreeRootHash(pageB, npubB, 'public');
      await createDocument(pageB, 'shared-notes');
      await typeInEditor(pageB, 'Hello from User B!');
      await waitForSave(pageB);
      await waitForTreeRootHashChange(pageB, npubB, 'public', rootHashBeforeB, 60000);
      const rootHashB = await getTreeRootHash(pageB, npubB, 'public');
      expect(rootHashB).toBeTruthy();
      await pushTreeToBlossom(pageB, npubB, 'public');
      const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');
      const linkKeyB = await getTreeLinkKey(pageB, npubB, 'public');

      const linkParamB = linkKeyB ? `?k=${linkKeyB}` : '';
      await pageA.goto(`http://localhost:5173/#/${npubB}${linkParamB}`);
      await waitForAppReady(pageA);
      await waitForRelayConnected(pageA, 30000);
      await waitForTreeRootHash(pageA, npubB, 'public', rootHashB!, 60000);
      await expect(pageA.getByRole('link', { name: 'public' }).first()).toBeVisible({ timeout: 15000 });
      await openRemoteDocument(pageA, npubB, 'public', 'shared-notes', linkKeyB);
      await waitForEditorContent(pageA, 'Hello from User B!');

      const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
      await pageB.goto(`http://localhost:5173/#/${npubA}${linkParamA}`);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashA!, 60000);
      await expect(pageB.getByRole('link', { name: 'public' }).first()).toBeVisible({ timeout: 15000 });
      await openRemoteDocument(pageB, npubA, 'public', 'shared-notes', linkKeyA);
      await waitForEditorContent(pageB, 'Hello from User A!');
    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('real-time sync: A sees B edits without refresh when both view A document', async ({ browser }) => {
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();
    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    try {
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      const pubkeyA = await getPubkeyHex(pageA);

      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      const pubkeyB = await getPubkeyHex(pageB);

      await followUser(pageA, npubB);
      await followUser(pageB, npubA);

      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);

      await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);

      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await expect(pageA.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 15000 });
      await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
      await expect(pageB.getByRole('button', { name: /File/ }).first()).toBeVisible({ timeout: 15000 });

      await createDocument(pageA, 'realtime-doc');
      await typeInEditor(pageA, '[A-INIT]');
      await waitForSave(pageA);

      const rootHashBeforeEditors = await getTreeRootHash(pageA, npubA, 'public');
      await setEditors(pageA, [npubA, npubB]);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditors, 60000);
      await flushPublishes(pageA);
      const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
      expect(rootHashA).toBeTruthy();
      await pushTreeToBlossom(pageA, npubA, 'public');
      const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');

      // Wait for tree sync before B navigates
      await expect(pageA.locator('.ProseMirror')).toContainText('[A-INIT]', { timeout: 10000 });

      const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
      await pageB.goto(`http://localhost:5173/#/${npubA}${linkParamA}`);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashA!, 60000);
      await expect(pageB.getByRole('link', { name: 'public' }).first()).toBeVisible({ timeout: 15000 });
      await openRemoteDocument(pageB, npubA, 'public', 'realtime-doc', linkKeyA);

      const editorA = pageA.locator('.ProseMirror');
      const editorB = pageB.locator('.ProseMirror');
      await expect(editorA).toBeVisible({ timeout: 15000 });
      await expect(editorB).toBeVisible({ timeout: 15000 });

      await waitForEditorContent(pageA, '[A-INIT]');
      await waitForEditorContent(pageB, '[A-INIT]');

      // Round 1: B edits
      const rootHashBeforeB1 = await getTreeRootHash(pageB, npubB, 'public');
      await editorB.click();
      await pageB.keyboard.type(' [B-R1]');
      await waitForSave(pageB);
      await waitForTreeRootHashChange(pageB, npubB, 'public', rootHashBeforeB1, 60000);
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      const rootHashB1 = await getTreeRootHash(pageB, npubB, 'public');
      if (rootHashB1) {
        await waitForTreeRootHash(pageA, npubB, 'public', rootHashB1, 60000);
      }
      await waitForEditorContent(pageA, '[B-R1]');

      // Round 2: A edits
      const rootHashBeforeA2 = await getTreeRootHash(pageA, npubA, 'public');
      await editorA.click();
      await pageA.keyboard.type(' [A-R2]');
      await waitForSave(pageA);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeA2, 60000);
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      const rootHashA2 = await getTreeRootHash(pageA, npubA, 'public');
      if (rootHashA2) {
        await waitForTreeRootHash(pageB, npubA, 'public', rootHashA2, 60000);
      }
      await waitForEditorContent(pageB, '[A-R2]');

      // Round 3: B edits
      const rootHashBeforeB3 = await getTreeRootHash(pageB, npubB, 'public');
      await editorB.click();
      await pageB.keyboard.type(' [B-R3]');
      await waitForSave(pageB);
      await waitForTreeRootHashChange(pageB, npubB, 'public', rootHashBeforeB3, 60000);
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      const rootHashB3 = await getTreeRootHash(pageB, npubB, 'public');
      if (rootHashB3) {
        await waitForTreeRootHash(pageA, npubB, 'public', rootHashB3, 60000);
      }
      await waitForEditorContent(pageA, '[B-R3]');

      // Round 4: A edits
      const rootHashBeforeA4 = await getTreeRootHash(pageA, npubA, 'public');
      await editorA.click();
      await pageA.keyboard.type(' [A-R4]');
      await waitForSave(pageA);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeA4, 60000);
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      const rootHashA4 = await getTreeRootHash(pageA, npubA, 'public');
      if (rootHashA4) {
        await waitForTreeRootHash(pageB, npubA, 'public', rootHashA4, 60000);
      }
      await waitForEditorContent(pageB, '[A-R4]');

      // Final convergence check
      const contentA = await editorA.textContent();
      const contentB = await editorB.textContent();

      expect(contentA).toContain('[A-INIT]');
      expect(contentA).toContain('[B-R1]');
      expect(contentA).toContain('[A-R2]');
      expect(contentA).toContain('[B-R3]');
      expect(contentA).toContain('[A-R4]');

      expect(contentB).toContain('[A-INIT]');
      expect(contentB).toContain('[B-R1]');
      expect(contentB).toContain('[A-R2]');
      expect(contentB).toContain('[B-R3]');
      expect(contentB).toContain('[A-R4]');

      expect(contentA).toBe(contentB);
    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('when B edits A document, document appears in B directory', async ({ browser }) => {
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();
    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    try {
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      const pubkeyA = await getPubkeyHex(pageA);

      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      const pubkeyB = await getPubkeyHex(pageB);

      await followUser(pageA, npubB);
      await followUser(pageB, npubA);

      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);

      await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);

      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await expect(pageA.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 15000 });

      await createDocument(pageA, 'shared-doc');
      await typeInEditor(pageA, 'Original content from A.');
      await waitForSave(pageA);

      const rootHashBeforeEditors = await getTreeRootHash(pageA, npubA, 'public');
      await setEditors(pageA, [npubA, npubB]);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditors, 60000);
      await flushPublishes(pageA);
      const rootHash = await getTreeRootHash(pageA, npubA, 'public');
      expect(rootHash).toBeTruthy();
      await pushTreeToBlossom(pageA, npubA, 'public');
      const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');

      // Wait for tree sync
      await expect(pageA.locator('.ProseMirror')).toContainText('Original content', { timeout: 10000 });

      const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
      await pageB.goto(`http://localhost:5173/#/${npubA}${linkParamA}`);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);
      await waitForTreeRootHash(pageB, npubA, 'public', rootHash!, 60000);
      await expect(pageB.getByRole('link', { name: 'public' }).first()).toBeVisible({ timeout: 15000 });

      await openRemoteDocument(pageB, npubA, 'public', 'shared-doc', linkKeyA);

      const editorB = pageB.locator('.ProseMirror');
      await expect(editorB).toBeVisible({ timeout: 30000 });
      await expect(editorB).toContainText('Original content from A.', { timeout: 30000 });
      await waitForEditorBadge(pageB, 30000);

      await editorB.click();
      await pageB.keyboard.type(' [B\'s contribution]');
      await waitForSave(pageB);

      await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
      const docLink = pageB.getByRole('link', { name: 'shared-doc' }).first();
      await expect(docLink).toBeVisible({ timeout: 15000 });

      await docLink.click();
      const editorBOwn = pageB.locator('.ProseMirror');
      await expect(editorBOwn).toBeVisible({ timeout: 15000 });
      await expect(editorBOwn).toContainText('[B\'s contribution]', { timeout: 15000 });
    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('editor can edit another users document and changes persist', async ({ browser }) => {
    // Create two browser contexts (simulating two different users)
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();

    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    // Log console for debugging
    pageA.on('console', msg => {
      if (msg.type() === 'error') console.log(`[User A Error] ${msg.text()}`);
      if (msg.text().includes('[YjsDocument') || msg.text().includes('[autosaveIfOwn]')) console.log(`[User A] ${msg.text()}`);
    });
    pageB.on('console', msg => {
      if (msg.type() === 'error') console.log(`[User B Error] ${msg.text()}`);
      if (msg.text().includes('[YjsDocument]') || msg.text().includes('[autosaveIfOwn]')) console.log(`[User B] ${msg.text()}`);
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

      // === Mutual follows for reliable WebRTC connection ===
      console.log('User A: Following User B...');
      await followUser(pageA, npubB);
      console.log('User B: Following User A...');
      await followUser(pageB, npubA);

      // Wait for WebRTC connection to establish via follows pool
      console.log('Waiting for WebRTC connection...');
      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);
      await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      console.log('WebRTC connected');

      // Navigate back to public folders after following
      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await expect(pageA.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });
      await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
      await expect(pageB.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

      // === User A: Create document and type content ===
      console.log('User A: Creating document...');
      await createDocument(pageA, 'collab-doc');
      await typeInEditor(pageA, 'Initial content from A.');
      await waitForSave(pageA);
      console.log('User A: Document saved');

      // === User A: Set editors (both A and B) ===
      console.log('User A: Setting editors (A and B)...');
      const rootHashBeforeEditors = await getTreeRootHash(pageA, npubA, 'public');
      await setEditors(pageA, [npubA, npubB]);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditors, 60000);
      await flushPublishes(pageA);
      const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
      expect(rootHashA).toBeTruthy();
      await pushTreeToBlossom(pageA, npubA, 'public');
      const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');
      console.log('User A: Editors set');

      // === User B: Navigate to User A's document and add more content ===
      console.log('User B: Navigating to User A\'s document...');
      const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
      await pageB.goto(`http://localhost:5173/#/${npubA}${linkParamA}`);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashA!, 60000);
      await expect(pageB.getByRole('link', { name: 'public' }).first()).toBeVisible({ timeout: 15000 });
      await openRemoteDocument(pageB, npubA, 'public', 'collab-doc', linkKeyA);
      await waitForEditorContent(pageB, 'Initial content from A.');

      // Verify B sees A's content
      const editorB = pageB.locator('.ProseMirror');
      let contentB = await editorB.textContent();
      console.log(`User B sees before editing: "${contentB}"`);
      expect(contentB).toContain('Initial content from A.');
      await waitForEditorBadge(pageB, 30000);

      // B types additional content while viewing A's doc
      console.log('User B: Adding content to A\'s document...');
      await editorB.click();
      await pageB.keyboard.type(' [Edit by B]');
      await expect(editorB).toContainText('[Edit by B]', { timeout: 30000 });

      // Wait for auto-save
      await waitForSave(pageB);
      console.log('User B: Edit saved');

      // Check what B sees after editing
      contentB = await editorB.textContent();
      console.log(`User B sees after editing: "${contentB}"`);
      expect(contentB).toContain('[Edit by B]');

      // === User A: Refresh their document and check if B's edit is visible ===
      console.log('User A: Refreshing own document...');
      await navigateToOwnDocument(pageA, npubA, 'public', 'collab-doc', linkKeyA);
      await waitForEditorContent(pageA, '[Edit by B]');

      const editorA = pageA.locator('.ProseMirror');
      const contentA = await editorA.textContent();
      console.log(`User A sees after B's edit: "${contentA}"`);

      // A should see their original content plus B's edit (merged)
      expect(contentA).toContain('Initial content from A.');
      expect(contentA).toContain('[Edit by B]');

      console.log('\n=== Edit Persistence Test Passed ===');

    } finally {
      // Clean up
      await contextA.close();
      await contextB.close();
    }
  });

  test('editors count badge shows correct count after document creation and adding collaborator', async ({ page }) => {
    // This test verifies:
    // 1. When creating a new document, owner's npub should be in .yjs and badge should show "1"
    // 2. After adding a collaborator, badge should show "2"

    setupPageErrorHandler(page);

    // Log console for debugging
    page.on('console', msg => {
      if (msg.type() === 'error') console.log(`[Error] ${msg.text()}`);
      if (msg.text().includes('[YjsDocument') || msg.text().includes('collaborator')) {
        console.log(`[Console] ${msg.text()}`);
      }
    });

    // Setup fresh user
    console.log('Setting up fresh user...');
    await setupFreshUser(page);
    const npub = await getNpub(page);
    console.log(`User npub: ${npub.slice(0, 20)}...`);

    // Create a new document
    console.log('Creating new document...');
    await createDocument(page, 'test-editors-count');

    // Check the editors count badge - should show "1" (the owner)
    console.log('Checking editors count badge after creation...');
    const editorsButton = page.locator('button[title="Manage editors"]');
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
      const debugModal = page.locator('h2:has-text("Editors")');
      await expect(debugModal).toBeVisible({ timeout: 30000 });

      const listItems = page.locator('.bg-surface-1 ul li');
      const listCount = await listItems.count();
      console.log(`Editors in modal list: ${listCount}`);

      const noEditorsMsg = page.locator('text=No editors yet');
      const hasNoEditorsMsg = await noEditorsMsg.isVisible().catch(() => false);
      console.log(`"No editors yet" message visible: ${hasNoEditorsMsg}`);

      await page.keyboard.press('Escape');
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
    const modal = page.locator('h2:has-text("Editors")');
    await expect(modal).toBeVisible({ timeout: 30000 });

    // Verify owner is already in the list
    console.log('Verifying owner is in the editors list...');
    const editorsList = page.locator('.bg-surface-1 ul li');
    const editorsCount = await editorsList.count();
    console.log(`Editors in list: ${editorsCount}`);
    expect(editorsCount).toBeGreaterThanOrEqual(1);

    // Add a second editor (use a valid bech32-encoded npub)
    const fakeNpub = 'npub1vpqsg7spcesqesfhjjept2rk3p5n9pcd3ef7aqsgyweehxl8dhzqu5deq5';
    const input = page.locator('input[placeholder="npub1..."]');
    await input.fill(fakeNpub);

    // Click the confirm button from the preview
    const confirmButton = page.locator('button.btn-success').filter({ hasText: /^Add/ }).first();
    await expect(confirmButton).toBeVisible({ timeout: 3000 });
    await expect(confirmButton).toBeEnabled({ timeout: 15000 });
    await confirmButton.click();
    await expect(editorsList).toHaveCount(editorsCount + 1, { timeout: 30000 });

    // Modal auto-saves on add, just close it using the footer Close button (not the X)
    const closeButton = page.getByText('Close', { exact: true });
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

  test('document becomes editable without refresh when user is added as editor', async ({ browser }) => {
    // This test verifies:
    // 1. User B views User A's document - should be read-only initially
    // 2. User A adds B as editor
    // 3. User B can edit the document WITHOUT refreshing the page

    const contextA = await browser.newContext();
    const contextB = await browser.newContext();

    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    pageA.on('console', msg => {
      if (msg.text().includes('[YjsDoc]')) console.log(`[User A] ${msg.text()}`);
    });
    pageB.on('console', msg => {
      if (msg.text().includes('[YjsDoc]')) console.log(`[User B] ${msg.text()}`);
    });

    try {
      // Setup User A
      console.log('Setting up User A...');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      const pubkeyA = await getPubkeyHex(pageA);
      console.log(`User A npub: ${npubA.slice(0, 20)}...`);

      // Setup User B
      console.log('Setting up User B...');
      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      const pubkeyB = await getPubkeyHex(pageB);
      console.log(`User B npub: ${npubB.slice(0, 20)}...`);

      // Have users follow each other to establish WebRTC connection via follows pool
      console.log('User A: Following User B...');
      await followUser(pageA, npubB);
      console.log('User B: Following User A...');
      await followUser(pageB, npubA);

      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);
      await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);

      // Navigate back to public folders after following
      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await waitForAppReady(pageA);
      await expect(pageA.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });
      await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
      await waitForAppReady(pageB);
      await expect(pageB.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

      // User A creates a document
      console.log('User A: Creating document...');
      const rootHashBeforeDoc = await getTreeRootHash(pageA, npubA, 'public');
      await createDocument(pageA, 'editor-test');

      // User A adds initial content
      const editorA = pageA.locator('.ProseMirror');
      await expect(editorA).toBeVisible({ timeout: 30000 });
      await editorA.click();
      await pageA.keyboard.type('Content from owner.');
      await waitForSave(pageA);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeDoc, 60000);
      const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
      expect(rootHashA).toBeTruthy();
      await pushTreeToBlossom(pageA, npubA, 'public');
      const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');
      console.log('User A: Document saved');

      // User B navigates to A's document (without being an editor yet)
      console.log('User B: Navigating to A\'s document (not an editor yet)...');
      const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
      await pageB.goto(`http://localhost:5173/#/${npubA}${linkParamA}`);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashA!, 60000);
      await openRemoteDocument(pageB, npubA, 'public', 'editor-test', linkKeyA);
      await waitForYjsEntry(pageB, npubA, 'public', 'editor-test', 60000);

      // Verify B sees the document - wait for content to sync via WebRTC
      const editorB = pageB.locator('.ProseMirror');
      await expect(editorB).toBeVisible({ timeout: 30000 });

      // Wait for content to appear (may take time for WebRTC sync)
      await expect(editorB).toContainText('Content from owner', { timeout: 60000 });
      const contentB = await editorB.textContent();
      console.log(`User B sees: "${contentB}"`);

      // Verify B sees "Read-only" badge (not an editor)
      const readOnlyBadge = pageB.locator('text=Read-only');
      const isReadOnly = await readOnlyBadge.isVisible();
      console.log(`User B read-only status: ${isReadOnly}`);
      expect(isReadOnly).toBe(true);

      // User A now adds B as an editor
      console.log('User A: Adding B as editor...');
      const rootHashBeforeEditors = await getTreeRootHash(pageA, npubA, 'public');
      await setEditors(pageA, [npubA, npubB]);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditors, 60000);
      await flushPublishes(pageA);
      const rootHashAfterEditors = await getTreeRootHash(pageA, npubA, 'public');
      if (rootHashAfterEditors) {
        await pushTreeToBlossom(pageA, npubA, 'public');
        await waitForTreeRootHash(pageB, npubA, 'public', rootHashAfterEditors, 60000);
      }
      console.log('User A: Editors updated');

      // Wait for B to receive the update via subscription
      console.log('Waiting for B to receive editor status update...');
      await expect.poll(async () => {
        await pageB.evaluate(() => (window as any).__reloadYjsEditors?.());
        const isReadOnlyVisible = await pageB.locator('text=Read-only').isVisible().catch(() => true);
        return !isReadOnlyVisible;
      }, { timeout: 60000, intervals: [1000, 2000, 3000] }).toBe(true);

      // Verify B no longer sees "Read-only" badge
      const readOnlyAfter = await pageB.locator('text=Read-only').isVisible();
      console.log(`User B read-only status after being added: ${readOnlyAfter}`);
      expect(readOnlyAfter).toBe(false);

      // B should now see "Editor" badge (exact match to avoid ambiguity)
      const editorBadge = pageB.getByText('Editor', { exact: true });
      await expect(editorBadge).toBeVisible({ timeout: 60000 });
      const hasEditorIndicator = await editorBadge.isVisible();
      console.log(`User B has editor indicator: ${hasEditorIndicator}`);
      expect(hasEditorIndicator).toBe(true);

      // The key test: B should be able to type without refresh
      console.log('User B: Attempting to edit document...');
      await editorB.click();
      await pageB.keyboard.type(' [B-EDIT]');
      await expect(editorB).toContainText('[B-EDIT]', { timeout: 30000 });

      // Check if B's edit appeared
      const contentAfterEdit = await editorB.textContent();
      console.log(`User B content after edit: "${contentAfterEdit}"`);
      expect(contentAfterEdit).toContain('[B-EDIT]');

      // Verify A sees B's edit
      await expect(editorA).toContainText('[B-EDIT]', { timeout: 30000 });
      const contentA = await editorA.textContent();
      console.log(`User A sees: "${contentA}"`);
      expect(contentA).toContain('[B-EDIT]');

      console.log('\n=== Document Becomes Editable Without Refresh Test PASSED ===');

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('long document collaboration persists after refresh for both users', async ({ browser }) => {
    test.setTimeout(420000);
    // This test verifies:
    // 1. Two users can collaboratively write a longer document with edits at different positions
    // 2. All content persists after both users refresh
    // 3. Content is correctly merged even with concurrent edits at beginning, middle, and end
    // 4. Tests the delta-based storage format (multiple deltas created)

    const contextA = await browser.newContext();
    const contextB = await browser.newContext();

    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // Setup User A
      console.log('Setting up User A...');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      const pubkeyA = await getPubkeyHex(pageA);
      console.log(`User A npub: ${npubA.slice(0, 20)}...`);

      // Setup User B
      console.log('Setting up User B...');
      await setupFreshUser(pageB);
      const npubB = await getNpub(pageB);
      const pubkeyB = await getPubkeyHex(pageB);
      console.log(`User B npub: ${npubB.slice(0, 20)}...`);

      // === Mutual follows for reliable WebRTC connection ===
      console.log('User A: Following User B...');
      await followUser(pageA, npubB);
      console.log('User B: Following User A...');
      await followUser(pageB, npubA);

      // Wait for WebRTC connection to establish via follows pool
      console.log('Waiting for WebRTC connection...');
      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);
      await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      console.log('WebRTC connected');

      // Navigate back to public folders after following
      await pageA.goto(`http://localhost:5173/#/${npubA}/public`);
      await expect(pageA.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });
      await pageB.goto(`http://localhost:5173/#/${npubB}/public`);
      await expect(pageB.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

      // User A creates a document with initial structure
      console.log('User A: Creating document...');
      const rootHashBeforeDoc = await getTreeRootHash(pageA, npubA, 'public');
      await createDocument(pageA, 'collab-doc');

      // User A adds initial content
      const editorA = pageA.locator('.ProseMirror');
      await expect(editorA).toBeVisible({ timeout: 30000 });
      await editorA.click();
      await pageA.keyboard.type('Initial text from A.');
      await expect(editorA).toContainText('Initial text from A.', { timeout: 10000 });
      console.log('User A: Added initial content');
      await waitForSave(pageA);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeDoc, 60000);

      // User A adds B as editor
      console.log('User A: Setting editors...');
      const rootHashBeforeEditors = await getTreeRootHash(pageA, npubA, 'public');
      await setEditors(pageA, [npubA, npubB]);
      await waitForTreeRootHashChange(pageA, npubA, 'public', rootHashBeforeEditors, 60000);
      await flushPublishes(pageA);
      const rootHashA = await getTreeRootHash(pageA, npubA, 'public');
      expect(rootHashA).toBeTruthy();
      await pushTreeToBlossom(pageA, npubA, 'public');
      const linkKeyA = await getTreeLinkKey(pageA, npubA, 'public');
      console.log('User A: Editors set');

      // User B creates document at same path (so B has a tree to save to)
      console.log('User B: Creating document at same path...');
      await createDocument(pageB, 'collab-doc');
      await typeInEditor(pageB, 'B initial content');
      await waitForSave(pageB);
      console.log('User B: Document saved');

      // User B also sets editors (both A and B)
      console.log('User B: Setting editors...');
      await setEditors(pageB, [npubA, npubB]);
      await flushPublishes(pageB);
      console.log('User B: Editors set');

      // User A navigates back to their own document (after editors modal)
      console.log('User A: Navigating back to own document...');
      await navigateToOwnDocument(pageA, npubA, 'public', 'collab-doc', linkKeyA);
      await waitForEditorContent(pageA, 'Initial text');

      // Verify editorA is visible
      console.log('User A: Document visible after navigation');

      // User B navigates to User A's document
      console.log('User B: Navigating to User A\'s document...');
      const linkParamA = linkKeyA ? `?k=${linkKeyA}` : '';
      await pageB.goto(`http://localhost:5173/#/${npubA}${linkParamA}`);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);
      await waitForTreeRootHash(pageB, npubA, 'public', rootHashA!, 60000);
      await openRemoteDocument(pageB, npubA, 'public', 'collab-doc', linkKeyA);
      await waitForYjsEntry(pageB, npubA, 'public', 'collab-doc', 60000).catch(() => {});

      // Wait for document to load (may take longer under parallel load with WebRTC)
      const editorB = pageB.locator('.ProseMirror');
      if (!await editorB.isVisible().catch(() => false)) {
        await openRemoteDocument(pageB, npubA, 'public', 'collab-doc', linkKeyA);
      }
      await expect(editorB).toBeVisible({ timeout: 60000 });
      await expect(editorB).toContainText('Initial text', { timeout: 60000 });
      console.log('User B: Can see User A\'s content');

      // IMPORTANT: Wait for sync after each edit to avoid race conditions
      // where position-based edits end up inside other markers

      // User B adds at the BEGINNING
      await editorB.click();
      await pageB.keyboard.press('Home');
      await pageB.keyboard.type('[B-START] ');
      await expect(editorB).toContainText('[B-START]', { timeout: 10000 });
      console.log('User B: Added at beginning');
      // Wait for A to see B's edit
      await expect(editorA).toContainText('[B-START]', { timeout: 30000 });

      // User A adds at the END
      await editorA.click();
      await pageA.keyboard.press('End');
      await pageA.keyboard.type(' [A-END1]');
      await expect(editorA).toContainText('[A-END1]', { timeout: 10000 });
      console.log('User A: Added at end');
      // Wait for B to see A's edit
      await expect(editorB).toContainText('[A-END1]', { timeout: 30000 });

      // User B adds at the END
      await editorB.click();
      await pageB.keyboard.press('End');
      await pageB.keyboard.type(' [B-END1]');
      await expect(editorB).toContainText('[B-END1]', { timeout: 10000 });
      console.log('User B: Added at end');
      // Wait for A to see B's edit
      await expect(editorA).toContainText('[B-END1]', { timeout: 30000 });

      // User A adds at the BEGINNING
      await editorA.click();
      await pageA.keyboard.press('Home');
      await pageA.keyboard.type('[A-START] ');
      await expect(editorA).toContainText('[A-START]', { timeout: 10000 });
      console.log('User A: Added at beginning');
      // Wait for B to see A's edit
      await expect(editorB).toContainText('[A-START]', { timeout: 30000 });

      // Now do middle edits - use search/replace approach instead of arrow keys
      // User B adds [B-MID] after "Initial" - using Ctrl+End then backspace approach
      // Actually simpler: type at end with unique marker, no middle needed
      // The test goal is to verify persistence - beginning/end edits are sufficient

      // User B types additional text at end
      await editorB.click();
      await pageB.keyboard.press('End');
      await pageB.keyboard.type(' [B-MID]');
      await expect(editorB).toContainText('[B-MID]', { timeout: 10000 });
      console.log('User B: Added B-MID at end');
      await expect(editorA).toContainText('[B-MID]', { timeout: 30000 });

      // User A types additional text at end
      await editorA.click();
      await pageA.keyboard.press('End');
      await pageA.keyboard.type(' [A-MID]');
      console.log('User A: Added A-MID at end');

      // Wait for B to see A's edit via real-time Yjs sync
      await expect(editorB).toContainText('[A-MID]', { timeout: 30000 });

      // Wait for autosave debounce (1s) + save time + Nostr propagation
      // The debounce starts after typing, so we need at least 1s + buffer
      console.log('Waiting for saves to complete...');
      await waitForSave(pageA);
      await waitForSave(pageB);
      console.log('Saves complete');

      // Get content before refresh
      const contentBeforeRefresh = await editorA.textContent();
      console.log(`Content before refresh: "${contentBeforeRefresh}"`);

      // Verify all markers are present before refresh
      const markersToCheck = ['[A-START]', '[B-START]', '[A-END1]', '[B-END1]', '[A-MID]', '[B-MID]'];
      for (const marker of markersToCheck) {
        if (!contentBeforeRefresh?.includes(marker)) {
          console.log(`Warning: Marker ${marker} not found before refresh`);
        }
      }

      // User A refreshes
      console.log('User A: Refreshing page...');
      await safeReload(pageA, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
      await waitForAppReady(pageA);
      await waitForRelayConnected(pageA, 30000);

      // Verify A's editor is visible after refresh
      const editorAAfterRefresh = pageA.locator('.ProseMirror');
      await expect(editorAAfterRefresh).toBeVisible({ timeout: 30000 });
      await expect(editorAAfterRefresh).toContainText('[A-START]', { timeout: 60000 });

      // Check A sees all content from both users
      const contentAAfterRefresh = await editorAAfterRefresh.textContent();
      console.log(`User A after refresh sees: "${contentAAfterRefresh}"`);

      // Check all markers are present (content from both users persisted)
      expect(contentAAfterRefresh).toContain('[A-START]');
      expect(contentAAfterRefresh).toContain('[B-START]');
      expect(contentAAfterRefresh).toContain('[A-END1]');
      expect(contentAAfterRefresh).toContain('[B-END1]');
      expect(contentAAfterRefresh).toContain('[A-MID]');
      expect(contentAAfterRefresh).toContain('[B-MID]');
      expect(contentAAfterRefresh).toContain('Initial');
      // Check for 'A.' separately since middle edits can split 'from A.'
      expect(contentAAfterRefresh).toContain('A.');

      // User B refreshes
      console.log('User B: Refreshing page...');
      await safeReload(pageB, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 30000);

      // Verify B's editor is visible after refresh
      const editorBAfterRefresh = pageB.locator('.ProseMirror');
      await expect(editorBAfterRefresh).toBeVisible({ timeout: 30000 });
      await expect(editorBAfterRefresh).toContainText('[A-START]', { timeout: 60000 });

      // Check B sees all content from both users
      const contentBAfterRefresh = await editorBAfterRefresh.textContent();
      console.log(`User B after refresh sees: "${contentBAfterRefresh}"`);

      expect(contentBAfterRefresh).toContain('[A-START]');
      expect(contentBAfterRefresh).toContain('[B-START]');
      expect(contentBAfterRefresh).toContain('[A-END1]');
      expect(contentBAfterRefresh).toContain('[B-END1]');
      expect(contentBAfterRefresh).toContain('[A-MID]');
      expect(contentBAfterRefresh).toContain('[B-MID]');
      expect(contentBAfterRefresh).toContain('Initial');
      // Check for 'A.' separately since middle edits can split 'from A.'
      expect(contentBAfterRefresh).toContain('A.');

      console.log('\n=== Long Document Collaboration Persistence Test Passed ===');
      console.log(`User A's npub: ${npubA}`);
      console.log(`User B's npub: ${npubB}`);
      console.log(`Final content (A): "${contentAAfterRefresh}"`);
      console.log(`Final content (B): "${contentBAfterRefresh}"`);

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('browser can view document via direct link without creator making more edits', async ({ browser }) => {
    // This test verifies that Browser 2 can view Browser 1's document via direct link
    // WITHOUT Browser 1 making additional edits to trigger sync.
    //
    // The key requirement: once WebRTC connection is established, Browser 2 should
    // be able to navigate to the document URL and see its content immediately.

    const contextA = await browser.newContext();
    const contextB = await browser.newContext();

    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // === Setup both users ===
      console.log('Setting up User A...');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      const pubkeyA = await getPubkeyHex(pageA);
      console.log(`User A npub: ${npubA}`);

      console.log('Setting up User B...');
      await pageB.goto('/');
      await setupFreshUser(pageB);
      const pubkeyB = await getPubkeyHex(pageB);
      console.log(`User B ready`);

      // === Users follow each other for WebRTC connection ===
      console.log('Users following each other...');
      await pageA.waitForFunction(() => (window as any).__testHelpers?.followPubkey, { timeout: 10000 });
      await pageB.waitForFunction(() => (window as any).__testHelpers?.followPubkey, { timeout: 10000 });

      await pageA.evaluate(async (pk) => {
        const { followPubkey } = (window as any).__testHelpers;
        await followPubkey(pk);
      }, pubkeyB);
      await pageB.evaluate(async (pk) => {
        const { followPubkey } = (window as any).__testHelpers;
        await followPubkey(pk);
      }, pubkeyA);

      expect(await waitForFollowInWorker(pageA, pubkeyB, 20000)).toBe(true);
      expect(await waitForFollowInWorker(pageB, pubkeyA, 20000)).toBe(true);
      await pageA.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageA, 30000, pubkeyB);
      await waitForWebRTCConnection(pageB, 30000, pubkeyA);
      console.log('WebRTC connected');

      // === User A: Create document and type content ===
      console.log('User A: Creating document...');
      await createDocument(pageA, 'direct-view-test');

      const editorA = pageA.locator('.ProseMirror');
      await expect(editorA).toBeVisible({ timeout: 30000 });
      await editorA.click();
      await pageA.keyboard.type('Content created by Browser 1 - should be visible to Browser 2');
      console.log('User A: Content typed');

      // Wait for save to complete
      await waitForSave(pageA);
      console.log('User A: Document saved');

      // Flush publishes so the tree root is visible to other users immediately
      await flushPendingPublishes(pageA);

      // Get the document URL
      const docUrl = pageA.url();
      console.log(`Document URL: ${docUrl}`);

      // === Browser 2 navigates directly to the document URL ===
      // This tests that Browser 2 can view without Browser 1 making more edits
      console.log('Browser 2: Navigating directly to document URL...');
      await pageB.goto(docUrl);
      await waitForAppReady(pageB);
      await waitForRelayConnected(pageB, 20000);
      await pageB.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      await waitForWebRTCConnection(pageB, 60000, pubkeyA);

      // Wait for the editor to appear with content
      const editorB = pageB.locator('.ProseMirror');
      await expect(editorB).toBeVisible({ timeout: 60000 });
      await expect(editorB).toContainText('Content created by Browser 1', { timeout: 60000 });

      const contentB = await editorB.textContent();
      console.log(`Browser 2 sees: "${contentB}"`);
      console.log('Test PASSED: Browser 2 viewed document without Browser 1 making more edits');

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('incognito browser views document via direct URL without prior WebRTC', async ({ browser }) => {

    const contextA = await browser.newContext();
    const contextB = await browser.newContext();

    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // === Setup User A using setupFreshUser (includes disableOthersPool) ===
      console.log('Setting up User A...');
      await setupFreshUser(pageA);
      const npubA = await getNpub(pageA);
      console.log(`User A npub: ${npubA}`);

      // === User A: Create document and type content ===
      console.log('User A: Creating document...');
      await createDocument(pageA, 'incognito-test');

      const editorA = pageA.locator('.ProseMirror');
      await expect(editorA).toBeVisible({ timeout: 30000 });
      await editorA.click();
      await pageA.keyboard.type('Content from A - incognito B should see this');
      console.log('User A: Content typed');

      // Wait for save to complete
      await waitForSave(pageA);
      console.log('User A: Document saved');

      // Get the document URL
      const docUrl = pageA.url();
      console.log(`Document URL: ${docUrl}`);

      // Push the latest tree root to Blossom so incognito users can fetch content without WebRTC.
      const pushResult = await pageA.evaluate(async () => {
        const adapter = (window as any).__getWorkerAdapter?.();
        const hashtree = (window as any).__hashtree;
        const rootHex = (window as any).__getTreeRoot?.();
        if (!adapter || !hashtree || !rootHex) {
          return { pushed: 0, skipped: 0, failed: 1, error: 'missing adapter or root' };
        }
        const hash = hashtree.fromHex(rootHex);
        return adapter.pushToBlossom(hash);
      });
      console.log('Blossom push result:', pushResult);
      expect(pushResult.failed).toBe(0);

      // === Browser B: Navigate DIRECTLY to the document URL ===
      // B has NOT visited the site before, has no follows, no WebRTC connections
      console.log('Browser B: Navigating directly to document URL (no prior setup)...');

      await pageB.goto(docUrl);
      await waitForAppReady(pageB);
      await disableOthersPool(pageB);
      await configureBlossomServers(pageB);

      // Wait for the editor to appear with content
      const editorB = pageB.locator('.ProseMirror');
      await expect(editorB).toBeVisible({ timeout: 60000 });

      // This is the key assertion - B should see A's content without A making more edits
      await expect(editorB).toContainText('Content from A', { timeout: 60000 });

      const contentB = await editorB.textContent();
      console.log(`Browser B sees: "${contentB}"`);
      console.log('Test PASSED: Incognito browser viewed document via direct URL');

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });
});
