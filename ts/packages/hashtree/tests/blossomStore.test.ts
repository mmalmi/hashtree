import { afterEach, describe, expect, it, vi } from 'vitest';
import { BlossomStore, type BlossomSigner } from '../src/store/blossom.js';
import { sha256 } from '../src/hash.js';
import { toHex } from '../src/types.js';
import type { Hash } from '../src/types.js';

const DATA = new Uint8Array([1, 2, 3, 4, 5]);

async function makeHash(): Promise<Hash> {
  return await sha256(DATA) as Hash;
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
      'https://aaa-missing.example/blob/batch',
      expect.stringContaining('https://zzz-later.example/'),
    ]);
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
