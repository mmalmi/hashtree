import { describe, expect, it } from 'vitest';
import {
  createReplaceablePublishQueue,
  replaceableEventCoordinateFromTemplate,
  replaceableEventCoordinateKey,
} from '../src/replaceablePublish.js';

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((innerResolve, innerReject) => {
    resolve = innerResolve;
    reject = innerReject;
  });
  return { promise, resolve, reject };
}

describe('replaceable publish queue', () => {
  it('derives a coordinate key from parameterized replaceable event templates', () => {
    const coordinate = replaceableEventCoordinateFromTemplate('pubkey', {
      kind: 30078,
      tags: [['d', 'catalog']],
    });

    expect(coordinate).toEqual({
      pubkey: 'pubkey',
      kind: 30078,
      dTag: 'catalog',
    });
    expect(replaceableEventCoordinateKey(coordinate)).toBe('pubkey:0000757e:catalog');
  });

  it('waits until the next second before publishing another update for the same coordinate', async () => {
    let nowMs = 100;
    const sleeps: number[] = [];
    const published: Array<{ label: string; createdAt: number }> = [];
    const queue = createReplaceablePublishQueue({
      nowMs: () => nowMs,
      sleep: async (ms) => {
        sleeps.push(ms);
        nowMs += ms;
      },
    });

    const coordinate = 'pubkey:0000757e:catalog';
    const first = await queue.publish({
      coordinate,
      publish: async (createdAt) => {
        published.push({ label: 'first', createdAt });
        return 'first-ok';
      },
    });

    const secondPromise = queue.publish({
      coordinate,
      publish: async (createdAt) => {
        published.push({ label: 'second', createdAt });
        return 'second-ok';
      },
    });

    const second = await secondPromise;

    expect(first).toEqual({ status: 'published', createdAt: 0, result: 'first-ok' });
    expect(second).toEqual({ status: 'published', createdAt: 1, result: 'second-ok' });
    expect(sleeps).toEqual([900]);
    expect(published).toEqual([
      { label: 'first', createdAt: 0 },
      { label: 'second', createdAt: 1 },
    ]);
  });

  it('coalesces queued same-coordinate updates so only the last pending publish runs', async () => {
    let nowMs = 250;
    const sleeps: number[] = [];
    const published: Array<{ label: string; createdAt: number }> = [];
    const firstGate = deferred<void>();
    const queue = createReplaceablePublishQueue({
      nowMs: () => nowMs,
      sleep: async (ms) => {
        sleeps.push(ms);
        nowMs += ms;
      },
    });

    const coordinate = 'pubkey:0000757e:catalog';
    const firstPromise = queue.publish({
      coordinate,
      publish: async (createdAt) => {
        published.push({ label: 'first', createdAt });
        await firstGate.promise;
        return 'first-ok';
      },
    });
    await Promise.resolve();

    const secondPromise = queue.publish({
      coordinate,
      publish: async (createdAt) => {
        published.push({ label: 'second', createdAt });
        return 'second-ok';
      },
    });
    const thirdPromise = queue.publish({
      coordinate,
      publish: async (createdAt) => {
        published.push({ label: 'third', createdAt });
        return 'third-ok';
      },
    });

    firstGate.resolve();

    expect(await secondPromise).toEqual({ status: 'superseded' });
    expect(await firstPromise).toEqual({ status: 'published', createdAt: 0, result: 'first-ok' });
    expect(await thirdPromise).toEqual({ status: 'published', createdAt: 1, result: 'third-ok' });
    expect(sleeps).toEqual([750]);
    expect(published).toEqual([
      { label: 'first', createdAt: 0 },
      { label: 'third', createdAt: 1 },
    ]);
  });

  it('publishes different coordinates independently', async () => {
    let nowMs = 500;
    const published: Array<{ label: string; createdAt: number }> = [];
    const queue = createReplaceablePublishQueue({
      nowMs: () => nowMs,
      sleep: async (ms) => {
        nowMs += ms;
      },
    });

    const left = await queue.publish({
      coordinate: 'pubkey:0000757e:left',
      publish: async (createdAt) => {
        published.push({ label: 'left', createdAt });
        return 'left-ok';
      },
    });
    const right = await queue.publish({
      coordinate: 'pubkey:0000757e:right',
      publish: async (createdAt) => {
        published.push({ label: 'right', createdAt });
        return 'right-ok';
      },
    });

    expect(left).toEqual({ status: 'published', createdAt: 0, result: 'left-ok' });
    expect(right).toEqual({ status: 'published', createdAt: 0, result: 'right-ok' });
    expect(published).toEqual([
      { label: 'left', createdAt: 0 },
      { label: 'right', createdAt: 0 },
    ]);
  });
});
