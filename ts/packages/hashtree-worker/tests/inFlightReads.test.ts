import { describe, expect, it, vi } from 'vitest';
import { InFlightReads, copyBlobBytes } from '../src/inFlightReads.js';

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

describe('InFlightReads', () => {
  it('runs one load for concurrent callers and hands each its own bytes', async () => {
    const reads = new InFlightReads(copyBlobBytes);
    const gate = deferred<Uint8Array | null>();
    const load = vi.fn(() => gate.promise);

    const callers = [reads.read('a', load), reads.read('a', load), reads.read('a', load)];
    expect(load).toHaveBeenCalledTimes(1);
    expect(reads.size).toBe(1);

    const source = Uint8Array.from([1, 2, 3]);
    gate.resolve(source);
    const results = await Promise.all(callers);

    expect(results[0]).toBe(source);
    expect(new Set(results.map((bytes) => bytes!.buffer)).size).toBe(3);
    for (const bytes of results) expect(bytes).toEqual(source);
    expect(reads.size).toBe(0);
  });

  it('keeps joiners intact when the first caller transfers its buffer on resume', async () => {
    const reads = new InFlightReads(copyBlobBytes);
    const gate = deferred<Uint8Array | null>();

    const first = reads.read('a', () => gate.promise).then((bytes) => {
      structuredClone(bytes, { transfer: [bytes!.buffer] });
      return bytes!.byteLength;
    });
    const joiner = reads.read('a', () => gate.promise);
    gate.resolve(Uint8Array.from([9, 8, 7, 6]));

    expect(await first).toBe(0);
    expect(await joiner).toEqual(Uint8Array.from([9, 8, 7, 6]));
  });

  it('keeps keys independent', async () => {
    const reads = new InFlightReads(copyBlobBytes);
    const load = vi.fn(async (value: number) => Uint8Array.from([value]));

    const [a, b] = await Promise.all([reads.read('a', () => load(1)), reads.read('b', () => load(2))]);

    expect(load).toHaveBeenCalledTimes(2);
    expect(a).toEqual(Uint8Array.from([1]));
    expect(b).toEqual(Uint8Array.from([2]));
  });

  it('shares a miss as a miss and an error as an error', async () => {
    const reads = new InFlightReads(copyBlobBytes);
    const miss = deferred<Uint8Array | null>();
    const missing = [reads.read('a', () => miss.promise), reads.read('a', () => miss.promise)];
    miss.resolve(null);
    expect(await Promise.all(missing)).toEqual([null, null]);

    const failure = deferred<Uint8Array | null>();
    const failing = [reads.read('a', () => failure.promise), reads.read('a', () => failure.promise)];
    failure.reject(new Error('timed out'));
    const settled = await Promise.allSettled(failing);
    expect(settled.map((result) => result.status)).toEqual(['rejected', 'rejected']);
    expect((settled[1] as PromiseRejectedResult).reason).toEqual(new Error('timed out'));
  });

  it('starts a fresh load once the previous one has settled', async () => {
    const reads = new InFlightReads(copyBlobBytes);
    const load = vi.fn()
      .mockRejectedValueOnce(new Error('timed out'))
      .mockResolvedValueOnce(Uint8Array.from([5]));

    await expect(reads.read('a', load)).rejects.toThrow('timed out');
    expect(await reads.read('a', load)).toEqual(Uint8Array.from([5]));
    expect(load).toHaveBeenCalledTimes(2);
  });

  it('reports a synchronously throwing load to every caller', async () => {
    const reads = new InFlightReads(copyBlobBytes);
    const load = (): Promise<Uint8Array | null> => { throw new Error('not ready'); };

    const settled = await Promise.allSettled([reads.read('a', load), reads.read('a', load)]);

    expect(settled.map((result) => result.status)).toEqual(['rejected', 'rejected']);
    expect(reads.size).toBe(0);
  });
});
