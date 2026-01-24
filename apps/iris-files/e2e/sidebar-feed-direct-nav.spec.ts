/**
 * E2E test for sidebar feed on direct video navigation
 *
 * BUG: When navigating directly to a video (via npub/path),
 * the sidebar feed is empty because feedStore is only populated
 * by VideoHome which never loads on direct navigation.
 */
import { test, expect } from './fixtures';
import { setupPageErrorHandler, waitForAppReady, ensureLoggedIn, disableOthersPool, useLocalRelay, waitForRelayConnected, flushPendingPublishes } from './test-utils.js';

async function createSidebarVideos(page: any, suffix: string): Promise<{ npub: string; treeNames: string[] }> {
  return page.evaluate(async ({ suffix }) => {
    const { getTree } = await import('/src/store.ts');
    const { saveHashtree, nostrStore } = await import('/src/nostr');
    const hashtree = await import('/src/lib/nhash.ts');
    const { videoChunker, cid } = hashtree;

    const state = nostrStore.getState();
    if (!state.npub) {
      throw new Error('No npub available for test user');
    }

    const tree = getTree();
    const labels = [`Sidebar Direct A ${suffix}`, `Sidebar Direct B ${suffix}`];
    const treeNames: string[] = [];

    for (const label of labels) {
      const videoEntries: Array<{ name: string; cid: any; size: number }> = [];
      const videoData = new Uint8Array([0x00, 0x00, 0x00, 0x1C, 0x66, 0x74, 0x79, 0x70]);
      const streamWriter = tree.createStream({ chunker: videoChunker() });
      await streamWriter.append(videoData);
      const videoResult = await streamWriter.finalize();
      videoEntries.push({
        name: 'video.mp4',
        cid: cid(videoResult.hash, videoResult.key),
        size: videoResult.size,
      });

      const titleData = new TextEncoder().encode(label);
      const titleResult = await tree.putFile(titleData, {});
      videoEntries.push({ name: 'title.txt', cid: titleResult.cid, size: titleResult.size });

      const dirResult = await tree.putDirectory(videoEntries, {});
      const treeName = `videos/${label}`;
      const publishResult = await saveHashtree(treeName, dirResult.cid, { visibility: 'public' });
      if (!publishResult.success) {
        throw new Error(`Failed to publish ${treeName}`);
      }
      treeNames.push(treeName);
    }

    return { npub: state.npub, treeNames };
  }, { suffix });
}

test.describe('Sidebar Feed Direct Navigation', () => {
  test('sidebar feed loads on direct npub/path navigation', async ({ page }) => {
    test.slow();

    setupPageErrorHandler(page);

    await page.goto('/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await ensureLoggedIn(page);
    await useLocalRelay(page);
    await waitForRelayConnected(page);

    const suffix = Date.now().toString(36);
    const { npub, treeNames } = await createSidebarVideos(page, suffix);
    await flushPendingPublishes(page);
    const [treeNameA, treeNameB] = treeNames;
    const titleA = treeNameA.replace('videos/', '');
    const titleB = treeNameB.replace('videos/', '');

    const videoUrl = `/video.html#/${npub}/${encodeURIComponent(treeNameA)}`;
    const pageLoadStart = Date.now();
    await page.goto(videoUrl);
    await waitForAppReady(page);

    await expect(page.getByTestId('video-title')).toHaveText(titleA, { timeout: 20000 });

    const sidebarVideo = page.locator('a.flex.gap-2.group', { hasText: titleB }).first();
    await expect(sidebarVideo).toBeVisible({ timeout: 15000 });

    const feedLoadTime = Date.now() - pageLoadStart;
    expect(feedLoadTime).toBeLessThan(20000);
  });
});
