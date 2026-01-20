import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, waitForAppReady } from './test-utils';

/**
 * Tests for document thumbnail generation and display
 */
test.describe('Document Thumbnails', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('generates and displays thumbnail for document', async ({ page }) => {
    // Use slow mode since thumbnail capture is throttled (30s) but we'll override
    test.slow();

    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    // Wait for auto-login to complete and New Document card to appear
    const newDocCard = page.getByRole('button', { name: 'New Document' });
    await expect(newDocCard).toBeVisible({ timeout: 30000 });

    // Create a document
    await newDocCard.click();

    const docName = `Thumbnail Test ${Date.now()}`;
    await page.locator('input[placeholder="Document name..."]').fill(docName);
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for editor to load
    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });

    // Type some visible content that will appear in the thumbnail
    const editor = page.locator('.ProseMirror');
    await editor.click();
    await editor.type('This is a test document with some content for the thumbnail preview.');

    // Wait for initial autosave to complete
    await expect(page.getByText(/Saved\s/)).toBeVisible({ timeout: 30000 });

    // Reset the throttle timer to allow immediate thumbnail capture
    await page.evaluate(() => {
      const reset = (window as any).__thumbnailCaptureReset;
      if (reset) reset();
    });

    // Trigger another save by adding more content
    await editor.type(' More text.');

    // Verify thumbnail was saved by checking the tree
    await expect.poll(async () => {
      return page.evaluate(async () => {
        const { getTree } = await import('/src/store');
        const { getTreeRootSync } = await import('/src/stores');
        const nostrStore = (window as any).__nostrStore;

        const npub = nostrStore?.getState()?.npub;
        // Tree name is URL-encoded and includes docs/ prefix (e.g., docs%2FThumbnail%20Test)
        const treeNameMatch = window.location.hash.match(/\/npub[^/]+\/([^/?]+)/)?.[1];
        const treeName = treeNameMatch ? decodeURIComponent(treeNameMatch) : null;

        if (!npub || !treeName) return false;

        const rootCid = getTreeRootSync(npub, treeName);
        if (!rootCid) return false;

        const tree = getTree();
        const result = await tree.resolvePath(rootCid, '.thumbnail.jpg');
        return !!result;
      });
    }, { timeout: 60000 }).toBe(true);

    // Navigate to home page
    await page.evaluate(() => window.location.hash = '#/');

    // Wait for doc cards to load
    await expect(page.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

    // Find the doc card and verify it has a thumbnail image
    const docCard = page.locator(`a:has-text("${docName}")`);
    await expect(docCard).toBeVisible({ timeout: 30000 });

    // The thumbnail image should be visible inside the card (top preview area)
    const thumbnailImg = docCard.locator('div.flex-1 img');
    await expect(thumbnailImg).toBeVisible({ timeout: 20000 });

    // Verify the image actually loads
    await expect.poll(async () => {
      return page.evaluate((name) => {
        const cards = Array.from(document.querySelectorAll('a'));
        const card = cards.find((el) => el.textContent?.includes(name));
        const img = card?.querySelector('div.flex-1 img') as HTMLImageElement | null;
        if (!img) return false;
        return img.complete && img.naturalWidth > 0;
      }, docName);
    }, { timeout: 60000 }).toBe(true);
  });

  test('shows file icon when no thumbnail exists', async ({ page }) => {
    test.slow();
    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    // Wait for auto-login to complete and New Document card to appear
    const newDocCard = page.getByRole('button', { name: 'New Document' });
    await expect(newDocCard).toBeVisible({ timeout: 30000 });

    // Create a document
    await newDocCard.click();

    const docName = `No Thumb Test ${Date.now()}`;
    await page.locator('input[placeholder="Document name..."]').fill(docName);
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for editor
    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });

    // Don't type anything - just navigate away before any thumbnail capture
    // Navigate to home before thumbnail has a chance to be captured
    await page.evaluate(() => window.location.hash = '#/');

    // Wait for doc cards to load
    await expect(page.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

    // Find the doc card
    const docCard = page.locator(`a:has-text("${docName}")`);
    await expect(docCard).toBeVisible({ timeout: 30000 });

    // DocCard tries to load thumbnail URL first, shows icon on error.
    // Simulate an image load failure so we can verify the fallback icon.
    const thumbnailImg = docCard.locator('div.flex-1 img');
    await page.evaluate((name) => {
      const cards = Array.from(document.querySelectorAll('a'));
      const card = cards.find((el) => el.textContent?.includes(name));
      const img = card?.querySelector('div.flex-1 img') as HTMLImageElement | null;
      if (img) {
        img.dispatchEvent(new Event('error'));
      }
    }, docName);

    const fileIcon = docCard.locator('div.flex-1 .i-lucide-file-text');
    await expect(fileIcon).toHaveCount(1, { timeout: 15000 });
    await expect(thumbnailImg).toHaveCount(0, { timeout: 15000 });
  });

  test('thumbnail updates when document content changes', async ({ page }) => {
    test.slow();

    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    // Wait for auto-login to complete and New Document card to appear
    await expect(page.getByRole('button', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

    // Create a document
    await page.getByRole('button', { name: 'New Document' }).click();

    const docName = `Update Thumb ${Date.now()}`;
    await page.locator('input[placeholder="Document name..."]').fill(docName);
    await page.getByRole('button', { name: 'Create' }).click();

    // Wait for editor
    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });

    // Type initial content
    const editor = page.locator('.ProseMirror');
    await editor.click();
    await editor.type('Initial content for first thumbnail.');

    // Wait for save and reset throttle to allow thumbnail capture
    await expect(page.getByText(/Saved\s/)).toBeVisible({ timeout: 30000 });
    await page.evaluate(() => {
      const reset = (window as any).__thumbnailCaptureReset;
      if (reset) reset();
    });

    // Add a bit more to trigger save with thumbnail
    await editor.type('.');

    // Get the first thumbnail hash
    const getThumbnailHash = async () => {
      return page.evaluate(async () => {
        const { getTree } = await import('/src/store');
        const { getTreeRootSync } = await import('/src/stores');
        const nostrStore = (window as any).__nostrStore;

        const npub = nostrStore?.getState()?.npub;
        // Tree name is URL-encoded and includes docs/ prefix
        const treeNameMatch = window.location.hash.match(/\/npub[^/]+\/([^/?]+)/)?.[1];
        const treeName = treeNameMatch ? decodeURIComponent(treeNameMatch) : null;
        if (!npub || !treeName) return null;

        const rootCid = getTreeRootSync(npub, treeName);
        if (!rootCid) return null;

        const tree = getTree();
        const result = await tree.resolvePath(rootCid, '.thumbnail.jpg');
        if (!result) return null;

        // Return hash as hex string
        return Array.from(result.cid.hash).map(b => b.toString(16).padStart(2, '0')).join('');
      });
    };

    await expect.poll(getThumbnailHash, { timeout: 30000 }).not.toBeNull();
    const firstThumbHash = await getThumbnailHash();

    // Reset throttle and add more content
    await page.evaluate(() => {
      const reset = (window as any).__thumbnailCaptureReset;
      if (reset) reset();
    });

    await editor.click();
    await editor.press('End');
    await editor.type(' More content added for second thumbnail.');

    await expect.poll(async () => {
      const hash = await getThumbnailHash();
      return hash && hash !== firstThumbHash ? hash : null;
    }, { timeout: 30000 }).not.toBeNull();

    const secondThumbHash = await getThumbnailHash();

    // Both should exist
    expect(firstThumbHash).toBeTruthy();
    expect(secondThumbHash).toBeTruthy();

    // They should be different (content changed)
    expect(secondThumbHash).not.toBe(firstThumbHash);
  });
});
