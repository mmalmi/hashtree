import { afterEach, describe, expect, it, vi } from 'vitest';
import { BlossomTransport } from '../src/capabilities/blossomTransport.js';
import { sha256, toHex } from '@hashtree/core';

describe('BlossomTransport.fetch', () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('starts with the most promising read server instead of querying every server', async () => {
    const data = new TextEncoder().encode('parallel-blossom-thumb');
    const hashHex = toHex(await sha256(data));
    const fastBase = 'https://fast.example';
    const fetchMock = vi.fn((input: string | URL) => {
      const url = String(input);
      if (url === `${fastBase}/${hashHex}.bin`) {
        return Promise.resolve({
          ok: true,
          arrayBuffer: async () => data.buffer.slice(0),
        });
      }
      return Promise.resolve({
        ok: false,
        arrayBuffer: async () => new ArrayBuffer(0),
      });
    });
    vi.stubGlobal('fetch', fetchMock);

    const transport = new BlossomTransport([
      { url: fastBase, read: true, write: false },
      { url: 'https://slow.example', read: true, write: false },
    ]);

    const resultPromise = transport.fetch(hashHex);
    await Promise.resolve();
    await Promise.resolve();

    const requestedUrls = fetchMock.mock.calls.map(([url]) => String(url));
    expect(requestedUrls).toEqual([`${fastBase}/${hashHex}.bin`]);

    await expect(resultPromise).resolves.toEqual(data);
  });

  it('deduplicates concurrent fetches for the same hash', async () => {
    const data = new TextEncoder().encode('dedupe-blossom-thumb');
    const hashHex = toHex(await sha256(data));
    const base = 'https://fast.example';

    const fetchMock = vi.fn((input: string | URL) => {
      const url = String(input);
      if (url !== `${base}/${hashHex}.bin`) {
        return Promise.resolve({
          ok: false,
          arrayBuffer: async () => new ArrayBuffer(0),
        });
      }
      return Promise.resolve({
        ok: true,
        arrayBuffer: async () => data.buffer.slice(0),
      });
    });
    vi.stubGlobal('fetch', fetchMock);

    const transport = new BlossomTransport([
      { url: base, read: true, write: false },
    ], undefined, 60_000);

    const first = transport.fetch(hashHex);
    const second = transport.fetch(hashHex);

    await expect(Promise.all([first, second])).resolves.toEqual([data, data]);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('reads blob size with HEAD without downloading the body', async () => {
    const hashHex = 'b'.repeat(64);
    const base = 'https://fast.example';
    const fetchMock = vi.fn((_input: string | URL, init?: RequestInit) => Promise.resolve({
      ok: true,
      headers: new Headers({ 'content-length': '2097168' }),
      arrayBuffer: async () => {
        throw new Error('body should not be read');
      },
      method: init?.method,
    }));
    vi.stubGlobal('fetch', fetchMock);
    const transport = new BlossomTransport([{ url: base, read: true, write: false }]);

    await expect(transport.stat(hashHex)).resolves.toEqual({ size: 2_097_168 });
    expect(fetchMock).toHaveBeenCalledWith(`${base}/${hashHex}.bin`, expect.objectContaining({
      method: 'HEAD',
    }));
  });

  it('clears timed-out inflight reads so later retries can refetch', async () => {
    vi.useFakeTimers();
    const hashHex = 'a'.repeat(64);
    const base = 'https://slow.example';
    const fetchMock = vi.fn(() => new Promise(() => {}));
    vi.stubGlobal('fetch', fetchMock);

    const transport = new BlossomTransport([
      { url: base, read: true, write: false },
    ], undefined, 25);

    const first = transport.fetch(hashHex);
    const firstRejection = expect(first).rejects.toThrow(/timed out/i);
    await Promise.resolve();
    expect(fetchMock).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(25);
    await firstRejection;

    const second = transport.fetch(hashHex);
    const secondRejection = expect(second).rejects.toThrow(/timed out/i);
    await Promise.resolve();
    expect(fetchMock).toHaveBeenCalledTimes(2);

    await vi.advanceTimersByTimeAsync(25);
    await secondRejection;
  });

  it('limits concurrent read fetches across hashes', async () => {
    const base = 'https://throttled.example';
    const concurrencyLimit = 32;
    const payloads = Array.from({ length: concurrencyLimit + 8 }, (_, index) =>
      new TextEncoder().encode(`concurrent fetch ${index}`));
    const hashes = await Promise.all(payloads.map(async (data) => toHex(await sha256(data))));
    const payloadByHash = new Map(hashes.map((hash, index) => [hash, payloads[index]]));
    const releases: Array<() => void> = [];
    let active = 0;
    let maxActive = 0;

    const fetchMock = vi.fn((input: string | URL) => {
      const hash = String(input).split('/').pop()?.replace(/\.bin$/, '');
      const data = hash ? payloadByHash.get(hash) : undefined;
      active += 1;
      maxActive = Math.max(maxActive, active);
      return new Promise((resolve) => {
        releases.push(() => {
          active -= 1;
          resolve({
            ok: true,
            status: 200,
            arrayBuffer: async () => data!.buffer.slice(0),
          });
        });
      });
    });
    vi.stubGlobal('fetch', fetchMock);

    const transport = new BlossomTransport([
      { url: base, read: true, write: false },
    ]);

    const requests = hashes.map((hashHex) => transport.fetch(hashHex));

    await Promise.resolve();
    await Promise.resolve();

    expect(fetchMock).toHaveBeenCalledTimes(concurrencyLimit);
    expect(maxActive).toBe(concurrencyLimit);

    const firstBatch = releases.splice(0, releases.length);
    expect(firstBatch).toHaveLength(concurrencyLimit);
    for (const release of firstBatch) {
      release();
    }

    await vi.waitFor(() => {
      expect(fetchMock).toHaveBeenCalledTimes(hashes.length);
    });
    expect(active).toBe(hashes.length - concurrencyLimit);

    const secondBatch = releases.splice(0, releases.length);
    expect(secondBatch).toHaveLength(hashes.length - concurrencyLimit);
    for (const release of secondBatch) {
      release();
    }

    await expect(Promise.all(requests)).resolves.toEqual(payloads);
    expect(maxActive).toBeLessThanOrEqual(concurrencyLimit);
  }, 10_000);

  it('reuses BlossomStore backoff so failed servers are skipped on immediate retries', async () => {
    const data = new TextEncoder().encode('backoff-blossom-thumb');
    const hashHex = toHex(await sha256(data));
    const slowBase = 'https://slow.example';
    const fastBase = 'https://fast.example';
    let fastCalls = 0;

    const fetchMock = vi.fn((input: string | URL) => {
      const url = String(input);
      if (url === `${slowBase}/${hashHex}.bin`) {
        return Promise.reject(new Error('slow server offline'));
      }
      if (url === `${fastBase}/${hashHex}.bin`) {
        fastCalls += 1;
        if (fastCalls === 1) {
          return Promise.resolve({
            ok: false,
            status: 404,
            arrayBuffer: async () => new ArrayBuffer(0),
          });
        }
        return Promise.resolve({
          ok: true,
          status: 200,
          arrayBuffer: async () => data.buffer.slice(0),
        });
      }
      return Promise.resolve({
        ok: false,
        status: 404,
        arrayBuffer: async () => new ArrayBuffer(0),
      });
    });
    vi.stubGlobal('fetch', fetchMock);

    const transport = new BlossomTransport([
      { url: slowBase, read: true, write: false },
      { url: fastBase, read: true, write: false },
    ]);

    await expect(transport.fetch(hashHex)).rejects.toThrow(/slow server offline|uncertain/i);
    await expect(transport.fetch(hashHex)).resolves.toEqual(data);

    const requestedUrls = fetchMock.mock.calls.map(([url]) => String(url));
    expect(requestedUrls.filter((url) => url === `${slowBase}/${hashHex}.bin`)).toHaveLength(1);
    expect(requestedUrls.filter((url) => url === `${fastBase}/${hashHex}.bin`)).toHaveLength(2);
  });
});
