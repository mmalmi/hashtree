import { describe, it, expect, beforeEach } from 'vitest';
import { MemoryStore } from '../src/store/memory.js';
import { sha256 } from '../src/hash.js';
import { toHex } from '../src/types.js';

describe('MemoryStore', () => {
  let store: MemoryStore;

  beforeEach(() => {
    store = new MemoryStore();
  });

  describe('put', () => {
    it('should store data and return true for new data', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      const result = await store.put(hash, data);
      expect(result).toBe(true);
    });

    it('should return false for duplicate data', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      const result = await store.put(hash, data);
      expect(result).toBe(false);
    });

    it('should store copy of data', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      data[0] = 99; // Mutate original

      const retrieved = await store.get(hash);
      expect(retrieved![0]).toBe(1); // Should be original value
    });
  });

  describe('get', () => {
    it('should return data for existing hash', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      const result = await store.get(hash);

      expect(result).toEqual(data);
    });

    it('should return null for non-existent hash', async () => {
      const hash = new Uint8Array(32).fill(0);
      const result = await store.get(hash);
      expect(result).toBeNull();
    });

    it('should return copy of data', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      const retrieved = await store.get(hash);
      retrieved![0] = 99; // Mutate retrieved

      const secondRetrieve = await store.get(hash);
      expect(secondRetrieve![0]).toBe(1); // Should be original
    });
  });

  describe('has', () => {
    it('should return true for existing data', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      expect(await store.has(hash)).toBe(true);
    });

    it('should return false for non-existent data', async () => {
      const hash = new Uint8Array(32).fill(0);
      expect(await store.has(hash)).toBe(false);
    });
  });

  describe('delete', () => {
    it('should delete existing data', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      const result = await store.delete(hash);

      expect(result).toBe(true);
      expect(await store.has(hash)).toBe(false);
    });

    it('should return false for non-existent data', async () => {
      const hash = new Uint8Array(32).fill(0);
      const result = await store.delete(hash);
      expect(result).toBe(false);
    });
  });

  describe('size', () => {
    it('should return 0 for empty store', () => {
      expect(store.size).toBe(0);
    });

    it('should return correct count', async () => {
      const data1 = new Uint8Array([1]);
      const data2 = new Uint8Array([2]);
      const hash1 = await sha256(data1);
      const hash2 = await sha256(data2);

      await store.put(hash1, data1);
      await store.put(hash2, data2);

      expect(store.size).toBe(2);
    });
  });

  describe('totalBytes', () => {
    it('should return 0 for empty store', () => {
      expect(store.totalBytes).toBe(0);
    });

    it('should return correct total', async () => {
      const data1 = new Uint8Array([1, 2, 3]);
      const data2 = new Uint8Array([4, 5]);
      const hash1 = await sha256(data1);
      const hash2 = await sha256(data2);

      await store.put(hash1, data1);
      await store.put(hash2, data2);

      expect(store.totalBytes).toBe(5);
    });
  });

  describe('clear', () => {
    it('should remove all data', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await sha256(data);

      await store.put(hash, data);
      store.clear();

      expect(store.size).toBe(0);
      expect(await store.has(hash)).toBe(false);
    });
  });

  describe('keys', () => {
    it('should return empty array for empty store', () => {
      expect(store.keys()).toEqual([]);
    });

    it('should return all hashes', async () => {
      const data1 = new Uint8Array([1]);
      const data2 = new Uint8Array([2]);
      const hash1 = await sha256(data1);
      const hash2 = await sha256(data2);

      await store.put(hash1, data1);
      await store.put(hash2, data2);

      const keys = store.keys();
      expect(keys.length).toBe(2);

      const hexKeys = keys.map(toHex).sort();
      const expectedKeys = [toHex(hash1), toHex(hash2)].sort();
      expect(hexKeys).toEqual(expectedKeys);
    });
  });
});
