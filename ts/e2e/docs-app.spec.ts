import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool, configureBlossomServers, waitForWebRTCConnection, waitForAppReady, ensureLoggedIn } from './test-utils';

async function waitForTreeRootChange(page: any, previousRoot: string | null, timeoutMs: number = 30000) {
  await page.waitForFunction(
    () => typeof (window as any).__getTreeRoot === 'function',
    undefined,
    { timeout: 10000 }
  );
  await page.waitForFunction(
    (prev) => {
      const current = (window as any).__getTreeRoot?.();
      return !!current && current !== prev;
    },
    previousRoot,
    { timeout: timeoutMs }
  );
}

test.describe('Iris Docs App', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('shows Iris Docs header', async ({ page }) => {
    await page.goto('/docs.html#/');
    await expect(page.locator('text=Iris Docs')).toBeVisible({ timeout: 30000 });
  });

  test('shows New Document card after login', async ({ page }) => {
    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    await page.getByRole('button', { name: /New/i }).click();

    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 15000 });
  });

  test('can create new document', async ({ page }) => {
    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    await page.getByRole('button', { name: /New/i }).click();

    const newDocCard = page.locator('[role="button"]:has-text("New Document")');
    await expect(newDocCard).toBeVisible({ timeout: 15000 });

    await page.keyboard.press('Escape');
    await newDocCard.click();

    await expect(page.getByRole('heading', { name: 'New Document' })).toBeVisible({ timeout: 30000 });

    const docName = `Test Doc ${Date.now()}`;
    await page.locator('input[placeholder="Document name..."]').fill(docName);

    await expect(page.locator('button:has-text("public")')).toBeVisible({ timeout: 30000 });

    await page.getByRole('button', { name: 'Create' }).click();

    await page.waitForURL(/\/docs\.html#\/npub.*\/docs%2FTest%20Doc/, { timeout: 15000 });

    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });
    await expect(page.locator('.ProseMirror-container')).toBeVisible();
  });

  test('header has Iris Docs branding', async ({ page }) => {
    await page.goto('/docs.html#/');
    await expect(page.locator('text=Iris Docs')).toBeVisible({ timeout: 30000 });
  });

  test('document persists after refresh and shows on home', async ({ page }) => {
    test.slow();
    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    await page.getByRole('button', { name: /New/i }).click();

    const newDocCard = page.locator('[role="button"]:has-text("New Document")');
    await expect(newDocCard).toBeVisible({ timeout: 15000 });

    await page.keyboard.press('Escape');
    await newDocCard.click();

    const docName = `Persist Test ${Date.now()}`;
    await page.locator('input[placeholder="Document name..."]').fill(docName);
    await page.getByRole('button', { name: 'Create' }).click();

    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });

    const editor = page.locator('.ProseMirror');
    const rootBefore = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await editor.click();
    await editor.type('Hello persistence test!');
    await expect(editor).toContainText('Hello persistence test!', { timeout: 15000 });
    await waitForTreeRootChange(page, rootBefore, 60000);

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForAppReady(page, 60000);
    await page.waitForFunction(
      () => (window as any).__nostrStore?.getState?.().pubkey?.length === 64,
      undefined,
      { timeout: 60000 }
    );

    const editorAfterReload = page.locator('.ProseMirror');
    await expect(editorAfterReload).toBeVisible({ timeout: 60000 });
    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 60000 });
    await expect(editorAfterReload).toContainText('Hello persistence test!', { timeout: 60000 });

    await page.evaluate(() => window.location.hash = '#/');

    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 30000 });
    await expect(page.locator(`text=${docName}`)).toBeVisible({ timeout: 30000 });
  });

  test('can navigate from home to document and view content', async ({ page }) => {
    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    await page.getByRole('button', { name: /New/i }).click();

    const newDocCard = page.locator('[role="button"]:has-text("New Document")');
    await expect(newDocCard).toBeVisible({ timeout: 15000 });

    await page.keyboard.press('Escape');
    await newDocCard.click();

    const docName = `Navigate Test ${Date.now()}`;
    await page.locator('input[placeholder="Document name..."]').fill(docName);
    await page.getByRole('button', { name: 'Create' }).click();

    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });
    const editor = page.locator('.ProseMirror');
    const rootBefore = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await editor.click();
    await editor.type('Content for navigation test');
    await expect(editor).toContainText('Content for navigation test', { timeout: 15000 });
    await waitForTreeRootChange(page, rootBefore, 60000);

    await page.evaluate(() => window.location.hash = '#/');

    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 30000 });

    const docCard = page.locator(`text=${docName}`);
    await expect(docCard).toBeVisible({ timeout: 30000 });
    await docCard.click();

    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });
    await expect(page.locator('.ProseMirror')).toContainText('Content for navigation test', { timeout: 30000 });
  });

  test('edits to existing document persist after navigation and refresh', async ({ page }) => {
    test.setTimeout(90000); // Longer timeout for multiple reload operations

    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);
    await ensureLoggedIn(page, 30000);

    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 15000 });

    await page.keyboard.press('Escape');
    await page.locator('[role="button"]:has-text("New Document")').click();

    const docName = `Edit Persist Test ${Date.now()}`;
    const encodedDocPath = encodeURIComponent(`docs/${docName}`);
    await page.locator('input[placeholder="Document name..."]').fill(docName);
    await page.getByRole('button', { name: 'Create' }).click();

    await page.waitForURL(url => url.toString().includes(encodedDocPath), { timeout: 30000 });
    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });
    const editor = page.locator('.ProseMirror');
    const rootBeforeInitial = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await editor.click();
    await editor.type('Initial content.');
    await expect(editor).toContainText('Initial content.', { timeout: 15000 });
    await waitForTreeRootChange(page, rootBeforeInitial, 60000);

    await page.evaluate(() => window.location.hash = '#/');
    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 30000 });

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForAppReady(page);
    await waitForAppReady(page);
    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 30000 });

    const docCard = page.locator(`text=${docName}`);
    await expect(docCard).toBeVisible({ timeout: 30000 });
    await docCard.click();

    await page.waitForURL(url => url.toString().includes(encodedDocPath), { timeout: 30000 });
    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });
    await expect(page.locator('.ProseMirror')).toContainText('Initial content.', { timeout: 30000 });

    const editor2 = page.locator('.ProseMirror');
    const rootBeforeUpdate = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await editor2.click();
    await editor2.press('End');
    await editor2.type(' Added more content.');
    await expect(editor2).toContainText('Added more content.', { timeout: 15000 });
    await waitForTreeRootChange(page, rootBeforeUpdate, 60000);

    await page.reload();
    await waitForAppReady(page);

    await expect(page.locator('button[title="Bold (Ctrl+B)"]')).toBeVisible({ timeout: 30000 });
    await expect(page.locator('.ProseMirror')).toContainText('Initial content. Added more content.', { timeout: 30000 });
  });

  test('another browser can view document via shared link', async ({ browser }) => {
    test.slow();

    const context1 = await browser.newContext();
    const context2 = await browser.newContext();

    const page1 = await context1.newPage();
    const page2 = await context2.newPage();

    try {
      setupPageErrorHandler(page1);
      setupPageErrorHandler(page2);

      await page1.goto('/docs.html#/');
      await waitForAppReady(page1);
      await disableOthersPool(page1);
      await configureBlossomServers(page1);
      await ensureLoggedIn(page1);

      const newDocCard = page1.locator('[role="button"]:has-text("New Document")');
      await page1.keyboard.press('Escape');
      await expect(newDocCard).toBeVisible({ timeout: 15000 });
      await newDocCard.click();

      const docName = `Shared Doc ${Date.now()}`;
      await page1.locator('input[placeholder="Document name..."]').fill(docName);
      await page1.getByRole('button', { name: 'Create' }).click();

      const editor1 = page1.locator('.ProseMirror');
      await expect(editor1).toBeVisible({ timeout: 30000 });
      const content = `Shared content ${Date.now()}`;
      const rootBefore = await page1.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
      await editor1.click();
      await editor1.type(content);
      await expect(editor1).toContainText(content, { timeout: 15000 });
      await waitForTreeRootChange(page1, rootBefore, 60000);

      const pushButton = page1.getByRole('button', { name: 'Push to file servers' });
      await expect(pushButton).toBeVisible({ timeout: 15000 });
      await pushButton.click();

      const pushModal = page1.getByTestId('blossom-push-modal');
      await expect(pushModal).toBeVisible({ timeout: 15000 });
      await pushModal.getByTestId('start-push-btn').click();
      const doneButton = pushModal.getByRole('button', { name: 'Done' });
      await expect(doneButton).toBeVisible({ timeout: 60000 });
      await doneButton.click();
      await expect(pushModal).toBeHidden({ timeout: 15000 });

      const shareUrl = page1.url();
      expect(shareUrl).toContain('/docs.html#/');
      const treeNameMatch = shareUrl.match(/#\/npub1[0-9a-z]+\/(.+)$/);
      const treeName = treeNameMatch ? decodeURIComponent(treeNameMatch[1]) : null;
      expect(treeName).toBeTruthy();
      await page1.waitForFunction(
        async (docTreeName) => {
          const nostrStore = (window as any).__nostrStore;
          const npub = nostrStore?.getState()?.npub;
          if (!npub) return false;
          const { getLocalRootEntry } = await import('/src/treeRootCache');
          const entry = getLocalRootEntry(npub, docTreeName as string);
          return !!entry && entry.dirty === false;
        },
        treeName,
        { timeout: 30000 }
      );

      await page2.goto('/docs.html#/');
      await waitForAppReady(page2);
      await disableOthersPool(page2);
      await configureBlossomServers(page2);

      await page2.goto(shareUrl);
      await waitForAppReady(page2);
      await disableOthersPool(page2);

      await page2.waitForFunction(
        (prefix) => {
          const boxes = Array.from(document.querySelectorAll('[role="textbox"]'));
          const longest = boxes.reduce((best, current) => {
            return (current.textContent || '').length > (best.textContent || '').length ? current : best;
          }, boxes[0] || null);
          const text = (longest?.textContent || '').replace(/\s+/g, ' ');
          return text.includes(prefix);
        },
        'Shared content',
        { timeout: 60000 }
      );
    } finally {
      await context1.close();
      await context2.close();
    }
  });

  test('New Document button shows after auto-login on refresh', async ({ page }) => {
    test.setTimeout(60000);
    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    await page.getByRole('button', { name: /New/i }).click();

    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 15000 });

    try {
      await page.reload({ waitUntil: 'domcontentloaded', timeout: 60000 });
    } catch {
      await page.goto('/docs.html#/', { waitUntil: 'domcontentloaded' });
    }
    await waitForAppReady(page);

    await expect(page.locator('[role="button"]:has-text("New Document")')).toBeVisible({ timeout: 15000 });
  });

  test('editor maintains focus after auto-save in docs app', async ({ page }) => {
    await page.goto('/docs.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    await page.getByRole('button', { name: /New/i }).click();

    const newDocCard = page.locator('[role="button"]:has-text("New Document")');
    await expect(newDocCard).toBeVisible({ timeout: 15000 });

    await page.keyboard.press('Escape');
    await newDocCard.click();

    const docName = `Focus Test ${Date.now()}`;
    await page.locator('input[placeholder="Document name..."]').fill(docName);
    await page.getByRole('button', { name: 'Create' }).click();

    const editor = page.locator('.ProseMirror');
    await expect(editor).toBeVisible({ timeout: 30000 });
    await editor.click();

    const rootBefore = await page.evaluate(() => (window as any).__getTreeRoot?.() ?? null);
    await page.keyboard.type('First sentence.');
    await waitForTreeRootChange(page, rootBefore, 60000);

    const hasFocus = await page.evaluate(() => {
      const active = document.activeElement;
      const editor = document.querySelector('.ProseMirror');
      return editor?.contains(active) || active === editor;
    });
    expect(hasFocus).toBe(true);

    await page.keyboard.type(' Second sentence.');

    await expect(editor).toContainText('First sentence. Second sentence.', { timeout: 30000 });
  });
});
