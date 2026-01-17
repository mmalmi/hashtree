/**
 * Unit tests for DexieStore
 */
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { DexieStore } from './index.js';

// Helper to create a hash from data (simplified for tests)
function makeHash(data: Uint8Array): Uint8Array {
  // Create a 32-byte hash-like value (not cryptographic, just for testing)
  const hash = new Uint8Array(32);
  for (let i = 0; i < data.length && i < 32; i++) {
    hash[i] = data[i];
  }
  // Add some uniqueness based on length
  hash[31] = data.length % 256;
  return hash;
}

describe('DexieStore', () => {
  let store: DexieStore;
  const TEST_DB_NAME = 'test-dexie-store';

  beforeEach(async () => {
    // Delete any existing test database
    await DexieStore.deleteDatabase(TEST_DB_NAME);
    store = new DexieStore(TEST_DB_NAME);
  });

  afterEach(async () => {
    store.close();
    await DexieStore.deleteDatabase(TEST_DB_NAME);
  });

  describe('put and get', () => {
    it('should store and retrieve data', async () => {
      const data = new TextEncoder().encode('hello world');
      const hash = makeHash(data);

      await store.put(hash, data);
      const retrieved = await store.get(hash);

      expect(retrieved).not.toBeNull();
      expect(new TextDecoder().decode(retrieved!)).toBe('hello world');
    });

    it('should return null for non-existent hash', async () => {
      const hash = makeHash(new Uint8Array([1, 2, 3]));
      const retrieved = await store.get(hash);
      expect(retrieved).toBeNull();
    });

    it('should handle empty data', async () => {
      const data = new Uint8Array(0);
      const hash = makeHash(data);

      await store.put(hash, data);
      const retrieved = await store.get(hash);

      expect(retrieved).not.toBeNull();
      expect(retrieved!.length).toBe(0);
    });

    it('should handle binary data', async () => {
      const data = new Uint8Array([0, 1, 127, 128, 255]);
      const hash = makeHash(data);

      await store.put(hash, data);
      const retrieved = await store.get(hash);

      expect(retrieved).not.toBeNull();
      expect(Array.from(retrieved!)).toEqual([0, 1, 127, 128, 255]);
    });
  });

  describe('has', () => {
    it('should return true for existing data', async () => {
      const data = new TextEncoder().encode('test');
      const hash = makeHash(data);

      await store.put(hash, data);
      expect(await store.has(hash)).toBe(true);
    });

    it('should return false for non-existent data', async () => {
      const hash = makeHash(new Uint8Array([1, 2, 3]));
      expect(await store.has(hash)).toBe(false);
    });
  });

  describe('delete', () => {
    it('should delete existing data', async () => {
      const data = new TextEncoder().encode('to delete');
      const hash = makeHash(data);

      await store.put(hash, data);
      expect(await store.has(hash)).toBe(true);

      const deleted = await store.delete(hash);
      expect(deleted).toBe(true);
      expect(await store.has(hash)).toBe(false);
    });

    it('should return false for non-existent data', async () => {
      const hash = makeHash(new Uint8Array([1, 2, 3]));
      const deleted = await store.delete(hash);
      expect(deleted).toBe(false);
    });
  });

  describe('count and totalBytes', () => {
    it('should count items correctly', async () => {
      expect(await store.count()).toBe(0);

      const data1 = new TextEncoder().encode('one');
      const data2 = new TextEncoder().encode('two');
      const data3 = new TextEncoder().encode('three');

      await store.put(makeHash(data1), data1);
      expect(await store.count()).toBe(1);

      await store.put(makeHash(data2), data2);
      expect(await store.count()).toBe(2);

      await store.put(makeHash(data3), data3);
      expect(await store.count()).toBe(3);
    });

    it('should calculate total bytes correctly', async () => {
      expect(await store.totalBytes()).toBe(0);

      const data1 = new Uint8Array(100);
      const data2 = new Uint8Array(200);
      data2[0] = 1; // Make it different from data1

      await store.put(makeHash(data1), data1);
      expect(await store.totalBytes()).toBe(100);

      await store.put(makeHash(data2), data2);
      expect(await store.totalBytes()).toBe(300);
    });
  });

  describe('keys', () => {
    it('should return all stored hashes', async () => {
      const data1 = new TextEncoder().encode('one');
      const data2 = new TextEncoder().encode('two');
      const hash1 = makeHash(data1);
      const hash2 = makeHash(data2);

      await store.put(hash1, data1);
      await store.put(hash2, data2);

      const keys = await store.keys();
      expect(keys.length).toBe(2);
    });

    it('should return empty array for empty store', async () => {
      const keys = await store.keys();
      expect(keys.length).toBe(0);
    });
  });

  describe('clear', () => {
    it('should remove all data', async () => {
      const data1 = new TextEncoder().encode('one');
      const data2 = new TextEncoder().encode('two');

      await store.put(makeHash(data1), data1);
      await store.put(makeHash(data2), data2);
      expect(await store.count()).toBe(2);

      await store.clear();
      expect(await store.count()).toBe(0);
    });
  });

  describe('persistence', () => {
    it('should persist data across store instances', async () => {
      const data = new TextEncoder().encode('persistent');
      const hash = makeHash(data);

      await store.put(hash, data);
      store.close();

      // Create new instance with same db name
      const store2 = new DexieStore(TEST_DB_NAME);
      const retrieved = await store2.get(hash);

      expect(retrieved).not.toBeNull();
      expect(new TextDecoder().decode(retrieved!)).toBe('persistent');

      store2.close();
    });
  });

  describe('evict', () => {
    it('should evict entries when over limit', async () => {
      // Add 3 entries of 100 bytes each (300 bytes total)
      const data1 = new Uint8Array(100);
      const data2 = new Uint8Array(100);
      const data3 = new Uint8Array(100);
      data1[0] = 1;
      data2[0] = 2;
      data3[0] = 3;

      const hash1 = makeHash(data1);
      const hash2 = makeHash(data2);
      const hash3 = makeHash(data3);

      await store.put(hash1, data1);
      // Small delay to ensure different lastAccess times
      await new Promise(r => setTimeout(r, 10));
      await store.put(hash2, data2);
      await new Promise(r => setTimeout(r, 10));
      await store.put(hash3, data3);

      expect(await store.count()).toBe(3);
      expect(await store.totalBytes()).toBe(300);

      // Evict to max 150 bytes - should remove oldest entries
      const evicted = await store.evict(150);
      expect(evicted).toBeGreaterThan(0);

      const remaining = await store.totalBytes();
      expect(remaining).toBeLessThanOrEqual(150);
    });

    it('should not evict when under limit', async () => {
      const data = new Uint8Array(100);
      const hash = makeHash(data);
      await store.put(hash, data);

      const evicted = await store.evict(1000);
      expect(evicted).toBe(0);
      expect(await store.count()).toBe(1);
    });

    it('should evict least recently used first', async () => {
      const data1 = new Uint8Array(100);
      const data2 = new Uint8Array(100);
      const data3 = new Uint8Array(100);
      data1[0] = 1;
      data2[0] = 2;
      data3[0] = 3;

      const hash1 = makeHash(data1);
      const hash2 = makeHash(data2);
      const hash3 = makeHash(data3);

      // Put in order: 1, 2, 3 with delays to ensure different lastAccess times
      await store.put(hash1, data1);
      await new Promise(r => setTimeout(r, 50));
      await store.put(hash2, data2);
      await new Promise(r => setTimeout(r, 50));
      await store.put(hash3, data3);

      // Access hash1 to make it recently used
      await new Promise(r => setTimeout(r, 50));
      await store.get(hash1);
      // Wait for the fire-and-forget lastAccess update to complete
      await new Promise(r => setTimeout(r, 100));

      // Evict to max 200 bytes - should remove hash2 (oldest after hash1 was accessed)
      await store.evict(200);

      // hash1 and hash3 should remain (most recently accessed/created)
      expect(await store.has(hash1)).toBe(true);
      expect(await store.has(hash2)).toBe(false);
      expect(await store.has(hash3)).toBe(true);
    });

    it('should return 0 when store is empty', async () => {
      const evicted = await store.evict(100);
      expect(evicted).toBe(0);
    });
  });
});
