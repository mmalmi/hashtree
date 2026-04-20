import { afterEach, describe, expect, it, vi } from 'vitest';
import { MemoryStore, type Hash } from '@hashtree/core';
import { MeshRouterStore, type MeshReadSource } from '../src/capabilities/meshRouterStore.js';

const HASH_A = new Uint8Array(32).fill(1) as Hash;
const HASH_B = new Uint8Array(32).fill(2) as Hash;

function delayedSource(
  id: string,
  delayMs: number,
  value: Uint8Array | null,
  calls: { count: number },
): MeshReadSource {
  return {
    id,
    get: () => {
      calls.count += 1;
      return new Promise<Uint8Array | null>((resolve) => {
        setTimeout(() => resolve(value), delayMs);
      });
    },
  };
}

describe('MeshRouterStore', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('does not hedge to an unproven second source during a cold remote read', async () => {
    vi.useFakeTimers();
    const primary = new MemoryStore();
    const slowCalls = { count: 0 };
    const fastCalls = { count: 0 };
    const slowData = new Uint8Array([1]);
    const fastData = new Uint8Array([2]);
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 500,
      sources: [
        delayedSource('webrtc', 200, slowData, slowCalls),
        delayedSource('blossom', 50, fastData, fastCalls),
      ],
    });

    const pending = router.getDetailed(HASH_A);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(75);
    await vi.advanceTimersByTimeAsync(50);
    await expect(pending).resolves.toEqual({ data: fastData, sourceId: 'blossom' });
    expect(slowCalls.count).toBe(0);
    expect(fastCalls.count).toBe(1);
    await expect(primary.get(HASH_A)).resolves.toEqual(fastData);
  });

  it('prefers the previously successful source before hedging to the slower one', async () => {
    vi.useFakeTimers();
    const primary = new MemoryStore();
    const peerCalls = { count: 0 };
    const blossomCalls = { count: 0 };
    const peerData = new Uint8Array([11]);
    const blossomData = new Uint8Array([22]);
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 500,
      sources: [
        delayedSource('webrtc', 20, peerData, peerCalls),
        delayedSource('blossom', 200, blossomData, blossomCalls),
      ],
    });

    const first = router.getDetailed(HASH_A);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(100);
    await expect(first).resolves.toEqual({ data: peerData, sourceId: 'webrtc' });

    const second = router.getDetailed(HASH_B);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(100);
    await expect(second).resolves.toEqual({ data: peerData, sourceId: 'webrtc' });
  });

  it('supports remote-only filtered reads without consulting primary storage', async () => {
    const primary = new MemoryStore();
    const localData = new Uint8Array([5]);
    const remoteData = new Uint8Array([9]);
    const blossomCalls = { count: 0 };
    await primary.put(HASH_A, localData);

    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      sources: [
        {
          id: 'blossom',
          get: async () => {
            blossomCalls.count += 1;
            return remoteData;
          },
        },
      ],
    });

    await expect(router.getDetailed(HASH_A)).resolves.toEqual({ data: localData, sourceId: 'idb' });
    await expect(router.getDetailed(HASH_A, {
      skipPrimary: true,
      sourceIds: ['blossom'],
    })).resolves.toEqual({ data: remoteData, sourceId: 'blossom' });
    expect(blossomCalls.count).toBe(1);
  });

  it('falls back to remote sources when primary storage read stalls', async () => {
    vi.useFakeTimers();
    const remoteData = new Uint8Array([42]);
    const remoteCalls = { count: 0 };
    const primary = {
      put: vi.fn(async () => true),
      get: vi.fn(() => new Promise<Uint8Array | null>(() => {})),
      has: vi.fn(async () => false),
      delete: vi.fn(async () => false),
    };
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 250,
      requestTimeoutMs: 500,
      sources: [
        delayedSource('blossom', 50, remoteData, remoteCalls),
      ],
    });

    const pending = router.getDetailed(HASH_A);
    await Promise.resolve();
    expect(remoteCalls.count).toBe(0);

    await vi.advanceTimersByTimeAsync(250);
    expect(remoteCalls.count).toBe(1);

    await vi.advanceTimersByTimeAsync(50);
    await expect(pending).resolves.toEqual({ data: remoteData, sourceId: 'blossom' });
  });

  it('gives the last hedged source a full chance instead of clipping it at query start', async () => {
    vi.useFakeTimers();
    const primary = new MemoryStore();
    const warmCalls = { count: 0 };
    const missCalls = { count: 0 };
    const slowWarmMissCalls = { count: 0 };
    const slowCalls = { count: 0 };
    const warmData = new Uint8Array([7]);
    const slowData = new Uint8Array([99]);
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      requestTimeoutMs: 120,
      dispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 50,
      },
      sources: [
        {
          id: 'first-miss',
          get: async (hash) => {
            if (hash === HASH_B) {
              return delayedSource('warm', 10, warmData, warmCalls).get(hash);
            }
            return delayedSource('miss', 200, null, missCalls).get(hash);
          },
        },
        {
          id: 'late-hit',
          get: async (hash) => {
            if (hash === HASH_B) {
              return delayedSource('late-warm-miss', 20, null, slowWarmMissCalls).get(hash);
            }
            return delayedSource('late-hit', 100, slowData, slowCalls).get(hash);
          },
        },
      ],
    });

    const warm = router.getDetailed(HASH_B, { skipPrimary: true });
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(20);
    await expect(warm).resolves.toEqual({ data: warmData, sourceId: 'first-miss' });
    expect(slowWarmMissCalls.count).toBe(0);

    const pending = router.getDetailed(HASH_A, { skipPrimary: true });
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    expect(missCalls.count).toBe(1);
    expect(slowCalls.count).toBe(0);

    await vi.advanceTimersByTimeAsync(50);
    expect(slowCalls.count).toBe(1);

    await vi.advanceTimersByTimeAsync(100);
    await expect(pending).resolves.toEqual({ data: slowData, sourceId: 'late-hit' });
  });

  it('coalesces concurrent remote reads for the same hash and filter set', async () => {
    vi.useFakeTimers();
    const primary = new MemoryStore();
    let calls = 0;
    const data = new Uint8Array([77]);
    const router = new MeshRouterStore({
      primary,
      requestTimeoutMs: 500,
      sources: [
        {
          id: 'blossom',
          get: () => {
            calls += 1;
            return new Promise<Uint8Array | null>((resolve) => {
              setTimeout(() => resolve(data), 50);
            });
          },
        },
      ],
    });

    const first = router.getDetailed(HASH_A, { skipPrimary: true, sourceIds: ['blossom'] });
    const second = router.getDetailed(HASH_A, { skipPrimary: true, sourceIds: ['blossom'] });
    await Promise.resolve();

    expect(calls).toBe(1);

    await vi.advanceTimersByTimeAsync(50);
    await expect(first).resolves.toEqual({ data, sourceId: 'blossom' });
    await expect(second).resolves.toEqual({ data, sourceId: 'blossom' });
  });

  it('matches grouped dynamic endpoints when filtering by sourceIds', async () => {
    const primary = new MemoryStore();
    const aCalls = { count: 0 };
    const bCalls = { count: 0 };
    const aData = new Uint8Array([1, 2, 3]);
    const router = new MeshRouterStore({
      primary,
      sourceProviders: [
        () => [
          {
            id: 'blossom:https://a.example',
            groupId: 'blossom',
            get: async () => {
              aCalls.count += 1;
              return aData;
            },
          },
          {
            id: 'peer:nostr-peer',
            groupId: 'p2p',
            get: async () => {
              bCalls.count += 1;
              return new Uint8Array([9]);
            },
          },
        ],
      ],
    });

    await expect(router.getDetailed(HASH_A, {
      skipPrimary: true,
      sourceIds: ['blossom'],
    })).resolves.toEqual({ data: aData, sourceId: 'blossom:https://a.example' });

    expect(aCalls.count).toBe(1);
    expect(bCalls.count).toBe(0);
  });

  it('copies remote source bytes before caching and returning them', async () => {
    const primary = new MemoryStore();
    const sourceData = new Uint8Array([7, 8, 9]);
    const router = new MeshRouterStore({
      primary,
      sources: [
        {
          id: 'webrtc',
          get: async () => sourceData,
        },
      ],
    });

    const result = await router.getDetailed(HASH_A, { skipPrimary: true });
    expect(result).toEqual({ data: new Uint8Array([7, 8, 9]), sourceId: 'webrtc' });
    expect(result?.data).not.toBe(sourceData);

    sourceData[0] = 99;

    await expect(primary.get(HASH_A)).resolves.toEqual(new Uint8Array([7, 8, 9]));
    expect(result?.data).toEqual(new Uint8Array([7, 8, 9]));
  });
});
