import { describe, expect, it, vi, afterEach } from 'vitest';
import { FallbackStore } from '../src/store/fallback.js';
import { sha256 } from '../src/hash.js';
import type { Store } from '../src/types.js';

function makePrimary(): Store {
  return {
    put: vi.fn().mockResolvedValue(true),
    get: vi.fn().mockResolvedValue(null),
    has: vi.fn().mockResolvedValue(false),
    delete: vi.fn().mockResolvedValue(false),
  };
}

describe('FallbackStore', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('returns the first successful fallback instead of waiting in fallback order', async () => {
    const hash = await sha256(new Uint8Array([2]));
    vi.useFakeTimers();

    const primary = makePrimary();
    const slow = {
      get: vi.fn().mockImplementation(
        () => new Promise<Uint8Array | null>((resolve) => setTimeout(() => resolve(new Uint8Array([2])), 200))
      ),
    };
    const fast = {
      get: vi.fn().mockImplementation(
        () => new Promise<Uint8Array | null>((resolve) => setTimeout(() => resolve(new Uint8Array([2])), 50))
      ),
    };

    const store = new FallbackStore({
      primary,
      fallbacks: [slow, fast],
      timeout: 500,
    });

    const readPromise = store.get(hash);
    await vi.advanceTimersByTimeAsync(50);

    await expect(readPromise).resolves.toEqual(new Uint8Array([2]));
    expect(primary.put).toHaveBeenCalledWith(hash, new Uint8Array([2]));
  });

  it('still caches late fallback data after the initial timeout result', async () => {
    const hash = await sha256(new Uint8Array([9]));
    vi.useFakeTimers();

    const primary = makePrimary();
    const slow = {
      get: vi.fn().mockImplementation(
        () => new Promise<Uint8Array | null>((resolve) => setTimeout(() => resolve(new Uint8Array([9])), 120))
      ),
    };
    const miss = {
      get: vi.fn().mockResolvedValue(null),
    };

    const store = new FallbackStore({
      primary,
      fallbacks: [slow, miss],
      timeout: 50,
    });

    const readPromise = store.get(hash);
    await vi.advanceTimersByTimeAsync(55);
    await expect(readPromise).resolves.toBeNull();

    await vi.advanceTimersByTimeAsync(100);
    await vi.runAllTicks();

    await vi.waitFor(() => {
      expect(primary.put).toHaveBeenCalledWith(hash, new Uint8Array([9]));
    });
  });

  it('coalesces concurrent fallback reads for the same hash', async () => {
    const hash = await sha256(new Uint8Array([7]));
    vi.useFakeTimers();

    const primary = makePrimary();
    const fallback = {
      get: vi.fn().mockImplementation(
        () => new Promise<Uint8Array | null>((resolve) => setTimeout(() => resolve(new Uint8Array([7])), 50))
      ),
    };

    const store = new FallbackStore({
      primary,
      fallbacks: [fallback],
      timeout: 500,
    });

    const first = store.get(hash);
    const second = store.get(hash);

    await vi.advanceTimersByTimeAsync(50);

    await expect(first).resolves.toEqual(new Uint8Array([7]));
    await expect(second).resolves.toEqual(new Uint8Array([7]));
    expect(fallback.get).toHaveBeenCalledTimes(1);
    expect(primary.put).toHaveBeenCalledTimes(1);
    expect(primary.put).toHaveBeenCalledWith(hash, new Uint8Array([7]));
  });
});
