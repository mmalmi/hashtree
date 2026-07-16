import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const postMessageMock = vi.hoisted(() => vi.fn());
const idbDataByHash = vi.hoisted(() => new Map<string, Uint8Array>());
const peerShareableByStore = vi.hoisted(() => new Map<string, Set<string>>());
const blossomDataByHash = vi.hoisted(() => new Map<string, Uint8Array>());
const peerFetchResponder = vi.hoisted(() => ({
  handle: null as null | ((
    ctx: FakeWorkerGlobal,
    requestId: string,
    hashHex: string,
    peerId?: string,
    htl?: number,
  ) => void),
}));
const peerListResponder = vi.hoisted(() => ({
  peerIds: [] as string[],
}));

class FakeWorkerGlobal {
  private listener: EventListener | null = null;

  postMessage(message: unknown): void {
    postMessageMock(message);
    const candidate = message as { type?: string; requestId?: string };
    if (candidate?.type === 'p2pPeerList' && typeof candidate.requestId === 'string') {
      queueMicrotask(() => {
        this.dispatch({
          type: 'p2pPeerListResult',
          id: `peer-list-${candidate.requestId}`,
          requestId: candidate.requestId,
          peerIds: peerListResponder.peerIds,
        });
      });
      return;
    }
    if (candidate?.type === 'p2pFetch' && typeof candidate.requestId === 'string') {
      const request = message as { hashHex?: string; peerId?: string; htl?: number };
      if (peerFetchResponder.handle) {
        peerFetchResponder.handle(
          this,
          candidate.requestId,
          `${request.hashHex ?? ''}`,
          request.peerId,
          request.htl,
        );
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

type FakeStore = {
  put(hash: Uint8Array, data: Uint8Array): Promise<boolean>;
};

class FakeHashTree {
  private readonly store: FakeStore;

  constructor(config: { store: FakeStore }) {
    this.store = config.store;
  }

  async putBlob(data: Uint8Array): Promise<Uint8Array> {
    const hash = new Uint8Array(32).fill(data[0] ?? 0);
    await this.store.put(hash, data);
    return hash;
  }

  async putFile(data: Uint8Array): Promise<{ cid: { hash: Uint8Array; key: Uint8Array } }> {
    const hash = await this.putBlob(data);
    return { cid: { hash, key: new Uint8Array(32).fill(1) } };
  }

  async *walkBlocks(cid: { hash: Uint8Array }): AsyncGenerator<{ hash: Uint8Array }> {
    yield { hash: cid.hash };
  }
}

class FakeIdbBlobStorage {
  private readonly peerShareable: Set<string>;

  constructor(storeName: string, _maxBytes: number) {
    const existing = peerShareableByStore.get(storeName) ?? new Set<string>();
    peerShareableByStore.set(storeName, existing);
    this.peerShareable = existing;
  }

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
    this.peerShareable.delete(hashHex);
    return idbDataByHash.delete(hashHex);
  }

  async authorizePeerSharing(hashHexes: Iterable<string>): Promise<void> {
    for (const hashHex of hashHexes) this.peerShareable.add(hashHex.toLowerCase());
  }

  async loadPeerShareAuthorizations(): Promise<string[]> {
    return [...this.peerShareable];
  }

  async putByHash(hashHex: string, data: Uint8Array): Promise<void> {
    idbDataByHash.set(hashHex, data.slice());
  }

  async putByHashTrusted(hashHex: string, data: Uint8Array): Promise<void> {
    idbDataByHash.set(hashHex, data.slice());
  }
}

class FakeBlossomTransport {
  private readonly servers: Array<{ url: string; read?: boolean; write?: boolean }>;

  constructor(servers: Array<{ url: string; read?: boolean; write?: boolean }> = [], _onBandwidthUpdate?: (stats: unknown) => void) {
    this.servers = servers;
  }

  getBandwidthStats(): { totalBytesSent: number; totalBytesReceived: number; updatedAt: number; servers: [] } {
    return {
      totalBytesSent: 0,
      totalBytesReceived: 0,
      updatedAt: 0,
      servers: [],
    };
  }

  getServers(): Array<{ url: string; read: boolean; write: boolean }> {
    return this.servers.map((server) => ({
      url: server.url,
      read: server.read !== false,
      write: !!server.write,
    }));
  }

  getReadServers(): Array<{ url: string; read: boolean; write: boolean }> {
    return this.servers
      .filter((server) => server.read !== false)
      .map((server) => ({
        url: server.url,
        read: true,
        write: !!server.write,
      }));
  }

  getWriteServers(): Array<{ url: string; read: boolean; write: boolean }> {
    return this.servers
      .filter((server) => !!server.write)
      .map((server) => ({
        url: server.url,
        read: server.read !== false,
        write: true,
      }));
  }

  setServers(_servers: unknown): void {}

  createUploadStore(): { put: (hash: Uint8Array, data: Uint8Array) => Promise<boolean> } {
    return {
      put: async (hash: Uint8Array, data: Uint8Array): Promise<boolean> => {
        blossomDataByHash.set(bytesToHex(hash), data.slice());
        return true;
      },
    };
  }

  async fetch(hashHex: string): Promise<Uint8Array | null> {
    const found = blossomDataByHash.get(hashHex);
    return found ? found.slice() : null;
  }

  async fetchFromServer(hashHex: string, _serverUrl: string): Promise<Uint8Array | null> {
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

function hashHexForData(data: Uint8Array): string {
  return (data[0] ?? 0).toString(16).padStart(2, '0').repeat(32);
}

class FakeStoreBlobRoute {
  constructor(readonly id: string, private readonly store: {
    get(hash: Uint8Array): Promise<Uint8Array | null>;
  }) {}

  async read(request: { hash: Uint8Array }) {
    const data = await this.store.get(request.hash);
    return data === null ? { type: 'no-result' } : { type: 'data', data: data.slice() };
  }
}

async function verifyFakeBlobData(expectedHash: Uint8Array, data: Uint8Array): Promise<Uint8Array> {
  if (bytesToHex(expectedHash) !== hashHexForData(data)) throw new Error('wrong hash');
  return data.slice();
}

vi.mock('@hashtree/core', () => ({
  BLOB_DEFAULT_HTL: 10,
  BLOB_MAX_HTL: 10,
  BLOB_NO_RESULT: { type: 'no-result' },
  HashTree: FakeHashTree,
  StoreBlobRoute: FakeStoreBlobRoute,
  blobData: (data: Uint8Array) => ({ type: 'data', data }),
  blobReplyFromNullable: (data: Uint8Array | null | undefined) => (
    data === null || data === undefined ? { type: 'no-result' } : { type: 'data', data }
  ),
  createBlobRequest: (hash: Uint8Array, htl = 10) => ({ hash, htl }),
  decryptChk: vi.fn(),
  fromHex: (value: string) => hexToBytes(value),
  nhashDecode: vi.fn(),
  nhashEncode: ({ hash }: { hash: Uint8Array }) => `nhash1${bytesToHex(hash).slice(0, 8)}`,
  sha256: async (data: Uint8Array) => new Uint8Array(32).fill(data[0] ?? 0),
  toHex: (value: Uint8Array) => bytesToHex(value),
  tryDecodeTreeNode: vi.fn(),
  verifyBlobData: verifyFakeBlobData,
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

function readBlockStoredResponse(id: string): { type: string; id: string; block?: { hashHex: string; nhash: string } } | undefined {
  return postMessageMock.mock.calls
    .map((call) => call[0] as { type: string; id?: string })
    .find((message) => message.type === 'blockStored' && message.id === id) as {
      type: string;
      id: string;
      block?: { hashHex: string; nhash: string };
    } | undefined;
}

async function waitForBlobResponse(id: string): Promise<{ type: string; id: string; data?: Uint8Array; source?: string; error?: string }> {
  await vi.waitFor(() => {
    expect(readBlobResponse(id)).toBeDefined();
  });
  return readBlobResponse(id)!;
}

async function waitForBlockStoredResponse(id: string): Promise<{ type: string; id: string; block?: { hashHex: string; nhash: string } }> {
  await vi.waitFor(() => {
    expect(readBlockStoredResponse(id)).toBeDefined();
  });
  return readBlockStoredResponse(id)!;
}

describe('worker peer blob sharing', () => {
  beforeEach(() => {
    vi.resetModules();
    postMessageMock.mockReset();
    idbDataByHash.clear();
    peerShareableByStore.clear();
    blossomDataByHash.clear();
    peerFetchResponder.handle = null;
    peerListResponder.peerIds = [];
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

  it('serves a remotely verified peer blob only for the current worker session', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const blobData = new Uint8Array([1, 2, 3, 4]);
    const hashHex = hashHexForData(blobData);
    idbDataByHash.set(hashHex, blobData);
    blossomDataByHash.set(hashHex, blobData);

    ctx.dispatch({
      type: 'init',
      id: 'init-1',
      p2pProviderEnabled: true,
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

    blossomDataByHash.clear();
    ctx.dispatch({
      type: 'init',
      id: 'init-1-restart',
      p2pProviderEnabled: true,
      config: { relays: [], blossomServers: [] },
    });
    await flush();
    ctx.dispatch({ type: 'getBlob', id: 'blob-1-after-restart', hashHex, forPeer: true });
    expect(await waitForBlobResponse('blob-1-after-restart')).toEqual({
      type: 'blob',
      id: 'blob-1-after-restart',
      error: 'Refusing to serve blob to peer because it is not reachable from a shared read source',
    });
  });

  it('refuses a peer blob when it is only available from the local encrypted cache', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const localData = new Uint8Array([9, 8, 7]);
    const hashHex = hashHexForData(localData);
    idbDataByHash.set(hashHex, localData);

    ctx.dispatch({
      type: 'init',
      id: 'init-2',
      p2pProviderEnabled: true,
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

  it('restores explicit peer-share authorization after restart without exposing local-only cache', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);
    const storeName = 'restart-peer-share';
    const sharedData = new Uint8Array([21, 1, 2]);
    const privateData = new Uint8Array([22, 3, 4]);
    const sharedHashHex = hashHexForData(sharedData);
    const privateHashHex = hashHexForData(privateData);

    ctx.dispatch({
      type: 'init',
      id: 'init-before-restart',
      p2pProviderEnabled: true,
      config: { storeName, relays: [], blossomServers: [] },
    });
    await flush();
    ctx.dispatch({ type: 'putBlob', id: 'put-shared', data: sharedData, upload: true });
    ctx.dispatch({ type: 'putBlob', id: 'put-private', data: privateData, upload: false });
    await vi.waitFor(() => {
      expect(postMessageMock).toHaveBeenCalledWith(expect.objectContaining({ type: 'blobStored', id: 'put-shared' }));
      expect(postMessageMock).toHaveBeenCalledWith(expect.objectContaining({ type: 'blobStored', id: 'put-private' }));
    });

    ctx.dispatch({
      type: 'init',
      id: 'init-after-restart',
      p2pProviderEnabled: true,
      config: { storeName, relays: [], blossomServers: [] },
    });
    await flush();
    ctx.dispatch({ type: 'getBlob', id: 'shared-after-restart', hashHex: sharedHashHex, forPeer: true });
    ctx.dispatch({ type: 'getBlob', id: 'private-after-restart', hashHex: privateHashHex, forPeer: true });

    expect(await waitForBlobResponse('shared-after-restart')).toEqual({
      type: 'blob',
      id: 'shared-after-restart',
      data: sharedData,
      source: 'idb',
    });
    expect(await waitForBlobResponse('private-after-restart')).toEqual({
      type: 'blob',
      id: 'private-after-restart',
      error: 'Refusing to serve blob to peer because it is not reachable from a shared read source',
    });
  });

  it('keeps waiting for a late peer blob instead of failing at the old worker timeout', async () => {
    vi.useFakeTimers();
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const blobData = new Uint8Array([4, 5, 6, 7]);
    const hashHex = hashHexForData(blobData);
    peerListResponder.peerIds = ['peer-late'];
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
      p2pProviderEnabled: true,
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

  it('expires unanswered p2p fetches at the mesh read timeout', async () => {
    vi.useFakeTimers();
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const hashHex = '35'.repeat(32);
    peerListResponder.peerIds = ['peer-timeout'];
    let capturedRequestId = '';
    let requestObservedAt = 0;
    peerFetchResponder.handle = (_target, requestId) => {
      capturedRequestId = requestId;
      requestObservedAt = Date.now();
    };

    ctx.dispatch({
      type: 'init',
      id: 'init-3c',
      p2pProviderEnabled: true,
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await vi.advanceTimersByTimeAsync(0);

    ctx.dispatch({
      type: 'getBlob',
      id: 'blob-3c',
      hashHex,
    });
    for (let attempt = 0; attempt < 20 && !capturedRequestId; attempt += 1) {
      await vi.advanceTimersByTimeAsync(50);
      await Promise.resolve();
    }
    expect(capturedRequestId).toBeTruthy();

    const beforeTimeoutDelay = Math.max(0, requestObservedAt + 19_999 - Date.now());
    await vi.advanceTimersByTimeAsync(beforeTimeoutDelay);
    await Promise.resolve();
    expect(readBlobResponse('blob-3c')).toBeUndefined();

    const timeoutDelay = Math.max(1, requestObservedAt + 20_000 - Date.now());
    await vi.advanceTimersByTimeAsync(timeoutDelay);
    await Promise.resolve();
    await Promise.resolve();
    expect(readBlobResponse('blob-3c')?.error).toMatch(/timed out/i);

    ctx.dispatch({
      type: 'p2pFetchResult',
      id: `late-${capturedRequestId}`,
      requestId: capturedRequestId,
      data: new Uint8Array([9, 9, 9]),
    });
    await Promise.resolve();
    expect(readBlobResponse('blob-3c')?.error).toMatch(/timed out/i);
  });

  it('returns a provider error instead of converting it to Blob not found', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const hashHex = '36'.repeat(32);
    peerListResponder.peerIds = ['peer-error'];
    peerFetchResponder.handle = (target, requestId) => {
      queueMicrotask(() => {
        target.dispatch({
          type: 'p2pFetchResult',
          id: `peer-error-${requestId}`,
          requestId,
          error: 'peer unreachable',
        });
      });
    };

    ctx.dispatch({
      type: 'init',
      id: 'init-3d',
      p2pProviderEnabled: true,
      config: { relays: [], blossomServers: [] },
    });
    await flush();

    ctx.dispatch({ type: 'getBlob', id: 'blob-3d', hashHex });

    expect((await waitForBlobResponse('blob-3d')).error).toContain('peer unreachable');
  });

  it('does not install an auto-fetch route when no p2p provider is configured', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    ctx.dispatch({
      type: 'init',
      id: 'init-3e',
      config: { relays: [], blossomServers: [] },
    });
    await flush();

    ctx.dispatch({ type: 'getBlob', id: 'blob-3e', hashHex: '37'.repeat(32) });

    expect(await waitForBlobResponse('blob-3e')).toEqual({
      type: 'blob',
      id: 'blob-3e',
      error: 'Blob not found',
    });
    expect(postMessageMock.mock.calls.some(([message]) => (
      (message as { type?: string }).type === 'p2pFetch'
    ))).toBe(false);
  });

  it('lets the configured provider handle an empty provider roster as a miss', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const hashHex = hashHexForData(new Uint8Array([6, 7, 8, 9]));
    peerListResponder.peerIds = [];

    ctx.dispatch({
      type: 'init',
      id: 'init-3b',
      p2pProviderEnabled: true,
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'getBlob',
      id: 'blob-3b',
      hashHex,
    });

    expect(await waitForBlobResponse('blob-3b')).toEqual({
      type: 'blob',
      id: 'blob-3b',
      error: 'Blob not found',
    });
    expect(postMessageMock.mock.calls.some(([message]) => (
      (message as { type?: string }).type === 'p2pFetch'
    ))).toBe(true);
  });

  it('leaves exact p2p peer selection inside the configured provider', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const blobData = new Uint8Array([8, 9, 10]);
    const hashHex = hashHexForData(blobData);
    peerListResponder.peerIds = ['peer-a'];
    peerFetchResponder.handle = (target, requestId, requestedHashHex, peerId) => {
      expect(requestedHashHex).toBe(hashHex);
      expect(peerId).toBeUndefined();
      queueMicrotask(() => {
        target.dispatch({
          type: 'p2pFetchResult',
          id: `peer-hit-${requestId}`,
          requestId,
          data: blobData,
        });
      });
    };

    ctx.dispatch({
      type: 'init',
      id: 'init-4',
      p2pProviderEnabled: true,
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'getBlob',
      id: 'blob-4',
      hashHex,
    });

    expect(await waitForBlobResponse('blob-4')).toEqual({
      type: 'blob',
      id: 'blob-4',
      data: blobData,
      source: 'p2p',
    });
    expect(
      postMessageMock.mock.calls.some(
        ([message]) => (
          (message as { type?: string; peerId?: string }).type === 'p2pFetch'
          && !('peerId' in (message as { peerId?: string }))
        ),
      ),
    ).toBe(true);
  });

  it('stores raw blocks locally, uploads them through Blossom, and serves them to peers', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    const blobData = new Uint8Array([11, 12, 13]);
    const hashHex = hashHexForData(blobData);

    ctx.dispatch({
      type: 'init',
      id: 'init-5',
      p2pProviderEnabled: true,
      config: {
        relays: [],
        blossomServers: [{ url: 'https://upload.example', read: true, write: true }],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'putBlock',
      id: 'put-1',
      hashHex,
      data: blobData,
      upload: true,
    });

    expect(await waitForBlockStoredResponse('put-1')).toEqual({
      type: 'blockStored',
      id: 'put-1',
      block: {
        hashHex,
        nhash: `nhash1${hashHex.slice(0, 8)}`,
      },
    });
    expect(idbDataByHash.get(hashHex)).toEqual(blobData);
    expect(blossomDataByHash.get(hashHex)).toEqual(blobData);

    ctx.dispatch({
      type: 'getBlob',
      id: 'blob-5',
      hashHex,
      forPeer: true,
    });
    expect(await waitForBlobResponse('blob-5')).toEqual({
      type: 'blob',
      id: 'blob-5',
      data: blobData,
      source: 'idb',
    });
  });
});
