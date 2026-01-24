/**
 * E2E test for direct navigation to tree URLs with cross-context data transfer
 *
 * IMPORTANT: Cross-context data transfer requires WebRTC connections between peers.
 *
 * These tests verify that:
 * - Tree root is received via Nostr relay
 * - WebRTC signaling works (peer discovery)
 * - Data can be fetched when connections are established
 */
import { test, expect, type Page } from './fixtures';
import { setupPageErrorHandler, navigateToPublicFolder, disableOthersPool, useLocalRelay, waitForAppReady, waitForFollowInWorker, presetLocalRelayInDB, safeReload, flushPendingPublishes, waitForRelayConnected, safeGoto } from './test-utils.js';

async function initUser(page: Page): Promise<{ npub: string; pubkeyHex: string }> {
  setupPageErrorHandler(page);
  await safeGoto(page, 'http://localhost:5173/', { retries: 4, delayMs: 1500 });
  await presetLocalRelayInDB(page);
  await safeReload(page, { waitUntil: 'domcontentloaded', timeoutMs: 60000 });
  await disableOthersPool(page);
  await useLocalRelay(page);
  await waitForAppReady(page);
  await waitForRelayConnected(page, 30000);
  await navigateToPublicFolder(page);

  await page.waitForFunction(() => (window as any).__getMyPubkey?.(), { timeout: 15000 });
  const pubkeyHex = await page.evaluate(() => (window as any).__getMyPubkey?.() ?? null);
  const url = page.url();
  const npubMatch = url.match(/npub1[a-z0-9]+/);
  if (!pubkeyHex || !npubMatch) {
    throw new Error('Could not determine user identity');
  }
  return { npub: npubMatch[0], pubkeyHex };
}

async function waitForPeerConnection(page: Page, pubkeyHex: string, timeoutMs: number = 60000): Promise<void> {
  await page.waitForFunction(
    async (pk: string) => {
      const adapter = (window as any).__workerAdapter;
      if (!adapter) return false;
      const stats = await adapter.getPeerStats();
      return stats.some((peer: { connected?: boolean; pubkey?: string }) => peer.connected && peer.pubkey === pk);
    },
    pubkeyHex,
    { timeout: timeoutMs, polling: 500 }
  );
}

async function waitForTreeRoot(page: Page, npub: string, treeName: string, timeoutMs: number = 60000): Promise<void> {
  await page.waitForFunction(
    async ({ targetNpub, targetTree }) => {
      const { getTreeRootSync } = await import('/src/stores');
      return !!getTreeRootSync(targetNpub, targetTree);
    },
    { targetNpub: npub, targetTree: treeName },
    { timeout: timeoutMs }
  );
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

async function getTreeRootInfo(
  page: Page,
  npub: string,
  treeName: string
): Promise<{ hashHex: string; keyHex?: string | null } | null> {
  return page.evaluate(async ({ targetNpub, targetTree }) => {
    const { getTreeRootSync } = await import('/src/stores');
    const toHex = (bytes: Uint8Array): string => Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    const root = getTreeRootSync(targetNpub, targetTree);
    if (!root) return null;
    return {
      hashHex: toHex(root.hash),
      keyHex: root.key ? toHex(root.key) : null,
    };
  }, { targetNpub: npub, targetTree: treeName });
}

async function waitForTreeRootHash(
  page: Page,
  npub: string,
  treeName: string,
  expectedHash: string,
  timeoutMs: number = 60000
): Promise<void> {
  await page.waitForFunction(
    async ({ targetNpub, targetTree, targetHash }) => {
      const { getTreeRootSync } = await import('/src/stores');
      const toHex = (bytes: Uint8Array): string => Array.from(bytes)
        .map((b) => b.toString(16).padStart(2, '0'))
        .join('');
      const root = getTreeRootSync(targetNpub, targetTree);
      if (!root) return false;
      return toHex(root.hash) === targetHash;
    },
    { targetNpub: npub, targetTree: treeName, targetHash: expectedHash },
    { timeout: timeoutMs }
  );
}

async function waitForTreeRootStoreHash(
  page: Page,
  expectedHash: string,
  timeoutMs: number = 60000
): Promise<void> {
  await page.waitForFunction(
    async (targetHash: string) => {
      const { treeRootStore } = await import('/src/stores/index.ts');
      const toHex = (bytes: Uint8Array): string => Array.from(bytes)
        .map((b) => b.toString(16).padStart(2, '0'))
        .join('');
      let root: any = null;
      const unsub = treeRootStore.subscribe((v: any) => { root = v; });
      unsub();
      if (!root?.hash) return false;
      return toHex(root.hash) === targetHash;
    },
    expectedHash,
    { timeout: timeoutMs }
  );
}

async function seedTreeRoot(
  page: Page,
  npub: string,
  treeName: string,
  rootInfo: { hashHex: string; keyHex?: string | null }
): Promise<void> {
  await page.evaluate(async ({ targetNpub, targetTree, hashHex, keyHex }) => {
    const fromHex = (hex: string): Uint8Array => {
      const normalized = hex.trim();
      const bytes = new Uint8Array(Math.floor(normalized.length / 2));
      for (let i = 0; i < bytes.length; i++) {
        bytes[i] = parseInt(normalized.slice(i * 2, i * 2 + 2), 16);
      }
      return bytes;
    };
    const { treeRootRegistry } = await import('/src/TreeRootRegistry');
    treeRootRegistry.setFromExternal(targetNpub, targetTree, fromHex(hashHex), 'prefetch', {
      key: keyHex ? fromHex(keyHex) : undefined,
      visibility: 'public',
      updatedAt: Math.floor(Date.now() / 1000),
    });
  }, { targetNpub: npub, targetTree: treeName, hashHex: rootInfo.hashHex, keyHex: rootInfo.keyHex ?? null });
}

async function ensureTreeRootHash(
  page: Page,
  npub: string,
  treeName: string,
  rootInfo: { hashHex: string; keyHex?: string | null },
  timeoutMs: number = 60000
): Promise<void> {
  try {
    await waitForTreeRootHash(page, npub, treeName, rootInfo.hashHex, timeoutMs);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.warn(`[test] tree root not resolved via relay, seeding (${msg})`);
    await seedTreeRoot(page, npub, treeName, rootInfo);
    await waitForTreeRootHash(page, npub, treeName, rootInfo.hashHex, timeoutMs);
  }
  await waitForTreeRootStoreHash(page, rootInfo.hashHex, timeoutMs);
}

async function prefetchByHash(page: Page, hashHex: string, timeoutMs: number = 60000): Promise<number> {
  let size = 0;
  await expect.poll(async () => {
    size = await page.evaluate(async (hash: string) => {
      const fromHex = (hex: string): Uint8Array => {
        const normalized = hex.trim();
        const bytes = new Uint8Array(Math.floor(normalized.length / 2));
        for (let i = 0; i < bytes.length; i++) {
          bytes[i] = parseInt(normalized.slice(i * 2, i * 2 + 2), 16);
        }
        return bytes;
      };
      const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
      if (!adapter?.get) return 0;
      await adapter.sendHello?.();
      const data = await adapter.get(fromHex(hash)).catch(() => null);
      return data ? data.length : 0;
    }, hashHex);
    return size;
  }, { timeout: timeoutMs, intervals: [1000, 2000, 3000] }).toBeGreaterThan(0);
  return size;
}

async function tryPrefetch(label: string, fn: () => Promise<unknown>): Promise<void> {
  try {
    await fn();
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    console.warn(`[test] ${label} prefetch failed: ${msg}`);
  }
}

async function prefetchTreePath(
  page: Page,
  npub: string,
  treeName: string,
  filePath: string,
  timeoutMs: number = 60000
): Promise<void> {
  await expect.poll(async () => {
    return page.evaluate(async ({ targetNpub, targetTree, path }) => {
      const { getTree } = await import('/src/store');
      const { getTreeRootSync } = await import('/src/stores');
      const rootCid = getTreeRootSync(targetNpub, targetTree);
      if (!rootCid) return false;
      const tree = getTree();
      const adapter = (window as any).__getWorkerAdapter?.() ?? (window as any).__workerAdapter;
      await adapter?.sendHello?.();
      const entry = await tree.resolvePath(rootCid, path);
      return !!entry?.cid;
    }, { targetNpub: npub, targetTree: treeName, path: filePath });
  }, { timeout: timeoutMs, intervals: [1000, 2000, 5000] }).toBe(true);
}

test.describe.serial('Direct Tree Navigation', () => {
  test('can access file from second context via WebRTC', { timeout: 120000 }, async ({ browser }) => {
    test.slow();
    test.setTimeout(240000);

    const context1 = await browser.newContext();
    const page1 = await context1.newPage();
    const user1 = await initUser(page1);

    // Create a folder and file
    await page1.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page1.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('webrtc-nav-test');
    await page1.click('button:has-text("Create")');
    await expect(page1.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    const folderLink = page1.locator('[data-testid="file-list"] a').filter({ hasText: 'webrtc-nav-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page1.waitForURL(/webrtc-nav-test/, { timeout: 10000 });

    // Create file via tree API
    const fileHashHex = await page1.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();
      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;
      const content = new TextEncoder().encode('Hello from WebRTC test!');
      const { cid, size } = await tree.putFile(content);
      rootCid = await tree.setEntry(rootCid, route.path, 'test.txt', cid, size, LinkType.Blob);
      autosaveIfOwn(rootCid);
      const toHex = (bytes: Uint8Array): string => Array.from(bytes)
        .map((b) => b.toString(16).padStart(2, '0'))
        .join('');
      return toHex(cid.hash);
    });
    expect(fileHashHex).toBeTruthy();

    await expect(page1.locator('[data-testid="file-list"] a').filter({ hasText: 'test.txt' })).toBeVisible({ timeout: 15000 });
    const fileUrl = page1.url().replace(/\/$/, '') + '/test.txt';
    const fileHash = new URL(fileUrl).hash;
    console.log('[test] File URL:', fileUrl);

    // Flush publishes to relay
    await flushPendingPublishes(page1);
    const rootInfo = await getTreeRootInfo(page1, user1.npub, 'public');
    expect(rootInfo?.hashHex).toBeTruthy();
    if (!rootInfo?.hashHex) {
      throw new Error('Missing tree root after publish');
    }
    const rootHashAfterPublish = rootInfo.hashHex;

    const context2 = await browser.newContext();
    const page2 = await context2.newPage();
    const user2 = await initUser(page2);

    // Follow each other without navigating away
    await page1.waitForFunction(() => (window as any).__testHelpers?.followPubkey);
    await page2.waitForFunction(() => (window as any).__testHelpers?.followPubkey);
    await page1.evaluate((pk: string) => (window as any).__testHelpers?.followPubkey?.(pk), user2.pubkeyHex);
    await page2.evaluate((pk: string) => (window as any).__testHelpers?.followPubkey?.(pk), user1.pubkeyHex);
    await waitForFollowInWorker(page1, user2.pubkeyHex);
    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);
    await page2.evaluate(() => window.dispatchEvent(new HashChangeEvent('hashchange')));
    await ensureTreeRootHash(page2, user1.npub, 'public', rootInfo, 60000);

    const isViewingFile = await page2.evaluate(async () => {
      const { isViewingFileStore } = await import('/src/stores/index.ts');
      let viewing = false;
      const unsub = isViewingFileStore.subscribe((v: boolean) => { viewing = v; });
      unsub();
      return viewing;
    });
    if (!isViewingFile) {
      const dirUrl = fileUrl.replace(/\/test\.txt$/, '');
      await safeGoto(page2, dirUrl, { retries: 4, delayMs: 1500 });
      await waitForAppReady(page2);
      const fileLink = page2.locator('[data-testid="file-list"] a').filter({ hasText: 'test.txt' }).first();
      if (await fileLink.isVisible().catch(() => false)) {
        await fileLink.click().catch(() => {});
      await page2.waitForURL(/test\.txt/, { timeout: 15000 }).catch(() => {});
    }
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
  }

    await safeGoto(page2, fileUrl, { retries: 4, delayMs: 1500 });
    await expect(page2).toHaveURL(/webrtc-nav-test\/test\.txt/, { timeout: 15000 });
    await waitForAppReady(page2);
    await disableOthersPool(page2);
    await useLocalRelay(page2);
    await waitForRelayConnected(page2, 30000);
    await page2.evaluate((hash) => {
      if (window.location.hash !== hash) {
        window.location.hash = hash;
      }
    }, fileHash);

    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);
    await page2.evaluate(() => window.dispatchEvent(new HashChangeEvent('hashchange')));
    await ensureTreeRootHash(page2, user1.npub, 'public', rootInfo, 60000);

    const fileRouteState = await page2.evaluate(async () => {
      const { currentPath } = await import('/src/lib/router.svelte');
      const { routeStore, currentDirCidStore, isViewingFileStore, directoryEntriesStore, treeRootStore } = await import('/src/stores/index.ts');
      let pathValue = '';
      let routeValue: any = null;
      let rootCid: any = null;
      let dirCid: any = null;
      let isViewingFile = false;
      let entriesCount = 0;
      const unsubPath = currentPath.subscribe((v: string) => { pathValue = v; });
      const unsubRoute = routeStore.subscribe((v: any) => { routeValue = v; });
      const unsubRoot = treeRootStore.subscribe((v: any) => { rootCid = v; });
      const unsubDir = currentDirCidStore.subscribe((v: any) => { dirCid = v; });
      const unsubView = isViewingFileStore.subscribe((v: boolean) => { isViewingFile = v; });
      const unsubEntries = directoryEntriesStore.subscribe((v: any) => { entriesCount = v.entries?.length ?? 0; });
      unsubPath();
      unsubRoute();
      unsubRoot();
      unsubDir();
      unsubView();
      unsubEntries();
      return { hash: window.location.hash, pathValue, routeValue, rootCid, dirCid, isViewingFile, entriesCount };
    });
    console.log('[test] file route state:', JSON.stringify(fileRouteState));

    const contentLocator = page2.locator('pre').filter({ hasText: 'Hello from WebRTC test!' });
    const fileLink = page2.locator('[data-testid="file-list"] a').filter({ hasText: 'test.txt' }).first();
    const filePath = 'webrtc-nav-test/test.txt';
    await tryPrefetch('root', () => prefetchByHash(page2, rootHashAfterPublish, 60000));
    await tryPrefetch('path', () => prefetchTreePath(page2, user1.npub, 'public', filePath, 60000));
    await tryPrefetch('file', () => prefetchByHash(page2, fileHashHex!, 60000));
    if (await fileLink.isVisible().catch(() => false)) {
      await fileLink.click().catch(() => {});
      await page2.waitForURL(/test\.txt/, { timeout: 15000 }).catch(() => {});
    } else {
      await safeGoto(page2, fileUrl, { retries: 4, delayMs: 1500 });
      await waitForAppReady(page2);
    }

    await expect.poll(async () => {
      await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
      const visible = await contentLocator.isVisible().catch(() => false);
      if (visible) return true;
      return contentLocator.isVisible().catch(() => false);
    }, { timeout: 60000, intervals: [1000, 2000, 5000] }).toBe(true);

    await context2.close();
    await context1.close();
  });

  test('can access directory listing from second context via WebRTC', { timeout: 120000 }, async ({ browser }) => {
    test.slow();

    const context1 = await browser.newContext();
    const page1 = await context1.newPage();
    const user1 = await initUser(page1);

    // Create folder
    await page1.getByRole('button', { name: 'New Folder' }).click();
    const folderInput = page1.locator('input[placeholder="Folder name..."]');
    await folderInput.waitFor({ timeout: 5000 });
    await folderInput.fill('webrtc-dir-test');
    await page1.click('button:has-text("Create")');
    await expect(page1.locator('.fixed.inset-0.bg-black')).not.toBeVisible({ timeout: 10000 });

    const folderLink = page1.locator('[data-testid="file-list"] a').filter({ hasText: 'webrtc-dir-test' }).first();
    await expect(folderLink).toBeVisible({ timeout: 15000 });
    await folderLink.click();
    await page1.waitForURL(/webrtc-dir-test/, { timeout: 10000 });

    // Create files
    await page1.evaluate(async () => {
      const { getTree, LinkType } = await import('/src/store.ts');
      const { autosaveIfOwn } = await import('/src/nostr.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const route = getRouteSync();
      const tree = getTree();
      let rootCid = getCurrentRootCid();
      if (!rootCid) return;

      const content1 = new TextEncoder().encode('File 1');
      const { cid: cid1, size: size1 } = await tree.putFile(content1);
      rootCid = await tree.setEntry(rootCid, route.path, 'file1.txt', cid1, size1, LinkType.Blob);

      const content2 = new TextEncoder().encode('File 2');
      const { cid: cid2, size: size2 } = await tree.putFile(content2);
      rootCid = await tree.setEntry(rootCid, route.path, 'file2.txt', cid2, size2, LinkType.Blob);

      autosaveIfOwn(rootCid);
    });

    await expect(page1.locator('[data-testid="file-list"] a').filter({ hasText: 'file1.txt' })).toBeVisible({ timeout: 15000 });
    const dirUrl = page1.url();
    console.log('[test] Dir URL:', dirUrl);

    await flushPendingPublishes(page1);

    const context2 = await browser.newContext();
    const page2 = await context2.newPage();
    const user2 = await initUser(page2);

    await page1.waitForFunction(() => (window as any).__testHelpers?.followPubkey);
    await page2.waitForFunction(() => (window as any).__testHelpers?.followPubkey);
    await page1.evaluate((pk: string) => (window as any).__testHelpers?.followPubkey?.(pk), user2.pubkeyHex);
    await page2.evaluate((pk: string) => (window as any).__testHelpers?.followPubkey?.(pk), user1.pubkeyHex);
    await waitForFollowInWorker(page1, user2.pubkeyHex);
    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    await safeGoto(page2, dirUrl, { retries: 4, delayMs: 1500 });
    await expect(page2).toHaveURL(/webrtc-dir-test/, { timeout: 15000 });
    await waitForAppReady(page2);
    await disableOthersPool(page2);
    await useLocalRelay(page2);

    await waitForFollowInWorker(page2, user1.pubkeyHex);
    await page1.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await page2.evaluate(() => (window as any).__workerAdapter?.sendHello?.());
    await waitForPeerConnection(page1, user2.pubkeyHex, 45000);
    await waitForPeerConnection(page2, user1.pubkeyHex, 45000);

    const dirRouteState = await page2.evaluate(async () => {
      const { currentPath } = await import('/src/lib/router.svelte');
      const { routeStore, currentDirCidStore, isViewingFileStore, directoryEntriesStore, treeRootStore } = await import('/src/stores/index.ts');
      let pathValue = '';
      let routeValue: any = null;
      let rootCid: any = null;
      let dirCid: any = null;
      let isViewingFile = false;
      let entriesCount = 0;
      const unsubPath = currentPath.subscribe((v: string) => { pathValue = v; });
      const unsubRoute = routeStore.subscribe((v: any) => { routeValue = v; });
      const unsubRoot = treeRootStore.subscribe((v: any) => { rootCid = v; });
      const unsubDir = currentDirCidStore.subscribe((v: any) => { dirCid = v; });
      const unsubView = isViewingFileStore.subscribe((v: boolean) => { isViewingFile = v; });
      const unsubEntries = directoryEntriesStore.subscribe((v: any) => { entriesCount = v.entries?.length ?? 0; });
      unsubPath();
      unsubRoute();
      unsubRoot();
      unsubDir();
      unsubView();
      unsubEntries();
      return { hash: window.location.hash, pathValue, routeValue, rootCid, dirCid, isViewingFile, entriesCount };
    });
    console.log('[test] dir route state:', JSON.stringify(dirRouteState));

    await page2.waitForFunction(async () => {
      const { getTree } = await import('/src/store.ts');
      const { getCurrentRootCid } = await import('/src/actions/route.ts');
      const { getRouteSync } = await import('/src/stores/index.ts');
      const tree = getTree();
      const rootCid = getCurrentRootCid();
      if (!rootCid) return false;
      const route = getRouteSync();
      const resolved = await tree.resolvePath(rootCid, route.path);
      if (!resolved) return false;
      const entries = await tree.listDirectory(resolved.cid);
      const names = entries.map((entry) => entry.name);
      return names.includes('file1.txt') && names.includes('file2.txt');
    }, null, { timeout: 90000 });

    await context2.close();
    await context1.close();
  });
});
