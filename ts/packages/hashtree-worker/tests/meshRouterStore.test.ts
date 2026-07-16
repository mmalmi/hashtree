import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  BLOB_NO_RESULT,
  blobData,
  MemoryStore,
  sha256,
  type BlobReply,
  type Hash,
} from '@hashtree/core';
import {
  MeshRouterStore,
  type MeshReadSource,
  type MeshRouterGetOptions,
} from '../src/capabilities/meshRouterStore.js';

const DATA_A = new Uint8Array([1, 2, 3]);
const DATA_B = new Uint8Array([4, 5, 6]);
const HASH_A = await sha256(DATA_A) as Hash;
const HASH_B = await sha256(DATA_B) as Hash;

function reply(value: Uint8Array | null): BlobReply {
  return value === null ? BLOB_NO_RESULT : blobData(value);
}

function delayedSource(
  id: string,
  delayMs: number,
  value: Uint8Array | null,
  calls: { count: number },
): MeshReadSource {
  return {
    id,
    read: () => {
      calls.count += 1;
      return new Promise<BlobReply>((resolve) => {
        setTimeout(() => resolve(reply(value)), delayMs);
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
    const slowData = DATA_A;
    const fastData = DATA_A;
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 500,
      sources: [
        delayedSource('fips', 200, slowData, slowCalls),
        delayedSource('blossom', 50, fastData, fastCalls),
      ],
    });

    const pending = router.getDetailed(HASH_A);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

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
    const peerData = (hash: Hash) => hash === HASH_A ? DATA_A : DATA_B;
    const blossomData = peerData;
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 500,
      sources: [
        {
          id: 'fips',
          read: (request) => new Promise((resolve) => {
            peerCalls.count += 1;
            setTimeout(() => resolve(blobData(peerData(request.hash))), 20);
          }),
        },
        {
          id: 'blossom',
          read: (request) => new Promise((resolve) => {
            blossomCalls.count += 1;
            setTimeout(() => resolve(blobData(blossomData(request.hash))), 200);
          }),
        },
      ],
    });

    const first = router.getDetailed(HASH_A);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(100);
    await expect(first).resolves.toEqual({ data: DATA_A, sourceId: 'fips' });

    const second = router.getDetailed(HASH_B);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(100);
    await expect(second).resolves.toEqual({ data: DATA_B, sourceId: 'fips' });
  });

  it('supports remote-only filtered reads without consulting primary storage', async () => {
    const primary = new MemoryStore();
    const localData = DATA_A;
    const remoteData = DATA_A;
    const blossomCalls = { count: 0 };
    await primary.put(HASH_A, localData);

    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      sources: [
        {
          id: 'blossom',
          read: async () => {
            blossomCalls.count += 1;
            return blobData(remoteData);
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
    const remoteData = DATA_A;
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

  it('bounds a single unresolved remote source by requestTimeoutMs', async () => {
    vi.useFakeTimers();
    const primary = new MemoryStore();
    let resolveSource: ((data: Uint8Array | null) => void) | null = null;
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 120,
      sources: [
        {
          id: 'p2p',
          read: () => new Promise<BlobReply>((resolve) => {
            resolveSource = (data) => resolve(reply(data));
          }),
        },
      ],
    });

    const pending = router.getDetailed(HASH_A);
    let settled = false;
    void pending.then(
      () => { settled = true; },
      () => { settled = true; },
    );
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(119);
    expect(settled).toBe(false);

    const rejection = expect(pending).rejects.toThrow(/timed out/i);
    await vi.advanceTimersByTimeAsync(1);
    await rejection;
    expect(router.getSourceStats().p2p?.timeouts).toBe(1);

    resolveSource?.(new Uint8Array([99]));
    await Promise.resolve();
    await expect(primary.get(HASH_A)).resolves.toBeNull();
  });

  it('keeps a slow primary read alive after hedging and prefers its eventual local hit', async () => {
    vi.useFakeTimers();
    const localData = DATA_A;
    const remoteCalls = { count: 0 };
    const primary = {
      put: vi.fn(async () => true),
      get: vi.fn(() => new Promise<Uint8Array | null>((resolve) => {
        setTimeout(() => resolve(localData), 400);
      })),
      has: vi.fn(async () => false),
      delete: vi.fn(async () => false),
    };
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 250,
      requestTimeoutMs: 1_000,
      sources: [
        delayedSource('p2p', 5_000, null, remoteCalls),
      ],
    });

    const pending = router.getDetailed(HASH_A);
    await Promise.resolve();
    expect(remoteCalls.count).toBe(0);

    await vi.advanceTimersByTimeAsync(250);
    expect(remoteCalls.count).toBe(1);

    await vi.advanceTimersByTimeAsync(150);
    await expect(pending).resolves.toEqual({ data: localData, sourceId: 'idb' });
  });

  it('gives the last hedged source a full chance instead of clipping it at query start', async () => {
    vi.useFakeTimers();
    const primary = new MemoryStore();
    const warmCalls = { count: 0 };
    const missCalls = { count: 0 };
    const slowWarmMissCalls = { count: 0 };
    const slowCalls = { count: 0 };
    const warmData = DATA_B;
    const slowData = DATA_A;
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
          read: async (request, signal) => {
            if (request.hash === HASH_B) {
              return delayedSource('warm', 10, warmData, warmCalls).read(request, signal);
            }
            return delayedSource('miss', 200, null, missCalls).read(request, signal);
          },
        },
        {
          id: 'late-hit',
          read: async (request, signal) => {
            if (request.hash === HASH_B) {
              return delayedSource('late-warm-miss', 20, null, slowWarmMissCalls).read(request, signal);
            }
            return delayedSource('late-hit', 100, slowData, slowCalls).read(request, signal);
          },
        },
      ],
    });

    const warm = router.getDetailed(HASH_B, { skipPrimary: true });
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(0);
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
    const data = DATA_A;
    const router = new MeshRouterStore({
      primary,
      requestTimeoutMs: 500,
      sources: [
        {
          id: 'blossom',
          read: () => {
            calls += 1;
            return new Promise<BlobReply>((resolve) => {
              setTimeout(() => resolve(blobData(data)), 50);
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

  it('coalesces concurrent remote reads after primary misses for the same hash', async () => {
    vi.useFakeTimers();
    const primary = new MemoryStore();
    let calls = 0;
    const data = DATA_A;
    const router = new MeshRouterStore({
      primary,
      primarySourceId: 'idb',
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 500,
      sources: [
        {
          id: 'p2p',
          read: () => {
            calls += 1;
            return new Promise<BlobReply>((resolve) => {
              setTimeout(() => resolve(blobData(data)), 50);
            });
          },
        },
      ],
    });

    const first = router.getDetailed(HASH_A);
    const second = router.getDetailed(HASH_A);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(0);

    expect(calls).toBe(1);

    await vi.advanceTimersByTimeAsync(50);
    await expect(first).resolves.toEqual({ data, sourceId: 'p2p' });
    await expect(second).resolves.toEqual({ data, sourceId: 'p2p' });
  });

  it('matches grouped dynamic endpoints when filtering by sourceIds', async () => {
    const primary = new MemoryStore();
    const aCalls = { count: 0 };
    const bCalls = { count: 0 };
    const aData = DATA_A;
    const router = new MeshRouterStore({
      primary,
      sourceProviders: [
        () => [
          {
            id: 'blossom:https://a.example',
            groupId: 'blossom',
            read: async () => {
              aCalls.count += 1;
              return blobData(aData);
            },
          },
          {
            id: 'peer:nostr-peer',
            groupId: 'p2p',
            read: async () => {
              bCalls.count += 1;
              return blobData(new Uint8Array([9]));
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
    const sourceData = DATA_A.slice();
    const router = new MeshRouterStore({
      primary,
      sources: [
        {
          id: 'fips',
          read: async () => blobData(sourceData),
        },
      ],
    });

    const result = await router.getDetailed(HASH_A, { skipPrimary: true });
    expect(result).toEqual({ data: DATA_A, sourceId: 'fips' });
    expect(result?.data).not.toBe(sourceData);

    sourceData[0] = 99;

    await expect(primary.get(HASH_A)).resolves.toEqual(DATA_A);
    expect(result?.data).toEqual(DATA_A);
  });

  it('rejects when a route fails instead of reporting a route-local miss', async () => {
    const data = new Uint8Array([1, 2, 3]);
    const hash = await sha256(data);
    const router = new MeshRouterStore({
      primary: new MemoryStore(),
      primaryReadTimeoutMs: 0,
      sources: [{
        id: 'unreachable-peer',
        read: async () => {
          throw new Error('peer unreachable');
        },
      }],
    });

    await expect(router.getDetailed(hash)).rejects.toThrow('peer unreachable');
    expect(router.getSourceStats()['unreachable-peer']?.failures).toBe(1);
    expect(router.getSourceStats()['unreachable-peer']?.misses).toBe(0);
  });

  it('rejects a bounded route timeout instead of reporting a miss', async () => {
    vi.useFakeTimers();
    const data = new Uint8Array([4, 5, 6]);
    const hash = await sha256(data);
    const router = new MeshRouterStore({
      primary: new MemoryStore(),
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 50,
      sources: [{
        id: 'stalled-peer',
        read: async () => new Promise<BlobReply>(() => {}),
      }],
    });

    const pending = router.getDetailed(hash);
    const rejection = expect(pending).rejects.toThrow(/timed out/i);
    await vi.advanceTimersByTimeAsync(50);

    await rejection;
    expect(router.getSourceStats()['stalled-peer']?.timeouts).toBe(1);
    expect(router.getSourceStats()['stalled-peer']?.misses).toBe(0);
  });

  it('rejects corrupt source bytes without caching them', async () => {
    const expected = new Uint8Array([7, 8, 9]);
    const corrupt = new Uint8Array([9, 8, 7]);
    const hash = await sha256(expected);
    const primary = new MemoryStore();
    const router = new MeshRouterStore({
      primary,
      primaryReadTimeoutMs: 0,
      sources: [{ id: 'corrupt-peer', read: async () => blobData(corrupt) }],
    });

    await expect(router.getDetailed(hash)).rejects.toThrow(/hash|corrupt|integrity/i);
    await expect(primary.get(hash)).resolves.toBeNull();
    expect(router.getSourceStats()['corrupt-peer']?.failures).toBe(1);
    expect(router.getSourceStats()['corrupt-peer']?.successes).toBe(0);
  });

  it('returns the first verified data even when an earlier route is corrupt', async () => {
    vi.useFakeTimers();
    const valid = new Uint8Array([10, 11, 12]);
    const hash = await sha256(valid);
    const router = new MeshRouterStore({
      primary: new MemoryStore(),
      primaryReadTimeoutMs: 0,
      requestTimeoutMs: 500,
      dispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 25,
      },
      sources: [
        { id: 'a-corrupt', read: async () => blobData(new Uint8Array([99])) },
        { id: 'b-valid', read: async () => blobData(valid) },
      ],
    });

    const pending = router.getDetailed(hash);
    await vi.advanceTimersByTimeAsync(25);

    await expect(pending).resolves.toEqual({ data: valid, sourceId: 'b-valid' });
  });

  it('verifies primary bytes and falls through to a valid route', async () => {
    const valid = new Uint8Array([13, 14, 15]);
    const hash = await sha256(valid);
    const primary = {
      put: vi.fn(async () => true),
      get: vi.fn(async () => new Uint8Array([0])),
      has: vi.fn(async () => true),
      delete: vi.fn(async () => false),
    };
    const router = new MeshRouterStore({
      primary,
      primaryReadTimeoutMs: 0,
      sources: [{ id: 'verified-peer', read: async () => blobData(valid) }],
    });

    await expect(router.getDetailed(hash)).resolves.toEqual({ data: valid, sourceId: 'verified-peer' });
  });

  it('passes the bounded HTL unchanged to each route request', async () => {
    const data = new Uint8Array([16, 17, 18]);
    const hash = await sha256(data);
    const read = vi.fn(async () => blobData(data));
    const router = new MeshRouterStore({
      primary: new MemoryStore(),
      primaryReadTimeoutMs: 0,
      sources: [{ id: 'peer', read }],
    });

    await router.getDetailed(hash, { skipPrimary: true, htl: 4 } as MeshRouterGetOptions & { htl: number });

    expect(read).toHaveBeenCalledWith({ hash, htl: 4 }, expect.any(AbortSignal));
  });

  it('keeps primary reads outside remote route HTL validation', async () => {
    const primary = new MemoryStore();
    await primary.put(HASH_A, DATA_A);
    const router = new MeshRouterStore({ primary });

    await expect(router.getDetailed(HASH_A, { htl: 255 })).resolves.toEqual({
      data: DATA_A,
      sourceId: 'primary',
    });
  });

  it('returns null only after every selected route explicitly misses', async () => {
    vi.useFakeTimers();
    const router = new MeshRouterStore({
      primary: new MemoryStore(),
      requestTimeoutMs: 100,
      dispatch: { initialFanout: 2, hedgeFanout: 1, maxFanout: 2, hedgeIntervalMs: 10 },
      sources: [
        { id: 'fast-miss', read: async () => BLOB_NO_RESULT },
        {
          id: 'slow-miss',
          read: async () => new Promise<BlobReply>((resolve) => {
            setTimeout(() => resolve(BLOB_NO_RESULT), 30);
          }),
        },
      ],
    });

    const pending = router.getDetailed(HASH_A, { skipPrimary: true });
    let settled = false;
    void pending.then(() => { settled = true; });
    await vi.advanceTimersByTimeAsync(29);
    expect(settled).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    await expect(pending).resolves.toBeNull();
  });

  it('returns the first verified data without waiting for another route', async () => {
    vi.useFakeTimers();
    const router = new MeshRouterStore({
      primary: new MemoryStore(),
      requestTimeoutMs: 500,
      dispatch: { initialFanout: 2, hedgeFanout: 1, maxFanout: 2, hedgeIntervalMs: 10 },
      sources: [
        {
          id: 'fast-data',
          read: async () => new Promise<BlobReply>((resolve) => {
            setTimeout(() => resolve(blobData(DATA_A)), 20);
          }),
        },
        { id: 'stalled-route', read: async () => new Promise<BlobReply>(() => {}) },
      ],
    });

    const pending = router.getDetailed(HASH_A, { skipPrimary: true });
    await vi.advanceTimersByTimeAsync(20);
    await expect(pending).resolves.toEqual({ data: DATA_A, sourceId: 'fast-data' });
    expect(router.getSourceStats()['stalled-route']?.timeouts).toBe(0);
  });
});
