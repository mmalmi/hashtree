/**
 * Storage benchmark tests - compares IndexedDB and OPFS performance
 */
import { test, expect } from './fixtures';

test.setTimeout(60000);

test.describe('Storage Benchmark', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await page.waitForSelector('header', { timeout: 10000 });
    await page.waitForFunction(() => !!(window as any).__hashtree, { timeout: 10000 });

    // Clear storage before each test
    await page.evaluate(async () => {
      const root = await navigator.storage.getDirectory();
      // @ts-ignore
      for await (const [name] of root.entries()) {
        await root.removeEntry(name, { recursive: true });
      }
    });
  });

  test('Dexie vs OPFS write/read performance', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { DexieStore, OpfsStore, sha256 } = window.__hashtree || {};
      if (!DexieStore || !OpfsStore || !sha256) {
        return { error: 'Missing DexieStore/OpfsStore/sha256 in hashtree bundle' };
      }

      const iterations = 60;
      const dataSize = 1024;
      const testData: { hash: Uint8Array; data: Uint8Array }[] = [];

      for (let i = 0; i < iterations; i++) {
        const data = new Uint8Array(dataSize);
        crypto.getRandomValues(data);
        const hash = await sha256(data);
        testData.push({ hash, data });
      }

      // Dexie (IndexedDB)
      await DexieStore.deleteDatabase('benchmark-dexie');
      const dexieStore = new DexieStore('benchmark-dexie');
      const dexieWriteStart = performance.now();
      for (const { hash, data } of testData) {
        await dexieStore.put(hash, data);
      }
      const dexieWriteEnd = performance.now();

      // Read back from new instance
      const dexieStore2 = new DexieStore('benchmark-dexie');
      const dexieReadStart = performance.now();
      let dexieVerified = 0;
      for (const { hash, data } of testData) {
        const retrieved = await dexieStore2.get(hash);
        if (retrieved && retrieved.length === data.length) dexieVerified++;
      }
      const dexieReadEnd = performance.now();
      dexieStore.close();
      dexieStore2.close();
      await DexieStore.deleteDatabase('benchmark-dexie');

      // OPFS
      const opfsStore = new OpfsStore('benchmark-opfs');
      const opfsWriteStart = performance.now();
      for (const { hash, data } of testData) {
        await opfsStore.put(hash, data);
      }
      const opfsWriteEnd = performance.now();

      // Read back from new instance
      const opfsStore2 = new OpfsStore('benchmark-opfs');
      const opfsReadStart = performance.now();
      let opfsVerified = 0;
      for (const { hash, data } of testData) {
        const retrieved = await opfsStore2.get(hash);
        if (retrieved && retrieved.length === data.length) opfsVerified++;
      }
      const opfsReadEnd = performance.now();
      await opfsStore.clear();
      await opfsStore.close();

      return {
        dexieWrite: Math.round(dexieWriteEnd - dexieWriteStart),
        dexieRead: Math.round(dexieReadEnd - dexieReadStart),
        dexieVerified,
        opfsWrite: Math.round(opfsWriteEnd - opfsWriteStart),
        opfsRead: Math.round(opfsReadEnd - opfsReadStart),
        opfsVerified,
        iterations,
        error: null,
      };
    });

    if (result.error) {
      throw new Error(result.error);
    }
    expect(result.dexieVerified).toBe(result.iterations);
    expect(result.opfsVerified).toBe(result.iterations);
  });
});
