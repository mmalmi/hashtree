import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  BLOB_NO_RESULT,
  MemoryStore,
  blobData,
  sha256,
  type BlobReply,
  type BlobRoute,
  type BlobRouteContext,
  type Hash,
} from '@hashtree/core';
import { BlobRouter } from '../src/index.js';

const DATA = new Uint8Array([1, 2, 3]);
const HASH = await sha256(DATA) as Hash;

function route(
  id: string,
  read: (context?: BlobRouteContext) => Promise<BlobReply>,
): BlobRoute {
  return { id, read: (_request, context) => read(context) };
}

describe('BlobRouter', () => {
  afterEach(() => vi.useRealTimers());

  it('tries explicit preferences first and passes a bounded route context', async () => {
    const calls: string[] = [];
    let received: BlobRouteContext | undefined;
    const router = new BlobRouter([
      route('other', async () => {
        calls.push('other');
        return BLOB_NO_RESULT;
      }),
      route('preferred', async (context) => {
        calls.push('preferred');
        received = context;
        return blobData(DATA);
      }),
    ], { requestTimeoutMs: 1_000, routeAttemptBudget: 3 });

    await expect(router.get(HASH, ['preferred'])).resolves.toEqual(DATA);
    expect(calls).toEqual(['preferred']);
    expect(received?.attemptBudget).toBe(3);
    expect(received?.deadlineMs).toBeGreaterThan(Date.now());
  });

  it('keeps misses and failures route-local while the first valid data wins', async () => {
    const router = new BlobRouter([
      route('miss', async () => BLOB_NO_RESULT),
      route('failed', async () => { throw new Error('unreachable'); }),
      route('corrupt', async () => blobData(new Uint8Array([9]))),
      route('valid', async () => blobData(DATA)),
    ], { hedgeDelayMs: 0, maxInFlight: 2 });

    await expect(router.get(HASH)).resolves.toEqual(DATA);
    const outcomes = router.outcomes();
    expect(outcomes.failed?.failureWeight).toBeGreaterThan(0);
    expect(outcomes.corrupt?.failureWeight).toBeGreaterThan(0);
    expect(outcomes.valid?.successfulWeight).toBeGreaterThan(0);
  });

  it('returns null only when every attempted route explicitly misses', async () => {
    const allMiss = new BlobRouter([
      route('one', async () => BLOB_NO_RESULT),
      route('two', async () => BLOB_NO_RESULT),
    ], { hedgeDelayMs: 0 });
    await expect(allMiss.get(HASH)).resolves.toBeNull();

    const uncertain = new BlobRouter([
      route('miss', async () => BLOB_NO_RESULT),
      route('failed', async () => { throw new Error('offline'); }),
    ], { hedgeDelayMs: 0 });
    await expect(uncertain.get(HASH)).rejects.toThrow(/offline/);
  });

  it('bounds hanging routes, aborts them, and continues to another route', async () => {
    vi.useFakeTimers();
    let aborted = false;
    const router = new BlobRouter([
      route('hanging', (context) => new Promise<BlobReply>(() => {
        context?.signal.addEventListener('abort', () => { aborted = true; }, { once: true });
      })),
      route('valid', async () => blobData(DATA)),
    ], { requestTimeoutMs: 100, hedgeDelayMs: 25, maxInFlight: 2 });

    const pending = router.get(HASH, ['hanging']);
    await vi.advanceTimersByTimeAsync(25);
    await expect(pending).resolves.toEqual(DATA);
    expect(aborted).toBe(true);
  });

  it('honors an already-cancelled parent context without launching a route', async () => {
    const calls: string[] = [];
    const controller = new AbortController();
    controller.abort();
    const router = new BlobRouter([
      route('unused', async () => {
        calls.push('unused');
        return blobData(DATA);
      }),
    ]);

    await expect(router.getDetailed(HASH, {
      context: {
        signal: controller.signal,
        deadlineMs: Date.now() + 1_000,
        attemptBudget: 1,
      },
    })).rejects.toThrow(/cancelled/);
    expect(calls).toEqual([]);
  });

  it('cancels an active search without spending more route budget', async () => {
    const calls: string[] = [];
    const controller = new AbortController();
    let started: (() => void) | undefined;
    const routeStarted = new Promise<void>((resolve) => { started = resolve; });
    const router = new BlobRouter([
      route('hanging', (context) => new Promise<BlobReply>(() => {
        calls.push('hanging');
        context?.signal.addEventListener('abort', () => calls.push('aborted'), { once: true });
        started?.();
      })),
      route('unused', async () => {
        calls.push('unused');
        return blobData(DATA);
      }),
    ], { hedgeDelayMs: 1_000 });

    const pending = router.getDetailed(HASH, {
      context: {
        signal: controller.signal,
        deadlineMs: Date.now() + 2_000,
        attemptBudget: 2,
      },
    });
    await routeStarted;
    controller.abort();

    await expect(pending).rejects.toThrow(/cancelled/);
    expect(calls).toEqual(['hanging', 'aborted']);
    expect(router.outcomes().hanging?.failureWeight).toBe(0);
  });

  it('caches only centrally verified data in the explicitly selected cache', async () => {
    const cache = new MemoryStore();
    const router = new BlobRouter([
      route('source', async () => blobData(DATA)),
    ], { cache });

    await expect(router.get(HASH)).resolves.toEqual(DATA);
    await expect(cache.get(HASH)).resolves.toEqual(DATA);
  });

  it('can itself be used as one opaque composite route', async () => {
    const inner = new BlobRouter([
      route('peer-a', async () => BLOB_NO_RESULT),
      route('peer-b', async () => blobData(DATA)),
    ], { id: 'providers', hedgeDelayMs: 0 });

    await expect(inner.read(
      { hash: HASH, htl: 10 },
      { signal: new AbortController().signal, deadlineMs: Date.now() + 1_000, attemptBudget: 2 },
    )).resolves.toEqual(blobData(DATA));
  });

  it('hard-filters routes independently from preference ordering', async () => {
    const calls: string[] = [];
    const router = new BlobRouter([
      route('local', async () => {
        calls.push('local');
        return blobData(DATA);
      }),
      { ...route('remote-a', async () => {
        calls.push('remote-a');
        return BLOB_NO_RESULT;
      }), groupId: 'remote' },
      { ...route('remote-b', async () => {
        calls.push('remote-b');
        return blobData(DATA);
      }), groupId: 'remote' },
    ], { hedgeDelayMs: 0 });

    await expect(router.getDetailed(HASH, { allowedRouteIds: ['remote'] }))
      .resolves.toEqual({ data: DATA, routeId: 'remote-b' });
    expect(calls).toEqual(['remote-a', 'remote-b']);
  });

  it('adapts toward a lower-latency reliable route', async () => {
    const calls: string[] = [];
    const delayed = (id: string, delayMs: number) => route(id, () => new Promise((resolve) => {
      calls.push(id);
      setTimeout(() => resolve(blobData(DATA)), delayMs);
    }));
    const router = new BlobRouter([
      delayed('slow', 20),
      delayed('fast', 2),
    ], { hedgeDelayMs: 100 });

    await router.get(HASH, ['slow']);
    await router.get(HASH, ['fast']);
    calls.length = 0;
    await router.get(HASH);
    expect(calls).toEqual(['fast']);
  });

  it('bounds route attempts and resets learned cooldown when a route object is replaced', async () => {
    const failed = route('provider', async () => { throw new Error('offline'); });
    const fallback = route('fallback', async () => blobData(DATA));
    const router = new BlobRouter([failed, fallback], {
      hedgeDelayMs: 0,
      maxRouteAttempts: 1,
      initialCooldownMs: 10_000,
    });

    await expect(router.get(HASH, ['provider'])).rejects.toThrow(/attempt budget/);
    expect(router.outcomes().provider?.coolingDown).toBe(true);

    const replacement = route('provider', async () => blobData(DATA));
    router.setRoutes([replacement, fallback]);
    expect(router.outcomes().provider?.coolingDown).toBe(false);
    await expect(router.get(HASH, ['provider'])).resolves.toEqual(DATA);
  });
});
