import { test, expect } from './fixtures';
import { waitForAppReady } from './test-utils';

/**
 * Direct navigation should not wait for relay connections before worker init.
 * To simulate slow relay handshakes, run with:
 * RELAY_HANDSHAKE_DELAY_MS=4000 pnpm exec playwright test e2e/direct-nav-worker-init.spec.ts
 */
test('direct navigation initializes worker quickly', async ({ page }) => {
  test.slow();

  await page.goto('/#/npub1xndmdgymsf4a34rzr7346vp8qcptxf75pjqweh8naa8rklgxpfqqmfjtce/hashtree-ts');
  await waitForAppReady(page);

  const startTime = Date.now();
  await page.waitForFunction(() => {
    const getAdapter = (window as any).__getWorkerAdapter;
    return typeof getAdapter === 'function' && !!getAdapter();
  }, null, { timeout: 3000 });

  const elapsed = Date.now() - startTime;
  console.log(`[e2e] Worker adapter ready in ${elapsed}ms`);
  expect(elapsed).toBeLessThan(3000);
});
