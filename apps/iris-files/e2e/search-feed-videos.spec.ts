import { test, expect } from './fixtures';
import { setupPageErrorHandler, waitForAppReady, disableOthersPool, ensureLoggedIn, useLocalRelay, getTestRelayUrl } from './test-utils';
import path from 'path';
import { fileURLToPath } from 'url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/**
 * Production test: Verify videos in the feed can be found via search and load properly.
 * Uses bootstrap index from sirius.
 *
 * Run with: pnpm exec playwright test --config=playwright.production.config.ts e2e/search-feed-videos.spec.ts
 */
test.describe('Search Feed Videos', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('feed videos appear in search and load correctly', async ({ page }) => {
    test.slow(); // This test needs more time

    await page.goto('/video.html#/');
    await waitForAppReady(page);
    const testRelayUrl = getTestRelayUrl();
    const isTestMode = await page.evaluate((relayUrl) => {
      const store = (window as any).__settingsStore;
      if (!store?.subscribe) return false;
      let settings: any = null;
      store.subscribe((value: any) => { settings = value; })();
      return settings?.network?.relays?.includes(relayUrl);
    }, testRelayUrl);

    if (isTestMode) {
      await disableOthersPool(page);
      await ensureLoggedIn(page);
      await useLocalRelay(page);

      const uniqueKeyword = `FeedSearch${Date.now()}`;
      const videoTitle = `Local ${uniqueKeyword} Demo`;

      await page.keyboard.press('Escape');
      const createBtn = page.locator('button:has-text("Create")');
      await expect(createBtn).toBeVisible({ timeout: 15000 });
      await createBtn.click();

      const uploadOption = page.locator('button:has-text("Upload Video")').first();
      await expect(uploadOption).toBeVisible({ timeout: 5000 });
      await uploadOption.click();

      await expect(page.getByRole('heading', { name: 'Upload Video' })).toBeVisible({ timeout: 10000 });
      const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
      await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);
      await page.locator('input[placeholder="Video title"]').fill(videoTitle);
      await page.locator('.fixed button:has-text("Upload")').click();

      await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });
      await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

      await page.locator('a[href="#/"]').first().click();
      await page.waitForURL(/\/video\.html#\/$/, { timeout: 10000 });

      await expect(page.locator(`text=${videoTitle}`).first()).toBeVisible({ timeout: 30000 });

      const searchInput = page.locator('header input[placeholder*="Search"]');
      await expect(searchInput).toBeVisible({ timeout: 5000 });

      const searchKeyword = uniqueKeyword.toLowerCase();
      await page.waitForFunction(
        async (kw) => {
          // @ts-expect-error accessing module
          const searchIndex = await import('/src/stores/searchIndex.ts');
          const results = await searchIndex.searchVideos(kw, 5);
          return results.length > 0;
        },
        searchKeyword,
        { timeout: 20000 }
      );

      await searchInput.fill(searchKeyword);
      const dropdown = page.locator('div.absolute.top-full');
      await expect(dropdown).toBeVisible({ timeout: 30000 });
      const dropdownResult = dropdown.locator('button').filter({ hasText: uniqueKeyword }).first();
      await expect(dropdownResult).toBeVisible({ timeout: 30000 });

      await dropdownResult.click();
      await page.waitForURL(/\/video\.html#\/(npub1.*\/videos|nhash1)/, { timeout: 10000 });
      await expect(page.getByRole('heading', { level: 1 })).toContainText(uniqueKeyword, { timeout: 15000 });
      return;
    }

    // Wait for app to load
    await expect(page.locator('text=Iris Video')).toBeVisible({ timeout: 30000 });

    // Wait for feed to have videos
    const hasVideos = await page.locator('a[href*="videos"]').first().isVisible({ timeout: 30000 }).catch(() => false);
    expect(hasVideos).toBe(true);

    // Wait for feed to populate with videos
    const videoCards = page.locator('a[href*="videos%2F"]');
    await expect(videoCards.first()).toBeVisible({ timeout: 30000 });

    // Wait for videos to be indexed as they load in feed
    console.log('Waiting for videos to be indexed...');
    const firstTitle = await videoCards.first().locator('h3, .text-text-1, .font-medium').first().textContent().catch(() => null);
    const firstSearchTerm = firstTitle
      ? firstTitle.split(/\s+/).find(w => w.length >= 3) || 'video'
      : 'video';
    await page.waitForFunction(
      async (term) => {
        // @ts-expect-error accessing module
        const { searchVideos } = await import('/src/stores/searchIndex.ts');
        const results = await searchVideos(term, 1);
        return results.length > 0;
      },
      firstSearchTerm.toLowerCase(),
      { timeout: 30000, polling: 500 }
    );

    // Check index state
    const indexState = await page.evaluate(async () => {
      // @ts-expect-error accessing module
      const { getIndexRoot, searchVideos } = await import('/src/stores/searchIndex.ts');
      const root = getIndexRoot();
      const testResults: Record<string, number> = {};
      for (const term of ['video', 'music', 'angel', 'sirius', 'mine']) {
        const r = await searchVideos(term, 5);
        testResults[term] = r.length;
      }
      return { hasRoot: root !== null, testResults };
    });
    console.log('Index state:', indexState);

    // Collect video info from feed that ARE in the search index
    const feedVideos: { title: string; href: string; searchable: boolean }[] = [];
    const cardCount = await videoCards.count();
    console.log(`Found ${cardCount} video cards in feed`);

    const maxVideosToTest = 5;
    let testedCount = 0;
    for (let i = 0; i < cardCount && testedCount < maxVideosToTest; i++) {
      const card = videoCards.nth(i);
      const href = await card.getAttribute('href');
      const titleEl = card.locator('h3, .text-text-1, .font-medium').first();
      const title = await titleEl.textContent().catch(() => null);

      if (title && href && title.trim().length > 2) {
        // Check if this video is searchable (has at least one result for first word)
        const words = title.trim().split(/\s+/).filter(w => w.length >= 3).slice(0, 2);
        if (words.length === 0) continue;

        const searchable = await page.evaluate(async (query) => {
          // @ts-expect-error accessing module
          const { searchVideos } = await import('/src/stores/searchIndex.ts');
          const results = await searchVideos(query, 5);
          return results.length > 0;
        }, words.join(' ').toLowerCase());

        if (searchable) {
          feedVideos.push({ title: title.trim(), href, searchable: true });
          console.log(`  [${feedVideos.length}] "${title.trim().slice(0, 50)}..." (indexed)`);
          testedCount++;
        } else {
          console.log(`  [skip] "${title.trim().slice(0, 50)}..." (not indexed)`);
        }
      }
    }

    expect(feedVideos.length).toBeGreaterThan(0);
    console.log(`\nTesting ${feedVideos.length} searchable videos\n`);

    // Take screenshot of feed
    await page.screenshot({ path: 'e2e/screenshots/feed-videos.png' });

    // Test search for each video
    const searchInput = page.locator('header input[placeholder*="Search"]');
    await expect(searchInput).toBeVisible({ timeout: 5000 });

    const failures: { title: string; reason: string }[] = [];

    for (let idx = 0; idx < feedVideos.length; idx++) {
      const { title } = feedVideos[idx];
      console.log(`\n--- [${idx + 1}/${feedVideos.length}] Searching for: "${title.slice(0, 40)}..." ---`);

      // Extract search keyword - use first 2 significant words
      const words = title
        .split(/\s+/)
        .filter(w => w.length >= 3 && !/^\d+$/.test(w))
        .slice(0, 2);

      if (words.length === 0) {
        console.log('  Skipping - no valid search words');
        continue;
      }

      const searchQuery = words.join(' ').toLowerCase();
      console.log(`  Query: "${searchQuery}"`);

      // Clear and search via UI
      await searchInput.click();
      await searchInput.fill('');
      await expect(searchInput).toHaveValue('');
      await searchInput.focus();
      await page.keyboard.type(searchQuery, { delay: 30 });

      // Wait for dropdown
      const dropdown = page.locator('div.absolute.top-full');
      const hasDropdown = await dropdown.isVisible({ timeout: 5000 }).catch(() => false);
      if (hasDropdown) {
        await page.waitForFunction(() => {
          const list = document.querySelector('div.absolute.top-full');
          return !!list && list.querySelectorAll('button').length > 0;
        }, null, { timeout: 5000 });
      }

      if (!hasDropdown) {
        console.log('  ✗ No search results dropdown');
        failures.push({ title, reason: 'not found in search' });
        continue;
      }

      // Find matching result
      const resultButtons = dropdown.locator('button');
      const resultCount = await resultButtons.count();
      console.log(`  Found ${resultCount} results`);

      let found = false;
      for (let i = 0; i < Math.min(resultCount, 5); i++) {
        const resultText = await resultButtons.nth(i).textContent();
        if (words.some(w => resultText?.toLowerCase().includes(w.toLowerCase()))) {
          found = true;
          console.log(`  ✓ Found matching result: "${resultText?.slice(0, 50)}..."`);

          // Click and verify navigation
          await resultButtons.nth(i).click();

          try {
            await page.waitForURL(/\/video\.html#\/npub/, { timeout: 10000 });
            const urlPath = page.url().split('#')[1]?.slice(0, 40);
            console.log(`  ✓ Navigated to: ${urlPath}...`);

            // Wait for video content to load - check for title text or video player
            // Use h1 selector since data-testid may not be picked up by dev server
            const videoTitleEl = page.locator('h1').filter({ hasText: words[0] });
            const videoPlayer = page.locator('video[src]');

            // Wait for either title or video player to appear
            const hasContent = await Promise.race([
              videoTitleEl.first().isVisible({ timeout: 15000 }).then(() => 'title'),
              videoPlayer.first().isVisible({ timeout: 15000 }).then(() => 'player'),
            ]).catch(() => null);

            // Take screenshot
            await page.screenshot({ path: `e2e/screenshots/video-${idx}.png` });

            if (!hasContent) {
              console.log('  ✗ Video content not visible - page failed to load');
              failures.push({ title, reason: 'content not loaded' });
            } else {
              // Check for video player with src
              const hasPlayer = await videoPlayer.first().isVisible().catch(() => false);
              if (hasPlayer) {
                const src = await videoPlayer.first().getAttribute('src');
                console.log(`  ✓ Video loaded: ${src?.slice(0, 50)}...`);
              } else {
                // Title visible but no player - might still be loading video
                const titleText = await page.locator('h1').first().textContent();
                console.log(`  ✓ Title visible: "${titleText?.slice(0, 50)}..."`);
              }
            }
          } catch (e) {
            console.log(`  ✗ Navigation failed: ${e}`);
            await page.screenshot({ path: `e2e/screenshots/error-${idx}.png` });
            failures.push({ title, reason: 'navigation failed' });
          }

          // Go back to home
          await page.locator('a[href="#/"]').first().click();
          await page.waitForURL(/\/video\.html#\/$/, { timeout: 10000 });
          break;
        }
      }

      if (!found) {
        console.log('  ✗ No matching result found in dropdown');
        failures.push({ title, reason: 'not found in dropdown' });
      }

      // Clear search for next iteration
      await searchInput.fill('');
      await page.waitForFunction(() => {
        const list = document.querySelector('div.absolute.top-full');
        return !list || list.querySelectorAll('button').length === 0;
      }, null, { timeout: 5000 }).catch(() => {});
    }

    // Summary
    console.log('\n=== RESULTS ===');
    console.log(`Passed: ${feedVideos.length - failures.length}/${feedVideos.length}`);

    if (failures.length > 0) {
      console.log('\nFailures:');
      for (const f of failures) {
        console.log(`  - "${f.title.slice(0, 40)}...": ${f.reason}`);
      }
    }

    // Take final screenshot
    await page.screenshot({ path: 'e2e/screenshots/search-feed-complete.png' });

    // Assert no failures
    expect(failures, `${failures.length} videos failed to load`).toHaveLength(0);
  });
});
