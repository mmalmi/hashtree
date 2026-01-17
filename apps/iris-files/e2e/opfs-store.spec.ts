/**
 * E2E tests for OpfsStore - Origin Private File System based store
 *
 * OPFS requires a real browser context, so these tests run via Playwright.
 * Tests are executed in the browser via page.evaluate().
 *
 * We expose hashtree module on window.__hashtree for testing.
 */
import { test, expect } from './fixtures';
import { disableOthersPool } from './test-utils.js';

// Extend timeout for this test file since OPFS operations can be slow
test.setTimeout(30000);

test.describe('OpfsStore', () => {
  // Serial mode: tests share OPFS and would interfere in parallel
  test.describe.configure({ mode: 'serial' });

  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await disableOthersPool(page); // Prevent WebRTC cross-talk from parallel tests

    // Wait for app to be ready and hashtree to be exposed on window
    await page.waitForSelector('header', { timeout: 10000 });
    await page.waitForFunction(() => !!(window as any).__hashtree, { timeout: 10000 });

    // Clear OPFS before each test
    await page.evaluate(async () => {
      const root = await navigator.storage.getDirectory();
      // @ts-ignore - entries() exists on FileSystemDirectoryHandle
      for await (const [name] of root.entries()) {
        await root.removeEntry(name, { recursive: true });
      }
    });
  });

  test('put should store data and return true for new data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      return await store.put(hash, data);
    });

    expect(result).toBe(true);
  });

  test('put should return false for duplicate data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      return await store.put(hash, data);
    });

    expect(result).toBe(false);
  });

  test('get should return data for existing hash', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      const retrieved = await store.get(hash);

      return retrieved ? Array.from(retrieved) : null;
    });

    expect(result).toEqual([1, 2, 3]);
  });

  test('get should return null for non-existent hash', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const hash = new Uint8Array(32).fill(0);

      return await store.get(hash);
    });

    expect(result).toBeNull();
  });

  test('has should return true for existing data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      return await store.has(hash);
    });

    expect(result).toBe(true);
  });

  test('has should return false for non-existent data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const hash = new Uint8Array(32).fill(0);

      return await store.has(hash);
    });

    expect(result).toBe(false);
  });

  test('delete should remove existing data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      const deleted = await store.delete(hash);
      const exists = await store.has(hash);

      return { deleted, exists };
    });

    expect(result.deleted).toBe(true);
    expect(result.exists).toBe(false);
  });

  test('delete should return false for non-existent data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const hash = new Uint8Array(32).fill(0);

      return await store.delete(hash);
    });

    expect(result).toBe(false);
  });

  test('keys should return all stored hashes', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256, toHex } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data1 = new Uint8Array([1]);
      const data2 = new Uint8Array([2]);
      const hash1 = await sha256(data1);
      const hash2 = await sha256(data2);

      await store.put(hash1, data1);
      await store.put(hash2, data2);

      const keys = await store.keys();
      return keys.map((k: Uint8Array) => toHex(k)).sort();
    });

    expect(result.length).toBe(2);
  });

  test('clear should remove all data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      await store.clear();

      const count = await store.count();
      const exists = await store.has(hash);

      return { count, exists };
    });

    expect(result.count).toBe(0);
    expect(result.exists).toBe(false);
  });

  test('count should return number of stored items', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data1 = new Uint8Array([1]);
      const data2 = new Uint8Array([2]);
      const hash1 = await sha256(data1);
      const hash2 = await sha256(data2);

      const countBefore = await store.count();
      await store.put(hash1, data1);
      await store.put(hash2, data2);
      const countAfter = await store.count();

      return { countBefore, countAfter };
    });

    expect(result.countBefore).toBe(0);
    expect(result.countAfter).toBe(2);
  });

  test('totalBytes should return total size of stored data', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data1 = new Uint8Array([1, 2, 3]); // 3 bytes
      const data2 = new Uint8Array([4, 5]); // 2 bytes
      const hash1 = await sha256(data1);
      const hash2 = await sha256(data2);

      await store.put(hash1, data1);
      await store.put(hash2, data2);

      return await store.totalBytes();
    });

    expect(result).toBe(5);
  });

  test('data should persist across store instances', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const hash = await sha256(data);

      // Store with first instance
      const store1 = new OpfsStore('persist-test');
      await store1.put(hash, data);
      await store1.close(); // Flush pending writes before creating new instance

      // Retrieve with new instance
      const store2 = new OpfsStore('persist-test');
      const retrieved = await store2.get(hash);
      await store2.close(); // Close store to release OPFS locks

      return retrieved ? Array.from(retrieved) : null;
    });

    expect(result).toEqual([1, 2, 3, 4, 5]);
  });

  test('separate stores should be isolated', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store1 = new OpfsStore('store-a');
      const store2 = new OpfsStore('store-b');

      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store1.put(hash, data);

      const existsInStore1 = await store1.has(hash);
      const existsInStore2 = await store2.has(hash);

      // Close stores to release OPFS locks
      await store1.close();
      await store2.close();

      return { existsInStore1, existsInStore2 };
    });

    expect(result.existsInStore1).toBe(true);
    expect(result.existsInStore2).toBe(false);
  });

  test('should handle large files', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      // 1MB of data
      const data = new Uint8Array(1024 * 1024);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }
      const hash = await sha256(data);

      await store.put(hash, data);
      const retrieved = await store.get(hash);

      if (!retrieved) {
        await store.close();
        return { success: false, sizeMatch: false };
      }

      // Verify size and some sample values
      const result = {
        success: true,
        sizeMatch: retrieved.length === data.length,
        firstByte: retrieved[0],
        lastByte: retrieved[retrieved.length - 1],
      };

      await store.close(); // Release OPFS locks
      return result;
    });

    expect(result.success).toBe(true);
    expect(result.sizeMatch).toBe(true);
    expect(result.firstByte).toBe(0);
    expect(result.lastByte).toBe(255);
  });

  test('close should flush and cleanup', async ({ page }) => {
    const result = await page.evaluate(async () => {
      // @ts-ignore
      const { OpfsStore, sha256 } = window.__hashtree;

      const store = new OpfsStore('test-store');
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      await store.close();

      // Create new instance to verify data persisted
      const store2 = new OpfsStore('test-store');
      const retrieved = await store2.get(hash);

      return retrieved ? Array.from(retrieved) : null;
    });

    expect(result).toEqual([1, 2, 3]);
  });
});
