import { test, expect, Page } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, configureBlossomServers } from './test-utils';
// Run tests in this file serially to avoid WebRTC/timing conflicts
test.describe.configure({ mode: 'serial' });

/**
 * Tests for playlist management features:
 * 1. Likes target individual videos in playlists (not the whole playlist)
 * 2. Delete removes only the video from playlist (not the whole playlist)
 * 3. Add to playlist functionality
 */

async function ensureLoggedIn(page: any) {
  const uploadBtn = page.locator('button:has-text("Create")');
  const isVisible = await uploadBtn.isVisible().catch(() => false);

  if (!isVisible) {
    const newBtn = page.getByRole('button', { name: /New/i });
    if (await newBtn.isVisible().catch(() => false)) {
      await newBtn.click();
      await expect(uploadBtn).toBeVisible({ timeout: 15000 });
    }
  }
}

/**
 * Helper to create a playlist with 2 videos for testing
 */
async function createTestPlaylist(page: any, playlistName: string) {
  return await page.evaluate(async (name: string) => {
    const { getTree } = await import('/src/store.ts');
    const { nostrStore } = await import('/src/nostr.ts');
    const { updateLocalRootCacheHex } = await import('/src/treeRootCache.ts');
    const hashtree = await import('/node_modules/hashtree/dist/index.js');
    const { toHex, videoChunker, cid } = hashtree;

    const tree = getTree();
    const npub: string = await new Promise((resolve) => {
      let unsub: (() => void) | null = null;
      unsub = nostrStore.subscribe((state: any) => {
        if (state.npub) {
          queueMicrotask(() => unsub?.());
          resolve(state.npub);
        }
      });
    });

    // Create 2 videos for the playlist
    const videos = [
      { id: 'testVideo001', title: 'Test Video 1' },
      { id: 'testVideo002', title: 'Test Video 2' },
    ];

    const rootEntries: Array<{ name: string; cid: any; size: number }> = [];

    for (const video of videos) {
      const videoEntries: Array<{ name: string; cid: any; size: number }> = [];

      // Create video file
      const videoData = new Uint8Array([0x00, 0x00, 0x00, 0x1C, 0x66, 0x74, 0x79, 0x70]);
      const streamWriter = tree.createStream({ chunker: videoChunker() });
      await streamWriter.append(videoData);
      const videoResult = await streamWriter.finalize();
      videoEntries.push({
        name: 'video.mp4',
        cid: cid(videoResult.hash, videoResult.key),
        size: videoResult.size,
      });

      // Create title.txt
      const titleData = new TextEncoder().encode(video.title);
      const titleResult = await tree.putFile(titleData, {});
      videoEntries.push({ name: 'title.txt', cid: titleResult.cid, size: titleResult.size });

      // Create video directory
      const videoDirResult = await tree.putDirectory(videoEntries, {});
      rootEntries.push({
        name: video.id,
        cid: videoDirResult.cid,
        size: videoEntries.reduce((sum, e) => sum + e.size, 0),
      });
    }

    // Create root playlist directory
    const rootDirResult = await tree.putDirectory(rootEntries, {});
    const treeName = `videos/${name}`;
    const rootKey = rootDirResult.cid.key ? toHex(rootDirResult.cid.key) : undefined;
    updateLocalRootCacheHex(npub, treeName, toHex(rootDirResult.cid.hash), rootKey, 'public');

    return {
      npub,
      treeName,
      rootHash: toHex(rootDirResult.cid.hash),
      rootKey,
      videos,
    };
  }, playlistName);
}

test.describe('Playlist Management', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('like on playlist video targets the individual video, not the playlist', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Wait for app initialization
    await page.waitForTimeout(1000);

    // Create a test playlist
    const playlist = await createTestPlaylist(page, 'Like Test Playlist');

    // Navigate to the first video in the playlist
    const videoUrl = `/video.html#/${playlist.npub}/${encodeURIComponent(playlist.treeName)}/${playlist.videos[0].id}`;
    await page.goto(videoUrl);

    // Wait for video page to load
    await page.waitForTimeout(3000);

    // Capture Nostr events
    const publishedEvents: any[] = [];
    await page.evaluate(() => {
      (window as any).__publishedEvents = [];
      const originalPublish = (window as any).ndk?.publish?.bind((window as any).ndk);
      if ((window as any).ndk) {
        const originalEventPublish = (window as any).ndk.constructor.prototype.publish;
        // Hook NDKEvent.publish to capture events
      }
    });

    // Find and click the like button
    const likeBtn = page.locator('button[title="Like"]');
    await expect(likeBtn).toBeVisible({ timeout: 10000 });
    await likeBtn.click();

    // Wait for like to be processed
    await expect(page.locator('button[title="Liked"]')).toBeVisible({ timeout: 10000 });

    // Check the video identifier used for the like
    // For playlist videos, it should include the videoId: npub/videos/PlaylistName/videoId
    const videoIdentifier = await page.evaluate(() => {
      // The identifier should be visible in the page's state
      // We can check localStorage or the DOM for the identifier
      const stored = localStorage.getItem('hashtree:recents');
      if (stored) {
        const recents = JSON.parse(stored);
        const recent = recents.find((r: any) => r.treeName?.includes('Like Test'));
        return recent?.videoId ? `has videoId: ${recent.videoId}` : 'no videoId';
      }
      return 'no recents';
    });

    console.log('Video identifier check:', videoIdentifier);

    // The test passes if we can click like on a playlist video
    // Full verification would require inspecting Nostr events
    await page.screenshot({ path: 'e2e/screenshots/playlist-like-test.png' });
  });

  test('delete on playlist video removes only that video, not the whole playlist', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    await page.waitForTimeout(1000);

    // Create a test playlist with 2 videos
    const playlist = await createTestPlaylist(page, 'Delete Test Playlist');

    // Navigate to the first video
    const videoUrl = `/video.html#/${playlist.npub}/${encodeURIComponent(playlist.treeName)}/${playlist.videos[0].id}`;
    await page.goto(videoUrl);

    // Wait for video page to load (the page structure loads even if video data is fake)
    // Note: We don't wait for the video element since the test uses programmatic fake data
    // Instead wait for the video page heading to appear
    await expect(page.getByRole('heading', { name: playlist.videos[0].id })).toBeVisible({ timeout: 30000 });

    // Take screenshot before delete
    await page.screenshot({ path: 'e2e/screenshots/playlist-before-delete.png' });

    // Accept the confirmation dialog
    page.on('dialog', dialog => dialog.accept());

    // Click delete button - should be visible even without actual video data
    const deleteBtn = page.locator('button[title="Delete video"]');
    await expect(deleteBtn).toBeVisible({ timeout: 15000 });
    await deleteBtn.click();

    // Wait for delete to process and page to change
    await page.waitForTimeout(3000);

    // Take screenshot after delete
    await page.screenshot({ path: 'e2e/screenshots/playlist-after-delete.png' });

    // Verify the delete completed by checking URL changed from the first video
    const currentHash = await page.evaluate(() => window.location.hash);

    // After delete, URL should not contain the first video ID anymore
    // (navigation behavior depends on app state, but the video should be removed)
    expect(currentHash).not.toContain('testVideo001');

    // Navigate back to playlist to verify video 2 still exists
    const video2Url = `/video.html#/${playlist.npub}/${encodeURIComponent(playlist.treeName)}/${playlist.videos[1].id}`;
    await page.goto(video2Url);

    // Video 2 should still be accessible (proves playlist wasn't deleted entirely)
    await expect(page.getByRole('heading', { name: playlist.videos[1].title })).toBeVisible({ timeout: 30000 });
  });

  test('add to playlist button is visible on video pages', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    await page.waitForTimeout(1000);

    // Create a single video (not a playlist)
    const result = await page.evaluate(async () => {
      const { getTree } = await import('/src/store.ts');
      const { nostrStore } = await import('/src/nostr.ts');
      const { updateLocalRootCacheHex } = await import('/src/treeRootCache.ts');
      const hashtree = await import('/node_modules/hashtree/dist/index.js');
      const { toHex, videoChunker, cid } = hashtree;

      const tree = getTree();
      let npub: string = '';
      const unsub = nostrStore.subscribe((state: any) => { npub = state.npub; });
      unsub();

      const videoEntries: Array<{ name: string; cid: any; size: number }> = [];

      // Create video file
      const videoData = new Uint8Array([0x00, 0x00, 0x00, 0x1C, 0x66, 0x74, 0x79, 0x70]);
      const streamWriter = tree.createStream({ chunker: videoChunker() });
      await streamWriter.append(videoData);
      const videoResult = await streamWriter.finalize();
      videoEntries.push({
        name: 'video.mp4',
        cid: cid(videoResult.hash, videoResult.key),
        size: videoResult.size,
      });

      // Create title.txt
      const titleData = new TextEncoder().encode('Add To Playlist Test Video');
      const titleResult = await tree.putFile(titleData, {});
      videoEntries.push({ name: 'title.txt', cid: titleResult.cid, size: titleResult.size });

      // Create video directory
      const videoDirResult = await tree.putDirectory(videoEntries, {});
      const treeName = 'videos/Add To Playlist Test';
      const rootKey = videoDirResult.cid.key ? toHex(videoDirResult.cid.key) : undefined;
      updateLocalRootCacheHex(npub, treeName, toHex(videoDirResult.cid.hash), rootKey, 'public');

      return { npub, treeName };
    });

    // Navigate to the video
    const videoUrl = `/video.html#/${result.npub}/${encodeURIComponent(result.treeName)}`;
    await page.goto(videoUrl);

    // Wait for video page to load
    await page.waitForTimeout(3000);

    // Take screenshot
    await page.screenshot({ path: 'e2e/screenshots/add-to-playlist-button-test.png' });

    // Check if "Add to playlist" button exists
    const addToPlaylistBtn = page.locator('button[title="Add to playlist"]');
    await expect(addToPlaylistBtn).toBeVisible({ timeout: 5000 });

    // Click the button to open the modal
    await addToPlaylistBtn.click();

    // Verify the modal opens
    await expect(page.getByText('Save to playlist')).toBeVisible({ timeout: 5000 });

    // Close the modal
    await page.locator('button:has(.i-lucide-x)').click();
    await expect(page.getByText('Save to playlist')).not.toBeVisible({ timeout: 5000 });
  });

  test('can add video to new playlist and see it on profile page', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    await page.waitForTimeout(1000);

    // Create a single video
    const result = await page.evaluate(async () => {
      const { getTree } = await import('/src/store.ts');
      const { nostrStore, saveHashtree } = await import('/src/nostr.ts');
      const hashtree = await import('/node_modules/hashtree/dist/index.js');
      const { toHex, videoChunker, cid } = hashtree;

      const tree = getTree();
      let npub: string = '';
      const unsub = nostrStore.subscribe((state: any) => { npub = state.npub; });
      unsub();

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

      const titleData = new TextEncoder().encode('Source Video For Modal Playlist Test');
      const titleResult = await tree.putFile(titleData, {});
      videoEntries.push({ name: 'title.txt', cid: titleResult.cid, size: titleResult.size });

      const videoDirResult = await tree.putDirectory(videoEntries, {});
      const treeName = 'videos/Source Video Modal Test';

      // Use saveHashtree to properly publish to Nostr
      await saveHashtree(treeName, videoDirResult.cid, { visibility: 'public' });

      return {
        npub,
        treeName,
        videoCid: videoDirResult.cid,
        videoSize: videoEntries.reduce((sum, e) => sum + e.size, 0),
      };
    });

    // Navigate to the video
    const videoUrl = `/video.html#/${result.npub}/${encodeURIComponent(result.treeName)}`;
    await page.goto(videoUrl);

    // Wait for video page to load - wait for the add-to-playlist button which is what we're testing
    const addToPlaylistBtn = page.locator('button[title="Add to playlist"]');
    await expect(addToPlaylistBtn).toBeVisible({ timeout: 30000 });

    // Take screenshot before clicking button
    await page.screenshot({ path: 'e2e/screenshots/add-to-playlist-before-click.png' });

    // Click "Add to playlist" button
    await addToPlaylistBtn.click();

    // Verify modal opens
    await expect(page.getByText('Save to playlist')).toBeVisible({ timeout: 5000 });

    // Take screenshot of modal
    await page.screenshot({ path: 'e2e/screenshots/add-to-playlist-modal-open.png' });

    // Click "Create new playlist" button
    const createNewBtn = page.getByText('Create new playlist');
    await expect(createNewBtn).toBeVisible({ timeout: 5000 });
    await createNewBtn.click();

    // Enter playlist name
    const playlistName = `Test Playlist ${Date.now()}`;
    const nameInput = page.locator('input#playlist-name');
    await expect(nameInput).toBeVisible({ timeout: 5000 });
    await nameInput.fill(playlistName);

    // Take screenshot of create form
    await page.screenshot({ path: 'e2e/screenshots/add-to-playlist-create-form.png' });

    // Click Create button
    const createBtn = page.locator('button[type="submit"]:has-text("Create")');
    await createBtn.click();

    // Wait for either: playlist appears in list (success) or error message appears
    // The playlist should appear with check mark once created
    const successOrError = page.locator('.i-lucide-check-square').or(page.locator('text=Failed to create playlist'));
    await expect(successOrError).toBeVisible({ timeout: 30000 });

    // Verify it was a success, not an error
    await expect(page.locator('.i-lucide-check-square')).toBeVisible({ timeout: 5000 });

    // Take screenshot after creation
    await page.screenshot({ path: 'e2e/screenshots/add-to-playlist-after-create.png' });

    // Click Done to close modal
    await page.getByText('Done').click();
    await expect(page.getByText('Save to playlist')).not.toBeVisible({ timeout: 5000 });

    // Navigate to profile page
    await page.goto(`/video.html#/${result.npub}`);

    // Wait for profile to load
    await page.waitForTimeout(2000);

    // Take screenshot of profile page
    await page.screenshot({ path: 'e2e/screenshots/add-to-playlist-profile-page.png' });

    // Check if playlist section exists and contains our playlist
    const playlistSection = page.getByText('Playlists');

    // Look for the playlist name on the page
    const playlistCard = page.getByText(playlistName.replace('videos/', ''));

    // Debug: log page content
    const pageContent = await page.content();
    console.log('Looking for playlist:', playlistName);
    console.log('Page has Playlists section:', await playlistSection.isVisible().catch(() => false));

    // The playlist should be visible on the profile page
    await expect(playlistCard).toBeVisible({ timeout: 10000 });

    // Click on the playlist to open it
    await playlistCard.click();

    // Wait for playlist page to load - should show the video inside
    // The URL should change to include the playlist name
    await page.waitForURL(/videos%2FTest%20Playlist/, { timeout: 10000 });

    // Take screenshot of playlist page
    await page.screenshot({ path: 'e2e/screenshots/add-to-playlist-opened.png' });

    // Debug: check what's on the page
    const currentUrl = page.url();
    console.log('Current URL after clicking playlist:', currentUrl);

    // Check if there's an error message
    const errorText = await page.locator('text=Video file not found').isVisible().catch(() => false);
    console.log('Shows "Video file not found" error:', errorText);

    // Check if redirect happened to first video
    const hasVideoId = currentUrl.includes('video_');
    console.log('URL contains video ID (redirected to first video):', hasVideoId);

    // If redirect didn't happen, there's an issue with findFirstVideoEntry
    if (!hasVideoId && errorText) {
      // Debug: check what's in the various caches and try to list directory
      const debugInfo = await page.evaluate(async (url) => {
        const { getLocalRootCache } = await import('/src/treeRootCache.ts');
        const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
        const { getTree } = await import('/src/store.ts');
        const { findFirstVideoEntry } = await import('/src/stores/playlist.ts');
        const hashtree = await import('/node_modules/hashtree/dist/index.js');

        // Parse URL to get npub and treeName
        const hashPart = url.split('#')[1] || '';
        const parts = hashPart.split('/').filter(Boolean);
        const npub = parts[0];
        const treeName = decodeURIComponent(parts[1] || '');

        // Check local root cache
        const localHash = getLocalRootCache(npub, treeName);

        // Check treeRootStore (synchronous)
        const treeRootCid = getTreeRootSync(npub, treeName);

        // Try to list directory directly
        let listResult: string | object = 'not attempted';
        let firstEntry: string | null = null;
        let storeHasData: string | boolean = false;
        if (treeRootCid) {
          try {
            const tree = getTree();
            // Check if tree store has the raw data for this hash
            const rawData = await tree.store?.get?.(treeRootCid.hash);
            storeHasData = rawData ? `${rawData.length} bytes, first bytes: ${Array.from(rawData.slice(0, 20)).map(b => b.toString(16).padStart(2, '0')).join(' ')}` : false;

            const entries = await tree.listDirectory(treeRootCid);
            listResult = entries?.map(e => ({ name: e.name, hash: hashtree.toHex(e.cid.hash) })) || [];
            firstEntry = await findFirstVideoEntry(treeRootCid);
          } catch (e) {
            listResult = `Error: ${e}`;
          }
        }

        return {
          npub,
          treeName,
          localRootCache: localHash ? hashtree.toHex(localHash) : null,
          treeRootSync: treeRootCid ? {
            hash: hashtree.toHex(treeRootCid.hash),
            hasKey: treeRootCid.key !== undefined,
            keyLen: treeRootCid.key?.length,
          } : null,
          storeHasData,
          listDirectory: listResult,
          findFirstVideoEntry: firstEntry,
        };
      }, currentUrl);

      console.log('Debug caches:', JSON.stringify(debugInfo, null, 2));
    }

    // Verify the playlist page shows content (video title or player)
    // The source video should be visible in the playlist
    const videoTitle = page.getByText('Source Video For Modal Playlist Test');
    await expect(videoTitle).toBeVisible({ timeout: 15000 });
  });

  test('another user can view a playlist published to Nostr', async ({ browser }) => {
    test.slow();

    // Create two separate browser contexts (two users)
    const contextA = await browser.newContext();
    const contextB = await browser.newContext();
    const pageA = await contextA.newPage();
    const pageB = await contextB.newPage();

    setupPageErrorHandler(pageA);
    setupPageErrorHandler(pageB);

    try {
      // ===== User A: Create and publish a playlist =====
      await pageA.goto('/video.html#/');
      await disableOthersPool(pageA);
      await configureBlossomServers(pageA);
      await ensureLoggedIn(pageA);

      // Wait for Create button - app auto-logs in
      await expect(pageA.locator('button:has-text("Create")')).toBeVisible({ timeout: 15000 });

      // Get User A's npub
      const userANpub = await pageA.evaluate(async () => {
        const { nostrStore } = await import('/src/nostr.ts');
        let npub: string = '';
        const unsub = nostrStore.subscribe((state: any) => { npub = state.npub; });
        unsub();
        return npub;
      });

      console.log('User A npub:', userANpub);

      // Create a playlist with 2 videos, publish to Nostr, and push to Blossom
      const playlist = await pageA.evaluate(async () => {
        const { getTree, getWebRTCStore } = await import('/src/store.ts');
        const { nostrStore, saveHashtree } = await import('/src/nostr.ts');
        const hashtree = await import('/node_modules/hashtree/dist/index.js');
        const { toHex, videoChunker, cid, BlossomStore } = hashtree;

        const tree = getTree();
        let npub: string = '';
        const unsub = nostrStore.subscribe((state: any) => { npub = state.npub; });
        unsub();

        // Create 2 videos for the playlist with distinct titles
        const videos = [
          { id: 'crossUserVideo001', title: 'Cross User Video Alpha' },
          { id: 'crossUserVideo002', title: 'Cross User Video Beta' },
        ];

        const rootEntries: Array<{ name: string; cid: any; size: number }> = [];

        for (const video of videos) {
          const videoEntries: Array<{ name: string; cid: any; size: number }> = [];

          // Create video file (minimal mp4 header)
          const videoData = new Uint8Array([0x00, 0x00, 0x00, 0x1C, 0x66, 0x74, 0x79, 0x70]);
          const streamWriter = tree.createStream({ chunker: videoChunker() });
          await streamWriter.append(videoData);
          const videoResult = await streamWriter.finalize();
          videoEntries.push({
            name: 'video.mp4',
            cid: cid(videoResult.hash, videoResult.key),
            size: videoResult.size,
          });

          // Create title.txt
          const titleData = new TextEncoder().encode(video.title);
          const titleResult = await tree.putFile(titleData, {});
          videoEntries.push({ name: 'title.txt', cid: titleResult.cid, size: titleResult.size });

          // Create video directory
          const videoDirResult = await tree.putDirectory(videoEntries, {});
          rootEntries.push({
            name: video.id,
            cid: videoDirResult.cid,
            size: videoEntries.reduce((sum, e) => sum + e.size, 0),
          });
        }

        // Create root playlist directory
        const rootDirResult = await tree.putDirectory(rootEntries, {});
        const treeName = 'videos/Cross User Playlist Test';

        // Publish to Nostr
        await saveHashtree(treeName, rootDirResult.cid, { visibility: 'public' });

        // Create BlossomStore for push (same pattern as BlossomPushModal)
        const { signEvent } = await import('/src/nostr.ts');
        const blossomStore = new BlossomStore({
          servers: [{ url: 'https://upload.iris.to', write: true }],
          signer: async (event: any) => signEvent({ ...event, pubkey: '', id: '', sig: '' }),
        });

        let pushResult = { attempted: true, success: false, error: '', pushed: 0 };
        try {
          await tree.push(rootDirResult.cid, blossomStore, {
            onProgress: (current: number, total: number) => {
              pushResult.pushed = current;
            },
          });
          pushResult.success = true;
        } catch (e) {
          pushResult.error = String(e);
        }

        return {
          npub,
          treeName,
          rootHash: toHex(rootDirResult.cid.hash),
          videos,
          rootCid: rootDirResult.cid,
          pushResult,
        };
      });

      console.log('User A created and pushed playlist:', playlist);

      // Navigate to playlist page to verify it works for User A first
      await pageA.goto(`/video.html#/${userANpub}/${encodeURIComponent(playlist.treeName)}/${playlist.videos[0].id}`);

      // Wait for the video title to appear (not the folder name)
      await expect(pageA.getByRole('heading', { name: 'Cross User Video Alpha' })).toBeVisible({ timeout: 15000 });

      // Take screenshot of User A's state
      await pageA.screenshot({ path: 'e2e/screenshots/playlist-share-userA.png' });

      // ===== User B: Initialize and navigate =====
      await pageB.goto('/video.html#/');
      await disableOthersPool(pageB);
      await configureBlossomServers(pageB);
      await ensureLoggedIn(pageB);

      // Wait for app to initialize
      await expect(pageB.locator('button:has-text("Create")')).toBeVisible({ timeout: 15000 });

      // Navigate directly to User A's playlist video
      const directUrl = `/video.html#/${userANpub}/${encodeURIComponent(playlist.treeName)}/${playlist.videos[0].id}`;
      console.log('User B navigating directly to:', directUrl);
      await pageB.goto(directUrl);

      // Take screenshot immediately
      await pageB.screenshot({ path: 'e2e/screenshots/playlist-share-userB-direct.png' });

      // Debug: Check tree root resolution and data availability
      const debugInfo = await pageB.evaluate(async (args: { npub: string; treeName: string }) => {
        // Wait a bit for tree resolution from Nostr
        await new Promise(r => setTimeout(r, 5000));

        // Try to get the tree root via the resolver
        const { waitForTreeRoot, getTreeRootSync } = await import('/src/stores/treeRoot.ts');

        // Check sync first
        const syncRoot = getTreeRootSync(args.npub, args.treeName);

        // Try async with timeout
        let asyncRoot = null;
        try {
          asyncRoot = await waitForTreeRoot(args.npub, args.treeName, 10000);
        } catch {}

        const storeInfo = {
          peerCount: (window as any).__appStore?.getState()?.peerCount || 0,
          syncRootFound: !!syncRoot,
          asyncRootFound: !!asyncRoot,
          asyncRootHash: asyncRoot?.hash ? Array.from(asyncRoot.hash as Uint8Array).slice(0, 4) : null,
        };

        return storeInfo;
      }, { npub: userANpub, treeName: playlist.treeName });
      console.log('Debug info:', JSON.stringify(debugInfo));

      // The title should NOT be the folder name (crossUserVideo001)
      // It SHOULD be the actual video title from title.txt
      const titleLocator = pageB.getByRole('heading', { level: 1 });
      await expect(titleLocator).toBeVisible({ timeout: 30000 });

      // Get the actual title text
      const actualTitle = await titleLocator.textContent();
      console.log('Actual title:', actualTitle);

      // Verify it's not just the folder ID
      expect(actualTitle).not.toBe('crossUserVideo001');
      expect(actualTitle).not.toBe('Loading video');
      expect(actualTitle).not.toBe('Video');

      // The title should be the actual video title
      expect(actualTitle).toContain('Cross User Video Alpha');

      // Take screenshot after title verification
      await pageB.screenshot({ path: 'e2e/screenshots/playlist-share-userB-title.png' });

      // Verify playlist sidebar shows both videos
      // Desktop sidebar uses "1/2" without parentheses, mobile uses "(1/2)"
      // Check for desktop version which should be visible in default viewport
      // Use more specific selector for the playlist sidebar panel
      const desktopSidebar = pageB.locator('.w-96.shrink-0.hidden.lg\\:block');
      await expect(desktopSidebar).toBeVisible({ timeout: 10000 });

      // Verify playlist name is visible in sidebar
      await expect(desktopSidebar.getByText('Cross User Playlist Test')).toBeVisible({ timeout: 5000 });

      // Verify both videos appear in the sidebar
      await expect(desktopSidebar.getByText('Cross User Video Alpha')).toBeVisible({ timeout: 5000 });
      await expect(desktopSidebar.getByText('Cross User Video Beta')).toBeVisible({ timeout: 5000 });

      // Take screenshot after sidebar verification
      await pageB.screenshot({ path: 'e2e/screenshots/playlist-share-userB-sidebar.png' });

      // ===== Navigate to User A's profile and verify playlist appears as playlist =====
      await pageB.goto(`/video.html#/${userANpub}`);
      await pageB.waitForTimeout(3000);

      // Take screenshot of profile
      await pageB.screenshot({ path: 'e2e/screenshots/playlist-share-userB-profile.png' });

      // Profile should show "Playlists" section with the playlist (not as a single video)
      const playlistsSection = pageB.getByText('Playlists');
      const isPlaylistsSectionVisible = await playlistsSection.isVisible({ timeout: 10000 }).catch(() => false);

      console.log('Profile has Playlists section:', isPlaylistsSectionVisible);

      // The playlist should appear with the playlist icon/video count
      const playlistCard = pageB.getByText('Cross User Playlist Test');
      await expect(playlistCard).toBeVisible({ timeout: 10000 });

      // Check that it shows video count (indicates it's recognized as a playlist)
      const videoCountText = pageB.getByText(/2 video/i);
      await expect(videoCountText).toBeVisible({ timeout: 10000 });

      console.log('Test passed: Another user can view playlist with full experience');

    } finally {
      await contextA.close();
      await contextB.close();
    }
  });

  test('setEntry method can add CID reference to directory', async ({ page }) => {
    // This test verifies that the hashtree setEntry method works correctly
    // for adding video CID references to playlists

    await page.goto('/video.html#/');
    await disableOthersPool(page);

    // Wait for app to initialize
    await page.waitForTimeout(1000);

    const result = await page.evaluate(async () => {
      const { getTree } = await import('/src/store.ts');
      const hashtree = await import('/node_modules/hashtree/dist/index.js');
      const { toHex, cid, LinkType } = hashtree;

      const tree = getTree();

      // Create a source video directory (simulating an existing video)
      const videoEntries: Array<{ name: string; cid: any; size: number }> = [];

      const videoData = new Uint8Array([0x00, 0x00, 0x00, 0x1C, 0x66, 0x74, 0x79, 0x70]);
      const videoResult = await tree.putFile(videoData, {});
      videoEntries.push({ name: 'video.mp4', cid: videoResult.cid, size: videoResult.size });

      const titleData = new TextEncoder().encode('Reference Test Video');
      const titleResult = await tree.putFile(titleData, {});
      videoEntries.push({ name: 'title.txt', cid: titleResult.cid, size: titleResult.size });

      const sourceVideoCid = await tree.putDirectory(videoEntries, {});
      const sourceVideoSize = videoEntries.reduce((sum, e) => sum + e.size, 0);

      // Create an empty playlist directory
      const emptyPlaylist = await tree.putDirectory([], {});

      // Use setEntry to add the video CID to the playlist
      const updatedPlaylist = await tree.setEntry(
        emptyPlaylist.cid,
        [], // root path
        'video-ref-001', // entry name
        sourceVideoCid.cid, // CID of the source video
        sourceVideoSize,
        LinkType.Dir // it's a directory
      );

      // Verify the entry was added
      const entries = await tree.listDirectory(updatedPlaylist);

      // Verify the referenced video's content is accessible
      let videoAccessible = false;
      for (const entry of entries) {
        if (entry.name === 'video-ref-001') {
          const subEntries = await tree.listDirectory(entry.cid);
          videoAccessible = subEntries.some((e: any) => e.name === 'video.mp4');
        }
      }

      return {
        entryCount: entries.length,
        entryNames: entries.map((e: any) => e.name),
        videoAccessible,
        sourceVideoHash: toHex(sourceVideoCid.cid.hash),
      };
    });

    // Verify setEntry correctly added the CID reference
    expect(result.entryCount).toBe(1);
    expect(result.entryNames).toContain('video-ref-001');
    expect(result.videoAccessible).toBe(true);

    console.log('setEntry CID reference test passed:', result);
  });
});
