import { afterEach, describe, expect, it, vi } from 'vitest';
import { BlossomStore, type BlossomSigner } from '../src/store/blossom.js';
import { sha256 } from '../src/hash.js';
import { toHex } from '../src/types.js';
import type { Hash } from '../src/types.js';

const DATA = new Uint8Array([1, 2, 3, 4, 5]);

async function makeHash(): Promise<Hash> {
  return await sha256(DATA) as Hash;
}

async function makeHashFor(data: Uint8Array): Promise<Hash> {
  return await sha256(data) as Hash;
}

function makeResponse(status: number, body?: Uint8Array, jsonBody?: unknown): Response {
  const textBody = jsonBody === undefined ? '' : JSON.stringify(jsonBody);
  return {
    ok: status >= 200 && status < 300,
    status,
    arrayBuffer: async () => (body ?? new Uint8Array()).buffer,
    text: async () => textBody,
    json: async () => {
      if (jsonBody === undefined) {
        throw new SyntaxError('Unexpected end of JSON input');
      }
      return jsonBody;
    },
  } as Response;
}

function makeBatchResponse(hash: Hash, data: Uint8Array): Response {
  const body = new Uint8Array(8 + 4 + 32 + 8 + data.length);
  body.set(new Uint8Array([72, 84, 66, 68, 86, 49, 0, 0]), 0); // HTBDV1\0\0
  const view = new DataView(body.buffer);
  view.setUint32(8, 1, false);
  body.set(hash, 12);
  view.setBigUint64(44, BigInt(data.length), false);
  body.set(data, 52);
  return makeResponse(200, body);
}

const signer: BlossomSigner = async () => ({
  kind: 24242,
  created_at: 1,
  content: '',
  tags: [],
  pubkey: '0'.repeat(64),
  id: '1'.repeat(64),
  sig: '2'.repeat(128),
});

describe('BlossomStore', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('prefers a single best read server instead of blasting every server', async () => {
    vi.useFakeTimers();
    const hash = await makeHash();

    const fetchMock = vi.fn((input: string | URL | RequestInfo) => {
      const url = String(input);
      if (url.startsWith('https://fast.example/')) {
        return new Promise<Response>((resolve) => {
          setTimeout(() => resolve(makeResponse(200, DATA)), 20);
        });
      }
      if (url.startsWith('https://slow.example/')) {
        return new Promise<Response>((resolve) => {
          setTimeout(() => resolve(makeResponse(200, DATA)), 200);
        });
      }
      return Promise.resolve(makeResponse(404));
    });

    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [
        { url: 'https://slow.example', read: true },
        { url: 'https://fast.example', read: true },
      ],
    });

    const readPromise = store.get(hash);
    await vi.advanceTimersByTimeAsync(20);

    await expect(readPromise).resolves.toEqual(DATA);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(String(fetchMock.mock.calls[0]?.[0])).toContain('https://fast.example/');
  });

  it('recovers from a cached raw 404 with one cache-reload request', async () => {
    const hash = await makeHash();
    const rawUrl = `https://cdn.example/${toHex(hash)}.bin`;
    const fetchMock = vi.fn((input: string | URL | RequestInfo, init?: RequestInit) => {
      expect(String(input)).toBe(rawUrl);
      if (init?.cache === 'reload') {
        return Promise.resolve(makeResponse(200, DATA));
      }
      expect(init?.cache).toBeUndefined();
      return Promise.resolve(makeResponse(404));
    });
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://cdn.example', read: true }],
    });

    await expect(store.get(hash)).resolves.toEqual(DATA);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(fetchMock.mock.calls.map(([url, init]) => ({
      url: String(url),
      cache: init?.cache,
    }))).toEqual([
      { url: rawUrl, cache: undefined },
      { url: rawUrl, cache: 'reload' },
    ]);
  });

  it('does not cache-reload a successful raw response', async () => {
    const hash = await makeHash();
    const rawUrl = `https://cdn.example/${toHex(hash)}.bin`;
    const fetchMock = vi.fn(() => Promise.resolve(makeResponse(200, DATA)));
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://cdn.example', read: true }],
    });

    await expect(store.get(hash)).resolves.toEqual(DATA);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledWith(
      rawUrl,
      expect.not.objectContaining({ cache: 'reload' }),
    );
  });

  it('cache-reloads a raw 404 only once when the blob is genuinely missing', async () => {
    const hash = await makeHash();
    const rawUrl = `https://cdn.example/${toHex(hash)}.bin`;
    const fetchMock = vi.fn(() => Promise.resolve(makeResponse(404)));
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://cdn.example', read: true }],
    });

    await expect(store.get(hash)).resolves.toBeNull();
    const rawCalls = fetchMock.mock.calls.filter(([url]) => String(url) === rawUrl);
    expect(rawCalls).toHaveLength(2);
    expect(rawCalls.map(([, init]) => init?.cache)).toEqual([undefined, 'reload']);
  });

  it('falls through immediately when the preferred server returns 404', async () => {
    vi.useFakeTimers();
    const hash = await makeHash();

    const fetchMock = vi.fn((input: string | URL | RequestInfo) => {
      const url = String(input);
      if (url.startsWith('https://aaa-missing.example/')) {
        return Promise.resolve(makeResponse(404));
      }
      if (url.startsWith('https://zzz-later.example/')) {
        return new Promise<Response>((resolve) => {
          setTimeout(() => resolve(makeResponse(200, DATA)), 10);
        });
      }
      return Promise.resolve(makeResponse(404));
    });

    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [
        { url: 'https://aaa-missing.example', read: true },
        { url: 'https://zzz-later.example', read: true },
      ],
    });

    const readPromise = store.get(hash);
    await vi.advanceTimersByTimeAsync(10);

    await expect(readPromise).resolves.toEqual(DATA);
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toEqual([
      expect.stringContaining('https://aaa-missing.example/'),
      expect.stringContaining('https://aaa-missing.example/'),
      'https://aaa-missing.example/blob/batch',
      expect.stringContaining('https://zzz-later.example/'),
    ]);
    expect(fetchMock.mock.calls[0]?.[1]?.cache).toBeUndefined();
    expect(fetchMock.mock.calls[1]?.[1]?.cache).toBe('reload');
  });

  it('falls back to blob batch download when hash URLs are blocked', async () => {
    const hash = await makeHash();
    const hashHex = toHex(hash);

    const fetchMock = vi.fn((input: string | URL | RequestInfo, init?: RequestInit) => {
      const url = String(input);
      if (url === `https://cdn.example/${hashHex}.bin`) {
        return Promise.reject(new TypeError('Blocked by client'));
      }
      if (url === 'https://cdn.example/blob/batch') {
        expect(init?.method).toBe('POST');
        expect(init?.body).toBe(JSON.stringify({ hashes: [hashHex] }));
        return Promise.resolve(makeBatchResponse(hash, DATA));
      }
      return Promise.resolve(makeResponse(404));
    });

    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://cdn.example', read: true }],
    });

    await expect(store.get(hash)).resolves.toEqual(DATA);
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toEqual([
      `https://cdn.example/${hashHex}.bin`,
      'https://cdn.example/blob/batch',
    ]);
  });

  it('can prefer blob batch downloads before raw hash URLs', async () => {
    const hash = await makeHash();
    const hashHex = toHex(hash);

    const fetchMock = vi.fn((input: string | URL | RequestInfo, init?: RequestInit) => {
      const url = String(input);
      if (url === 'https://cdn.example/blob/batch') {
        expect(init?.method).toBe('POST');
        expect(init?.body).toBe(JSON.stringify({ hashes: [hashHex] }));
        return Promise.resolve(makeBatchResponse(hash, DATA));
      }
      return Promise.resolve(makeResponse(404));
    });

    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://cdn.example', read: true, preferBatchReads: true }],
    });

    await expect(store.get(hash)).resolves.toEqual(DATA);
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toEqual([
      'https://cdn.example/blob/batch',
    ]);
  });

  it('hedges to a fallback server when the preferred server stalls', async () => {
    vi.useFakeTimers();
    const firstData = DATA;
    const secondData = new Uint8Array([9, 8, 7, 6, 5]);
    const firstHash = await sha256(firstData) as Hash;
    const secondHash = await sha256(secondData) as Hash;
    const firstHashHex = toHex(firstHash);
    const secondHashHex = toHex(secondHash);

    const fetchMock = vi.fn((input: string | URL | RequestInfo) => {
      const url = String(input);
      if (url === `https://fast.example/${firstHashHex}.bin`) {
        return new Promise<Response>((resolve) => {
          setTimeout(() => resolve(makeResponse(200, firstData)), 10);
        });
      }
      if (url === `https://fast.example/${secondHashHex}.bin`) {
        return new Promise<Response>(() => {});
      }
      if (url === `https://slow.example/${secondHashHex}.bin`) {
        return new Promise<Response>((resolve) => {
          setTimeout(() => resolve(makeResponse(200, secondData)), 10);
        });
      }
      return Promise.resolve(makeResponse(404));
    });

    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [
        { url: 'https://slow.example', read: true },
        { url: 'https://fast.example', read: true },
      ],
    });

    const warmRead = store.get(firstHash);
    await vi.advanceTimersByTimeAsync(10);
    await expect(warmRead).resolves.toEqual(firstData);

    const hedgedRead = store.get(secondHash);
    await vi.advanceTimersByTimeAsync(74);
    expect(fetchMock).toHaveBeenCalledTimes(2);

    await vi.advanceTimersByTimeAsync(1);
    expect(fetchMock).toHaveBeenCalledTimes(3);

    await vi.advanceTimersByTimeAsync(10);
    await expect(hedgedRead).resolves.toEqual(secondData);
  });

  it('returns a miss only when every attempted read server explicitly misses', async () => {
    const hash = await makeHash();
    vi.stubGlobal('fetch', vi.fn(() => Promise.resolve(makeResponse(404))));
    const store = new BlossomStore({
      servers: [
        { url: 'https://one.example', read: true },
        { url: 'https://two.example', read: true },
      ],
    });

    await expect(store.get(hash)).resolves.toBeNull();
  });

  it('preserves network failure as uncertainty when no server returns data', async () => {
    const hash = await makeHash();
    vi.stubGlobal('fetch', vi.fn((input: string | URL | RequestInfo) => (
      String(input).startsWith('https://offline.example/')
        ? Promise.reject(new Error('network offline'))
        : Promise.resolve(makeResponse(404))
    )));
    const store = new BlossomStore({
      servers: [
        { url: 'https://offline.example', read: true },
        { url: 'https://missing.example', read: true },
      ],
    });

    await expect(store.get(hash)).rejects.toThrow(/network offline|uncertain/i);
  });

  it('preserves corrupt server data as an error rather than a miss', async () => {
    const hash = await makeHash();
    vi.stubGlobal('fetch', vi.fn(() => Promise.resolve(makeResponse(
      200,
      new Uint8Array([9, 9, 9]),
    ))));
    const store = new BlossomStore({
      servers: [{ url: 'https://corrupt.example', read: true }],
    });

    await expect(store.get(hash)).rejects.toThrow(/hash mismatch|corrupt/i);
  });

  it('preserves a malformed successful batch response as an error', async () => {
    const hash = await makeHash();
    vi.stubGlobal('fetch', vi.fn((input: string | URL | RequestInfo) => (
      String(input).endsWith('/blob/batch')
        ? Promise.resolve(makeResponse(200, new Uint8Array([1, 2, 3])))
        : Promise.resolve(makeResponse(404))
    )));
    const store = new BlossomStore({
      servers: [{ url: 'https://malformed.example', read: true, preferBatchReads: true }],
    });

    await expect(store.get(hash)).rejects.toThrow(/batch|malformed|short/i);
  });

  it('aborts stalled upload requests', async () => {
    vi.useFakeTimers();
    const hash = await makeHash();
    const uploadEvents: string[] = [];

    const fetchMock = vi.fn((_input: string | URL | RequestInfo, init?: RequestInit) => (
      new Promise<Response>((_resolve, reject) => {
        const signal = init?.signal;
        if (signal instanceof AbortSignal) {
          signal.addEventListener('abort', () => reject(signal.reason), { once: true });
        }
      })
    ));

    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://write.example', write: true }],
      signer,
      putTimeoutMs: 5,
      onUploadProgress: (_serverUrl, status) => uploadEvents.push(status),
    });

    const putPromise = store.put(hash, DATA);
    await vi.advanceTimersByTimeAsync(5);

    await expect(putPromise).rejects.toThrow(/Blossom upload failed/);
    expect(fetchMock).toHaveBeenCalledWith(
      'https://write.example/upload',
      expect.objectContaining({
        method: 'PUT',
        signal: expect.any(AbortSignal),
      }),
    );
    expect(uploadEvents).toEqual(['failed']);
  });

  it('waits for write server backoff instead of failing the next upload immediately', async () => {
    vi.useFakeTimers();
    const firstHash = await makeHash();
    const secondData = new Uint8Array([6, 7, 8, 9]);
    const secondHash = await makeHashFor(secondData);
    const secondHashHex = toHex(secondHash);

    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(makeResponse(503, undefined, { error: 'busy' }))
      .mockResolvedValueOnce(makeResponse(201, undefined, { sha256: secondHashHex }));
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://write.example', write: true }],
      signer,
    });

    await expect(store.put(firstHash, DATA)).rejects.toThrow(/Blossom upload failed/);

    const secondPut = store.put(secondHash, secondData);
    await vi.advanceTimersByTimeAsync(999);
    expect(fetchMock).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(1);
    await expect(secondPut).resolves.toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it('bounds concurrent writes without serializing every upload behind one request', async () => {
    const hash = await makeHash();
    const hashHex = toHex(hash);
    const pending: Array<(response: Response) => void> = [];
    const fetchMock = vi.fn(() => new Promise<Response>((resolve) => pending.push(resolve)));
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://write.example', write: true }],
      signer,
      maxConcurrentWrites: 2,
    });

    const first = store.put(hash, DATA);
    const second = store.put(hash, DATA);
    const third = store.put(hash, DATA);

    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(pending).toHaveLength(2);

    pending.shift()!(makeResponse(201, undefined, { sha256: hashHex }));
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(3));
    pending.shift()!(makeResponse(201, undefined, { sha256: hashHex }));
    pending.shift()!(makeResponse(201, undefined, { sha256: hashHex }));

    await expect(Promise.all([first, second, third])).resolves.toEqual([true, true, true]);
  });

  it('treats 201 upload responses as newly stored', async () => {
    const hash = await makeHash();
    const hashHex = toHex(hash);
    const uploadEvents: string[] = [];

    const fetchMock = vi.fn(() => Promise.resolve(makeResponse(201, undefined, { sha256: hashHex })));
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://write.example', write: true }],
      signer,
      onUploadProgress: (_serverUrl, status) => uploadEvents.push(status),
    });

    await expect(store.put(hash, DATA)).resolves.toBe(true);
    expect(fetchMock).toHaveBeenCalledWith('https://write.example/upload', expect.objectContaining({ method: 'PUT' }));
    expect(uploadEvents).toEqual(['uploaded']);
  });

  it('treats BUD-02 200 upload responses as already stored', async () => {
    const hash = await makeHash();
    const hashHex = toHex(hash);
    const uploadEvents: string[] = [];

    const fetchMock = vi.fn(() => Promise.resolve(makeResponse(200, undefined, { sha256: hashHex })));
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://write.example', write: true }],
      signer,
      onUploadProgress: (_serverUrl, status) => uploadEvents.push(status),
    });

    await expect(store.put(hash, DATA)).resolves.toBe(false);
    expect(uploadEvents).toEqual(['skipped']);
  });

  it('accepts legacy 409 upload responses as already stored', async () => {
    const hash = await makeHash();
    const uploadEvents: string[] = [];

    const fetchMock = vi.fn(() => Promise.resolve(makeResponse(409)));
    vi.stubGlobal('fetch', fetchMock);

    const store = new BlossomStore({
      servers: [{ url: 'https://write.example', write: true }],
      signer,
      onUploadProgress: (_serverUrl, status) => uploadEvents.push(status),
    });

    await expect(store.put(hash, DATA)).resolves.toBe(false);
    expect(uploadEvents).toEqual(['skipped']);
  });
});
