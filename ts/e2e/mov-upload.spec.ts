import { test, expect } from './fixtures';
import { setupPageErrorHandler, disableOthersPool } from './test-utils';

test.describe('Video App', () => {
  test('SharedArrayBuffer and cross-origin isolation enabled', async ({ page }) => {
    test.setTimeout(30000);
    setupPageErrorHandler(page);

    await page.goto('/video.html#/');
    await disableOthersPool(page);

    // Wait for page to load and SW to initialize (may reload for COOP/COEP)
    await page.waitForTimeout(3000);

    // Check SharedArrayBuffer availability
    const sabAvailable = await page.evaluate(() => {
      return typeof SharedArrayBuffer !== 'undefined';
    });
    expect(sabAvailable).toBe(true);

    // Check crossOriginIsolated
    const coiStatus = await page.evaluate(() => {
      return (self as any).crossOriginIsolated;
    });
    expect(coiStatus).toBe(true);
  });
});
