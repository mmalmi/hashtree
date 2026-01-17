import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady } from './test-utils';
import path from 'path';
import { fileURLToPath } from 'url';
// Run tests in this file serially - they share search index state
test.describe.configure({ mode: 'serial' });

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/**
 * Helper to ensure user is logged in
 */
async function ensureLoggedIn(page: any) {
  const createBtn = page.locator('button:has-text("Create")');
  const isVisible = await createBtn.isVisible().catch(() => false);

  if (!isVisible) {
    const newBtn = page.getByRole('button', { name: /New/i });
    if (await newBtn.isVisible().catch(() => false)) {
      await newBtn.click();
      await expect(createBtn).toBeVisible({ timeout: 15000 });
    }
  }
}

/**
 * Helper to open the video upload modal
 */
async function openUploadModal(page: any) {
  const createBtn = page.locator('button:has-text("Create")');
  await expect(createBtn).toBeVisible({ timeout: 15000 });
  await createBtn.click();

  const uploadOption = page.locator('button:has-text("Upload Video")').first();
  await expect(uploadOption).toBeVisible({ timeout: 5000 });
  await uploadOption.click();

  await expect(page.getByRole('heading', { name: 'Upload Video' })).toBeVisible({ timeout: 10000 });
}

/**
 * Tests for video search functionality in Iris Video
 */
test.describe('Video Search', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('can search for video by title keyword', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Generate a unique keyword for this test
    const uniqueKeyword = `SearchTest${Date.now()}`;
    const videoTitle = `Amazing ${uniqueKeyword} Tutorial`;

    // Upload a video with a unique title
    await page.keyboard.press('Escape');
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    // Wait for upload to complete
    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Navigate back to home
    await page.locator('a[href="#/"]').first().click();
    await page.waitForURL(/\/video\.html#\/$/, { timeout: 10000 });

    // Wait for video to appear in feed (no "Feed" heading in current UI)
    await expect(page.locator('a[href*="videos"]').first()).toBeVisible({ timeout: 30000 });
    await expect(page.locator(`text=${videoTitle}`).first()).toBeVisible({ timeout: 30000 });

    // Type in the search input - use the unique keyword (lowercased since parsing lowercases)
    const searchInput = page.locator('header input[placeholder*="Search"]');
    await expect(searchInput).toBeVisible({ timeout: 5000 });
    await searchInput.click();

    // Search using the keyword - need to wait for index to be populated
    const searchKeyword = uniqueKeyword.toLowerCase();

    // Poll until search returns results (index is async with 500ms debounce)
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

    // Type in search (focus triggers showDropdown, input triggers search)
    await searchInput.click();
    await searchInput.fill(searchKeyword);

    // Look for result button in the search dropdown area
    const dropdown = page.locator('div.absolute.top-full');
    await expect(dropdown).toBeVisible({ timeout: 30000 });
    const dropdownResult = dropdown.locator('button').filter({ hasText: uniqueKeyword }).first();
    await expect(dropdownResult).toBeVisible({ timeout: 30000 });

    // Click on the result
    await dropdownResult.click();

    // Should navigate to the video page (via npub/videos path or nhash)
    await page.waitForURL(/\/video\.html#\/(npub1.*\/videos|nhash1)/, { timeout: 10000 });
    // Verify video page loaded by checking for video title or player
    await expect(page.getByRole('heading', { level: 1 }).filter({ hasText: 'Amazing' })).toBeVisible({ timeout: 15000 });
  });

  test('search shows no results for non-matching query', async ({ page }) => {
    await page.goto('/video.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    // Search for a random string that won't match anything
    const searchInput = page.locator('header input[placeholder*="Search"]');
    await expect(searchInput).toBeVisible({ timeout: 5000 });
    await searchInput.fill('xyznonexistentvideo12345');

    await page.waitForFunction(
      async (kw) => {
        // @ts-expect-error accessing module
        const searchModule = await import('/src/lib/search/index.ts');
        const results = await searchModule.search(kw, { limit: 5, sources: ['video'] });
        return results.length === 0;
      },
      'xyznonexistentvideo12345',
      { timeout: 10000 }
    );

    // Dropdown should not show video results (might show nothing or only user results)
    const videoResult = page.locator('button:has(.i-lucide-video)');
    await expect(videoResult).not.toBeVisible({ timeout: 3000 });
  });

  test('search filters stop words and short keywords', async ({ page }) => {
    test.slow();

    await page.goto('/video.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await ensureLoggedIn(page);

    // Upload video with title containing stop words
    const uniqueKeyword = `FilterTest${Date.now()}`;
    const videoTitle = `The ${uniqueKeyword} is a great video`;

    await page.keyboard.press('Escape');
    await openUploadModal(page);

    const testVideoPath = path.join(__dirname, 'fixtures', 'Big_Buck_Bunny_360_10s_1MB.mp4');
    await page.locator('input[type="file"][accept="video/*"]').setInputFiles(testVideoPath);
    await page.locator('input[placeholder="Video title"]').fill(videoTitle);
    await page.locator('.fixed button:has-text("Upload")').click();

    await page.waitForURL(/\/video\.html#\/npub.*\/videos%2F/, { timeout: 60000 });
    await expect(page.getByRole('heading', { name: 'Upload Video' })).not.toBeVisible({ timeout: 10000 });

    // Navigate back to home
    await page.locator('a[href="#/"]').first().click();
    await page.waitForURL(/\/video\.html#\/$/, { timeout: 10000 });
    await expect(page.locator('a[href*="videos"]').first()).toBeVisible({ timeout: 30000 });

    // Search for "the" - should not find results (stop word)
    const searchInput = page.locator('header input[placeholder*="Search"]');
    await searchInput.fill('the');
    await page.waitForFunction(
      async (kw) => {
        // @ts-expect-error accessing module
        const searchIndex = await import('/src/stores/searchIndex.ts');
        const results = await searchIndex.searchVideos(kw, 5);
        return results.length === 0;
      },
      'the',
      { timeout: 10000 }
    );

    // Should not find the video with just "the"
    const videoResultThe = page.locator('button', { hasText: new RegExp(uniqueKeyword) }).first();
    await expect(videoResultThe).not.toBeVisible({ timeout: 3000 });

    // Clear and search for the unique keyword - should find it
    await searchInput.fill(uniqueKeyword);
    const videoResultUnique = page.locator('button', { hasText: new RegExp(uniqueKeyword) }).first();
    await expect(videoResultUnique).toBeVisible({ timeout: 10000 });
  });

  test('search placeholder text includes videos', async ({ page }) => {
    await page.goto('/video.html#/');

    // Verify search placeholder mentions videos
    const searchInput = page.locator('header input[placeholder*="videos"]');
    await expect(searchInput).toBeVisible({ timeout: 10000 });
  });
});
