import { describe, expect, it } from 'vitest';
import { BLOB_MAX_BYTES } from '../src/blob-route.js';
import { sha256 } from '../src/hash.js';
import { FallbackStore } from '../src/store/fallback.js';
import { MemoryStore } from '../src/store/memory.js';
import { readFile } from '../src/tree/read.js';

describe('FallbackStore remote content integrity', () => {
  it('does not return or cache a corrupt remote blob', async () => {
    const expected = new TextEncoder().encode('expected content');
    const hash = await sha256(expected);
    const primary = new MemoryStore();
    const corrupt = new MemoryStore();
    await corrupt.put(hash, new TextEncoder().encode('attacker content'));
    const store = new FallbackStore({ primary, fallbacks: [corrupt] });

    expect(await readFile(store, hash)).toBeNull();
    expect(await primary.get(hash)).toBeNull();
  });

  it('lets a valid response win after a faster corrupt fallback', async () => {
    const expected = new TextEncoder().encode('verified content');
    const hash = await sha256(expected);
    const primary = new MemoryStore();
    const corrupt = new MemoryStore();
    const honest = new MemoryStore();
    await corrupt.put(hash, new TextEncoder().encode('attacker content'));
    await honest.put(hash, expected);
    const store = new FallbackStore({
      primary,
      fallbacks: [corrupt, {
        get: async (requestedHash) => {
          await new Promise((resolve) => setTimeout(resolve, 10));
          return honest.get(requestedHash);
        },
      }],
    });

    expect(await readFile(store, hash)).toEqual(expected);
    expect(await primary.get(hash)).toEqual(expected);
  });

  it('does not cache corrupt data that arrives after a fallback timeout', async () => {
    const hash = await sha256(new Uint8Array([1, 2, 3]));
    const primary = new MemoryStore();
    let deliver!: (data: Uint8Array) => void;
    const response = new Promise<Uint8Array>((resolve) => { deliver = resolve; });
    const store = new FallbackStore({
      primary,
      fallbacks: [{ get: () => response }],
      timeout: 1,
    });

    expect(await store.get(hash)).toBeNull();
    deliver(new Uint8Array([9, 9, 9]));
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(await primary.get(hash)).toBeNull();
  });

  it('rejects oversized fallback blobs even when their hash matches', async () => {
    const data = new Uint8Array(BLOB_MAX_BYTES + 1);
    const hash = await sha256(data);
    const primary = new MemoryStore();
    const store = new FallbackStore({ primary, fallbacks: [{ get: async () => data }] });

    expect((await store.get(hash)) === null).toBe(true);
    expect(await primary.has(hash)).toBe(false);
  });
});
