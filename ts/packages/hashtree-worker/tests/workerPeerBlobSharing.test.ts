import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const postMessageMock = vi.hoisted(() => vi.fn());
const idbDataByHash = vi.hoisted(() => new Map<string, Uint8Array>());
const blossomDataByHash = vi.hoisted(() => new Map<string, Uint8Array>());
const peerFetchResponder = vi.hoisted(() => ({
  handle: null as null | ((ctx: FakeWorkerGlobal, requestId: string, hashHex: string) => void),
}));

class FakeWorkerGlobal {
  private listener: EventListener | null = null;

  postMessage(message: unknown): void {
    postMessageMock(message);
    const candidate = message as { type?: string; requestId?: string };
    if (candidate?.type === 'p2pFetch' && typeof candidate.requestId === 'string') {
      const request = message as { hashHex?: string };
      if (peerFetchResponder.handle) {
        peerFetchResponder.handle(this, candidate.requestId, `${request.hashHex ?? ''}`);
        return;
      }
      queueMicrotask(() => {
        this.dispatch({
          type: 'p2pFetchResult',
          id: `peer-miss-${candidate.requestId}`,
          requestId: candidate.requestId,
        });
      });
    }
  }

  addEventListener(_type: 'message', listener: EventListenerOrEventListenerObject): void {
    if (typeof listener === 'function') {
      this.listener = listener;
      return;
    }
    this.listener = listener.handleEvent.bind(listener) as EventListener;
  }

  removeEventListener(_type: 'message', listener: EventListenerOrEventListenerObject): void {
    const normalized = typeof listener === 'function'
      ? listener
      : listener.handleEvent.bind(listener);
    if (this.listener === normalized) {
      this.listener = null;
    }
  }

  dispatch(data: unknown): void {
    this.listener?.({ data } as unknown as Event);
  }
}

class FakeHashTree {
  constructor(_config: unknown) {}
}

class FakeIdbBlobStorage {
  constructor(_storeName: string, _maxBytes: number) {}

  close(): void {}

  setMaxBytes(_maxBytes: number): void {}

  async getStats(): Promise<{ items: number; bytes: number; maxBytes: number }> {
    return { items: idbDataByHash.size, bytes: 0, maxBytes: 0 };
  }

  async get(hashHex: string): Promise<Uint8Array | null> {
    const found = idbDataByHash.get(hashHex);
    return found ? found.slice() : null;
  }

  async has(hashHex: string): Promise<boolean> {
    return idbDataByHash.has(hashHex);
  }

  async delete(hashHex: string): Promise<boolean> {
    return idbDataByHash.delete(hashHex);
  }

  async putByHashTrusted(hashHex: string, data: Uint8Array): Promise<void> {
    idbDataByHash.set(hashHex, data.slice());
  }
}

class FakeBlossomTransport {
  constructor(_servers: unknown, _onBandwidthUpdate?: (stats: unknown) => void) {}

  getBandwidthStats(): { totalBytesSent: number; totalBytesReceived: number; updatedAt: number; servers: [] } {
    return {
      totalBytesSent: 0,
      totalBytesReceived: 0,
      updatedAt: 0,
      servers: [],
    };
  }

  getServers(): Array<{ read: boolean; write: boolean }> {
    return [{ read: true, write: false }];
  }

  setServers(_servers: unknown): void {}

  async fetch(hashHex: string): Promise<Uint8Array | null> {
    const found = blossomDataByHash.get(hashHex);
    return found ? found.slice() : null;
  }
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes).map((value) => value.toString(16).padStart(2, '0')).join('');
}

function hexToBytes(value: string): Uint8Array {
  const bytes = new Uint8Array(value.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

vi.mock('@hashtree/core', () => ({
  HashTree: FakeHashTree,
  decryptChk: vi.fn(),
  fromHex: (value: string) => hexToBytes(value),
  nhashDecode: vi.fn(),
  nhashEncode: vi.fn(),
  toHex: (value: Uint8Array) => bytesToHex(value),
  tryDecodeTreeNode: vi.fn(),
}));

vi.mock('../src/capabilities/idbStorage.js', () => ({
  IdbBlobStorage: FakeIdbBlobStorage,
}));

vi.mock('../src/capabilities/blossomTransport.js', () => ({
  BlossomTransport: FakeBlossomTransport,
  DEFAULT_BLOSSOM_SERVERS: [],
}));

vi.mock('../src/capabilities/connectivity.js', () => ({
  probeConnectivity: vi.fn().mockResolvedValue({
    online: true,
    reachableReadServers: 1,
    totalReadServers: 1,
    reachableWriteServers: 0,
    totalWriteServers: 0,
    updatedAt: 0,
  }),
}));

vi.mock('../src/capabilities/rootResolver.js', () => ({
  resolveRootPathFromRelays: vi.fn(),
  watchRootPathFromRelays: vi.fn(),
}));

vi.mock('../src/mediaStreaming.js', () => ({
  streamFileRangeChunks: vi.fn(),
}));

function flush(): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

function readBlobResponse(id: string): { type: string; id: string; data?: Uint8Array; source?: string; error?: string } | undefined {
  return postMessageMock.mock.calls
    .map((call) => call[0] as { type: string; id?: string })
    .find((message) => message.type === 'blob' && message.id === id) as {
      type: string;
      id: string;
      data?: Uint8Array;
      source?: string;
      error?: string;
    } | undefined;
}

async function waitForBlobResponse(id: string): Promise<{ type: string; id: string; data?: Uint8Array; source?: string; error?: string }> {
  await vi.waitFor(() => {
    expect(readBlobResponse(id)).toBeDefined();
  });
  return readBlobResponse(id)!;
}

describe('worker peer blob sharing', () => {
  beforeEach(() => {
    vi.resetModules();
    postMessageMock.mockReset();
    idbDataByHash.clear();
    blossomDataByHash.clear();
    peerFetchResponder.handle = null;
    Object.defineProperty(globalThis, 'self', {
      configurable: true,
      writable: true,
      value: new FakeWorkerGlobal(),
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    // @ts-expect-error test cleanup
    delete globalThis.self;
  });

  it('serves a peer blob when the encrypted hash is reachable from a read source', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const hashHex = '11'.repeat(32);
    const blobData = new Uint8Array([1, 2, 3, 4]);
    idbDataByHash.set(hashHex, blobData);
    blossomDataByHash.set(hashHex, blobData);

    ctx.dispatch({
      type: 'init',
      id: 'init-1',
      config: {
        relays: [],
        blossomServers: [{ url: 'https://cdn.example', read: true, write: false }],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'getBlob',
      id: 'blob-1',
      hashHex,
      forPeer: true,
    });
    expect(await waitForBlobResponse('blob-1')).toEqual({
      type: 'blob',
      id: 'blob-1',
      data: blobData,
      source: 'blossom',
    });
  });

  it('refuses a peer blob when it is only available from the local encrypted cache', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const hashHex = '22'.repeat(32);
    idbDataByHash.set(hashHex, new Uint8Array([9, 8, 7]));

    ctx.dispatch({
      type: 'init',
      id: 'init-2',
      config: {
        relays: [],
        blossomServers: [{ url: 'https://cdn.example', read: true, write: false }],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'getBlob',
      id: 'blob-2',
      hashHex,
      forPeer: true,
    });
    expect(await waitForBlobResponse('blob-2')).toEqual({
      type: 'blob',
      id: 'blob-2',
      error: 'Refusing to serve blob to peer because it is not reachable from a shared read source',
    });
  });

  it('keeps waiting for a late peer blob instead of failing at the old worker timeout', async () => {
    vi.useFakeTimers();
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const hashHex = '33'.repeat(32);
    const blobData = new Uint8Array([4, 5, 6, 7]);
    peerFetchResponder.handle = (target, requestId) => {
      setTimeout(() => {
        target.dispatch({
          type: 'p2pFetchResult',
          id: `peer-hit-${requestId}`,
          requestId,
          data: blobData,
        });
      }, 16_000);
    };

    ctx.dispatch({
      type: 'init',
      id: 'init-3',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await vi.advanceTimersByTimeAsync(0);

    ctx.dispatch({
      type: 'getBlob',
      id: 'blob-3',
      hashHex,
    });
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(15_500);
    await Promise.resolve();
    await Promise.resolve();
    expect(readBlobResponse('blob-3')).toBeUndefined();

    await vi.advanceTimersByTimeAsync(500);
    await Promise.resolve();
    await Promise.resolve();
    expect(readBlobResponse('blob-3')).toEqual({
      type: 'blob',
      id: 'blob-3',
      data: blobData,
      source: 'p2p',
    });
  });
});
