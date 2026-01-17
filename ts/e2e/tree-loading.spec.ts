import { test, expect } from './fixtures';
import { waitForAppReady, ensureLoggedIn, disableOthersPool, useLocalRelay, presetProductionRelaysInDB, getTestRelayUrl } from './test-utils';

/**
 * Test tree loading with production relays.
 * Run with: pnpm exec playwright test e2e/tree-loading.spec.ts --config=playwright.production.config.ts
 */
test('measure tree loading time', async ({ page }) => {
  test.setTimeout(60000);

  await page.goto('/');
  await presetProductionRelaysInDB(page);
  await page.reload();
  await waitForAppReady(page);
  const testRelayUrl = getTestRelayUrl();
  const isTestMode = await page.evaluate((relayUrl) => {
    const store = (window as any).__settingsStore;
    if (!store?.subscribe) return false;
    let settings: any = null;
    store.subscribe((value: any) => { settings = value; })();
    return settings?.network?.relays?.includes(relayUrl);
  }, testRelayUrl);

  if (!isTestMode) {
    const startTime = Date.now();
    await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-ts');

    // Wait for README content from production relays/Blossom
    await expect(page.getByText('Content-addressed filesystem')).toBeVisible({ timeout: 45000 });

    // Git header should show latest commit (avoid "No commits yet" regressions)
    const commitLink = page.locator('table thead a[href*="commit="]').first();
    await expect(commitLink).toBeVisible({ timeout: 15000 });
    await expect(page.getByText('No commits yet')).not.toBeVisible({ timeout: 5000 });

    const loadTime = Date.now() - startTime;
    console.log(`\n\nTree loading time: ${loadTime}ms\n\n`);

    await page.screenshot({ path: '/tmp/tree-loaded.png', fullPage: true });

    expect(loadTime).toBeLessThan(20000); // Should load in under 20s
    return;
  }

  await ensureLoggedIn(page);
  await disableOthersPool(page);
  await useLocalRelay(page);

  const publicLink = page.getByRole('link', { name: 'public' }).first();
  await expect(publicLink).toBeVisible({ timeout: 20000 });

  const npub = await page.evaluate(() => (window as any).__nostrStore?.getState?.()?.npub || '');
  await page.goto(`/#/${npub}/public`);
  await page.waitForURL(/\/#\/npub.*\/public/, { timeout: 15000 });

  const filename = `README-${Date.now()}.md`;
  const content = 'Content-addressed filesystem';
  const fileInput = page.locator('input[type="file"][multiple]').first();
  await fileInput.setInputFiles({
    name: filename,
    mimeType: 'text/plain',
    buffer: Buffer.from(content, 'utf-8'),
  });

  await expect(page.getByText(filename).first()).toBeVisible({ timeout: 20000 });

  const treeUrl = `/#/${npub}/public`;

  await page.goto('/#/');
  const startTime = Date.now();
  await page.goto(treeUrl);
  await page.waitForFunction(
    async ({ pub, treeName, name }) => {
      const { getTree } = await import('/src/store.ts');
      const { getTreeRootSync } = await import('/src/stores/treeRoot.ts');
      const rootCid = getTreeRootSync(pub, treeName);
      if (!rootCid) return false;
      try {
        const tree = getTree();
        const entry = await tree.resolvePath(rootCid, [name]);
        return entry !== null;
      } catch {
        return false;
      }
    },
    { pub: npub, treeName: 'public', name: filename },
    { timeout: 15000, polling: 500 }
  );

  const loadTime = Date.now() - startTime;
  console.log(`\n\nTree loading time (test mode): ${loadTime}ms\n\n`);

  await page.screenshot({ path: '/tmp/tree-loaded.png', fullPage: true });

  expect(loadTime).toBeLessThan(15000);
});

test('direct nav loads repo on first page load', async ({ page }) => {
  test.setTimeout(60000);
  await page.goto('/');
  await presetProductionRelaysInDB(page);
  await page.reload();
  await waitForAppReady(page);

  await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-ts');
  await waitForAppReady(page);

  await expect(page.getByText('Content-addressed filesystem')).toBeVisible({ timeout: 45000 });
  const commitLink = page.locator('table thead a[href*="commit="]').first();
  await expect(commitLink).toBeVisible({ timeout: 15000 });
  await expect(page.getByText('No commits yet')).not.toBeVisible({ timeout: 5000 });
});
