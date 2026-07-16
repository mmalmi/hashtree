import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const endpointMessages = vi.hoisted(() => [] as unknown[]);
const p2pPeerIds = vi.hoisted(() => [] as string[]);
type FakeStore = {
  get(hash: Uint8Array): Promise<Uint8Array | null>;
};
const workerState = vi.hoisted(() => ({
  isDirectory: false,
  isDirectoryPlans: [] as boolean[],
  fileSize: 512 * 1024,
  readFileRangeImpl: vi.fn<(...args: unknown[]) => Promise<Uint8Array | null>>(),
  readFileStreamChunks: [] as Uint8Array[],
  readFileStreamPrefetches: [] as number[],
  readFileStreamPlans: [] as Array<{ chunks?: Uint8Array[]; error?: Error | null }>,
  readFileStreamOffsets: [] as number[],
  resolvePathImpl: vi.fn<(...args: unknown[]) => Promise<{ cid: { hash: Uint8Array }; type: number } | null>>(),
  stores: [] as FakeStore[],
}));

class FakeWorkerGlobal {
  private listener: EventListener | null = null;

  postMessage(message: unknown): void {
    endpointMessages.push(message);
    const request = message as { type?: string; requestId?: string };
    if (request.type === 'p2pPeerList' && request.requestId) {
      queueMicrotask(() => this.dispatch({
        type: 'p2pPeerListResult',
        id: `result-${request.requestId}`,
        requestId: request.requestId,
        peerIds: [...p2pPeerIds],
      }));
    }
  }

  addEventListener(_type: 'message', listener: EventListenerOrEventListenerObject): void {
    this.listener = typeof listener === 'function'
      ? listener
      : listener.handleEvent.bind(listener) as EventListener;
  }

  removeEventListener(_type: 'message', _listener: EventListenerOrEventListenerObject): void {
    this.listener = null;
  }

  dispatch(data: unknown): void {
    this.listener?.({ data } as unknown as Event);
  }
}

class FakeMessagePort {
  onmessage: ((event: MessageEvent<unknown>) => void) | null = null;
  readonly messages: unknown[] = [];

  start(): void {}

  close(): void {}

  postMessage(message: unknown): void {
    this.messages.push(message);
  }

  dispatch(data: unknown): void {
    this.onmessage?.({ data } as MessageEvent<unknown>);
  }
}

class FakeHashTree {
  constructor(config: { store: FakeStore }) {
    workerState.stores.push(config.store);
  }

  async isDirectory(): Promise<boolean> {
    if (workerState.isDirectoryPlans.length > 0) {
      return workerState.isDirectoryPlans.shift() ?? false;
    }
    return workerState.isDirectory;
  }

  async readFileRange(...args: unknown[]): Promise<Uint8Array | null> {
    return await workerState.readFileRangeImpl(...args);
  }

  async resolvePath(...args: unknown[]): Promise<{ cid: { hash: Uint8Array }; type: number } | null> {
    return await workerState.resolvePathImpl(...args);
  }

  async *readFileStream(_cid?: unknown, options?: { offset?: number }): AsyncGenerator<Uint8Array> {
    workerState.readFileStreamOffsets.push(options?.offset ?? 0);
    workerState.readFileStreamPrefetches.push((options as { prefetch?: number } | undefined)?.prefetch ?? 1);
    const plan = workerState.readFileStreamPlans.shift() ?? {
      chunks: workerState.readFileStreamChunks,
      error: null,
    };
    for (const chunk of plan.chunks ?? []) {
      yield chunk.slice();
    }
    if (plan.error) {
      throw plan.error;
    }
  }

  async getSize(): Promise<number> {
    return workerState.fileSize;
  }
}

class FakeIdbBlobStorage {
  constructor(_storeName: string, _maxBytes: number) {}

  close(): void {}

  setMaxBytes(_maxBytes: number): void {}

  async getStats(): Promise<{ items: number; bytes: number; maxBytes: number }> {
    return { items: 0, bytes: 0, maxBytes: 0 };
  }

  async get(_hashHex: string): Promise<Uint8Array | null> {
    return null;
  }

  async has(_hashHex: string): Promise<boolean> {
    return false;
  }

  async delete(_hashHex: string): Promise<boolean> {
    return false;
  }

  async authorizePeerSharing(_hashHexes: Iterable<string>): Promise<void> {}

  async loadPeerShareAuthorizations(): Promise<string[]> {
    return [];
  }

  async putByHashTrusted(_hashHex: string, _data: Uint8Array): Promise<void> {}
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
    return [];
  }

  getReadServers(): Array<{ url: string; read: boolean; write: boolean }> {
    return [];
  }

  setServers(_servers: unknown): void {}

  async fetch(_hashHex: string): Promise<Uint8Array | null> {
    return null;
  }

  async fetchFromServer(_hashHex: string, _serverUrl: string): Promise<Uint8Array | null> {
    return null;
  }
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (value) => value.toString(16).padStart(2, '0')).join('');
}

vi.mock('@hashtree/core', () => ({
  BLOB_DEFAULT_HTL: 10,
  BLOB_NO_RESULT: { type: 'no-result' },
  HashTree: FakeHashTree,
  blobData: (data: Uint8Array) => ({ type: 'data', data }),
  blobReplyFromNullable: (data: Uint8Array | null | undefined) => (
    data === null || data === undefined ? { type: 'no-result' } : { type: 'data', data }
  ),
  createBlobRequest: (hash: Uint8Array, htl = 10) => ({ hash, htl }),
  decryptChk: vi.fn(),
  fromHex: vi.fn(),
  nhashDecode: vi.fn(() => ({ hash: new Uint8Array(32).fill(7) })),
  nhashEncode: vi.fn(),
  sha256: async (data: Uint8Array) => new Uint8Array(32).fill(data[0] ?? 0),
  toHex: (data: Uint8Array) => bytesToHex(data),
  tryDecodeTreeNode: vi.fn(() => null),
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
    reachableReadServers: 0,
    totalReadServers: 0,
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

async function startRoutedMediaRead(
  requestId: string,
  p2pProviderEnabled: boolean,
): Promise<{ ctx: FakeWorkerGlobal; mediaPort: FakeMessagePort }> {
  const { attachHashtreeWorker } = await import('../src/worker.js');
  const ctx = globalThis.self as FakeWorkerGlobal;
  attachHashtreeWorker(ctx);
  workerState.readFileRangeImpl.mockImplementation(async (cid: { hash: Uint8Array }) => (
    await workerState.stores[1]?.get(cid.hash) ?? null
  ));

  ctx.dispatch({
    type: 'init',
    id: `init-${requestId}`,
    p2pProviderEnabled,
    config: { relays: [], blossomServers: [] },
  });
  await flush();

  const mediaPort = new FakeMessagePort();
  ctx.dispatch({
    type: 'registerMediaPort',
    id: `register-${requestId}`,
    port: mediaPort,
  });
  await flush();
  mediaPort.dispatch({
    type: 'hashtree-file',
    requestId,
    nhash: 'nhash1media',
    path: '',
    start: 0,
    sizeHint: 4,
    rangeHeader: 'bytes=0-',
    mimeType: 'image/jpeg',
  });
  return { ctx, mediaPort };
}

describe('worker media headers', () => {
  beforeEach(() => {
    vi.resetModules();
    endpointMessages.length = 0;
    p2pPeerIds.length = 0;
    workerState.isDirectory = false;
    workerState.isDirectoryPlans = [];
    workerState.readFileRangeImpl.mockReset();
    workerState.readFileStreamPlans = [];
    workerState.readFileStreamOffsets = [];
    workerState.readFileStreamPrefetches = [];
    workerState.fileSize = 512 * 1024;
    workerState.resolvePathImpl.mockReset();
    workerState.readFileStreamChunks = [];
    workerState.stores = [];
    Object.defineProperty(globalThis, 'self', {
      configurable: true,
      writable: true,
      value: new FakeWorkerGlobal(),
    });
  });

  afterEach(() => {
    // @ts-expect-error test cleanup
    delete globalThis.self;
  });

  it('honors startup byte-range requests with partial-content headers before the first chunk finishes loading', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    workerState.isDirectory = true;
    workerState.resolvePathImpl.mockResolvedValue({
      cid: { hash: new Uint8Array([1, 1, 1]) },
      type: 1,
    });
    let resolveRange: ((data: Uint8Array | null) => void) | null = null;
    workerState.readFileRangeImpl.mockImplementation(() => new Promise((resolve) => {
      resolveRange = resolve;
    }));

    ctx.dispatch({
      type: 'init',
      id: 'init-audio',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    const mediaPort = new FakeMessagePort();
    ctx.dispatch({
      type: 'registerMediaPort',
      id: 'register-audio-port',
      port: mediaPort,
    });
    await flush();

    mediaPort.dispatch({
      type: 'hashtree-file',
      requestId: 'audio-1',
      nhash: 'nhash1audio',
      path: 'song.mp3',
      start: 0,
      rangeHeader: 'bytes=0-',
      mimeType: 'audio/mpeg',
    });
    await flush();

    expect(mediaPort.messages).toContainEqual(expect.objectContaining({
      type: 'headers',
      requestId: 'audio-1',
      status: 206,
      headers: expect.objectContaining({
        'content-range': 'bytes 0-262143/524288',
        'content-length': '262144',
      }),
    }));

    resolveRange?.(new Uint8Array(256 * 1024));
    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'done',
        requestId: 'audio-1',
      }));
    });
  });

  it('keeps media reads Blossom-only when no P2P provider bridge is configured', async () => {
    const { mediaPort } = await startRoutedMediaRead('media-without-provider', false);

    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual({
        type: 'error',
        requestId: 'media-without-provider',
        message: 'File not found',
      });
    });
    expect(endpointMessages).not.toContainEqual(expect.objectContaining({ type: 'p2pFetch' }));
  });

  it('routes media through the configured P2P provider bridge with native HTL', async () => {
    p2pPeerIds.push('configured-media-peer');
    const { ctx, mediaPort } = await startRoutedMediaRead('media-with-provider', true);

    let p2pRequest: { requestId: string; hashHex: string; htl: number; peerId?: string } | undefined;
    await vi.waitFor(() => {
      p2pRequest = endpointMessages.find((message) => (
        (message as { type?: string }).type === 'p2pFetch'
      )) as typeof p2pRequest;
      expect(p2pRequest).toMatchObject({
        hashHex: '07'.repeat(32),
        htl: 10,
        peerId: 'configured-media-peer',
      });
    });

    ctx.dispatch({
      type: 'p2pFetchResult',
      id: `result-${p2pRequest!.requestId}`,
      requestId: p2pRequest!.requestId,
      data: new Uint8Array([7, 8, 9, 10]),
    });

    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'chunk',
        requestId: 'media-with-provider',
        data: new Uint8Array([7, 8, 9, 10]),
      }));
      expect(mediaPort.messages).toContainEqual({
        type: 'done',
        requestId: 'media-with-provider',
      });
    });
  });

  it('returns a larger partial-content window for nonzero open-ended video ranges', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const { streamFileRangeChunks } = await import('../src/mediaStreaming.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    workerState.isDirectory = true;
    workerState.fileSize = 16 * 1024 * 1024;
    workerState.resolvePathImpl.mockResolvedValue({
      cid: { hash: new Uint8Array([6, 6, 6]) },
      type: 1,
    });
    vi.mocked(streamFileRangeChunks).mockReturnValue((async function* () {
      yield new Uint8Array([1, 2, 3, 4]);
    })());

    ctx.dispatch({
      type: 'init',
      id: 'init-video-seek',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    const mediaPort = new FakeMessagePort();
    ctx.dispatch({
      type: 'registerMediaPort',
      id: 'register-video-seek-port',
      port: mediaPort,
    });
    await flush();

    mediaPort.dispatch({
      type: 'hashtree-file',
      requestId: 'video-seek-1',
      nhash: 'nhash1video',
      path: 'episode.mkv',
      start: 8 * 1024 * 1024,
      rangeHeader: 'bytes=8388608-',
      mimeType: 'video/x-matroska',
    });

    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'headers',
        requestId: 'video-seek-1',
        status: 206,
        headers: expect.objectContaining({
          'content-range': `bytes 8388608-16777215/${16 * 1024 * 1024}`,
          'content-length': String(8 * 1024 * 1024),
        }),
      }));
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'done',
        requestId: 'video-seek-1',
      }));
    });

    expect(vi.mocked(streamFileRangeChunks)).toHaveBeenCalledWith(
      expect.anything(),
      expect.anything(),
      8 * 1024 * 1024,
      16 * 1024 * 1024 - 1,
      256 * 1024,
      4,
    );
  });

  it('keeps non-audio requests waiting for the first chunk before replying', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    workerState.isDirectory = true;
    workerState.resolvePathImpl.mockResolvedValue({
      cid: { hash: new Uint8Array([2, 2, 2]) },
      type: 1,
    });
    let resolveRange: ((data: Uint8Array | null) => void) | null = null;
    workerState.readFileRangeImpl.mockImplementation(() => new Promise((resolve) => {
      resolveRange = resolve;
    }));

    ctx.dispatch({
      type: 'init',
      id: 'init-image',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    const mediaPort = new FakeMessagePort();
    ctx.dispatch({
      type: 'registerMediaPort',
      id: 'register-image-port',
      port: mediaPort,
    });
    await flush();

    mediaPort.dispatch({
      type: 'hashtree-file',
      requestId: 'image-1',
      nhash: 'nhash1image',
      path: 'cover.jpg',
      start: 0,
      mimeType: 'image/jpeg',
    });
    await flush();

    expect(mediaPort.messages).toEqual([]);

    resolveRange?.(new Uint8Array([5, 6, 7, 8]));
    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'headers',
        requestId: 'image-1',
        status: 200,
      }));
    });
  });

  it('sends explicit download headers before the first non-media chunk finishes loading', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    workerState.isDirectory = true;
    workerState.resolvePathImpl.mockResolvedValue({
      cid: { hash: new Uint8Array([3, 3, 3]) },
      type: 1,
    });
    let resolveRange: ((data: Uint8Array | null) => void) | null = null;
    workerState.readFileRangeImpl.mockImplementation(() => new Promise((resolve) => {
      resolveRange = resolve;
    }));

    ctx.dispatch({
      type: 'init',
      id: 'init-download',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    const mediaPort = new FakeMessagePort();
    ctx.dispatch({
      type: 'registerMediaPort',
      id: 'register-download-port',
      port: mediaPort,
    });
    await flush();

    mediaPort.dispatch({
      type: 'hashtree-file',
      requestId: 'download-1',
      nhash: 'nhash1download',
      path: 'iris-drive.dmg',
      start: 0,
      mimeType: 'application/octet-stream',
      download: true,
    });
    await flush();

    expect(mediaPort.messages).toContainEqual(expect.objectContaining({
      type: 'headers',
      requestId: 'download-1',
      status: 200,
      headers: expect.objectContaining({
        'content-disposition': 'attachment; filename="iris-drive.dmg"',
      }),
    }));
    expect(mediaPort.messages).not.toContainEqual(expect.objectContaining({
      type: 'chunk',
      requestId: 'download-1',
    }));

    resolveRange?.(new Uint8Array([5, 6, 7, 8]));
    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'done',
        requestId: 'download-1',
      }));
    });
  });

  it('retries transient directory path misses before failing an image request', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    workerState.isDirectory = true;
    workerState.resolvePathImpl
      .mockResolvedValueOnce(null)
      .mockResolvedValueOnce({
        cid: { hash: new Uint8Array([9, 9, 9]) },
        type: 0,
      });
    workerState.readFileRangeImpl.mockResolvedValue(new Uint8Array([8, 7, 6, 5]));

    ctx.dispatch({
      type: 'init',
      id: 'init-image-retry',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    const mediaPort = new FakeMessagePort();
    ctx.dispatch({
      type: 'registerMediaPort',
      id: 'register-image-retry-port',
      port: mediaPort,
    });
    await flush();

    mediaPort.dispatch({
      type: 'hashtree-file',
      requestId: 'image-retry-1',
      nhash: 'nhash1image',
      path: 'image',
      start: 0,
      mimeType: 'image/jpeg',
    });

    await vi.waitFor(() => {
      expect(workerState.resolvePathImpl).toHaveBeenCalledTimes(2);
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'done',
        requestId: 'image-retry-1',
      }));
    });
  });

  it('retries a transient root-is-file result before streaming directory-root media', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    workerState.isDirectoryPlans = [false, true];
    workerState.resolvePathImpl.mockResolvedValue({
      cid: { hash: new Uint8Array([4, 4, 4]) },
      type: 1,
    });
    workerState.readFileRangeImpl.mockResolvedValue(new Uint8Array([1, 2, 3, 4]));

    ctx.dispatch({
      type: 'init',
      id: 'init-root-kind-retry',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    const mediaPort = new FakeMessagePort();
    ctx.dispatch({
      type: 'registerMediaPort',
      id: 'register-root-kind-retry-port',
      port: mediaPort,
    });
    await flush();

    mediaPort.dispatch({
      type: 'hashtree-file',
      requestId: 'image-root-kind-1',
      nhash: 'nhash1image',
      path: 'image',
      start: 0,
      mimeType: 'image/jpeg',
    });

    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'done',
        requestId: 'image-root-kind-1',
      }));
      expect(workerState.resolvePathImpl).toHaveBeenCalled();
    });
  });

  it('resumes audio streaming after a transient missing chunk instead of aborting playback', async () => {
    const { attachHashtreeWorker } = await import('../src/worker.js');
    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    workerState.isDirectory = true;
    workerState.resolvePathImpl.mockResolvedValue({
      cid: { hash: new Uint8Array([5, 5, 5]) },
      type: 1,
    });
    workerState.readFileRangeImpl.mockResolvedValue(new Uint8Array([1, 2, 3, 4]));
    workerState.readFileStreamPlans = [
      {
        chunks: [new Uint8Array([5, 6])],
        error: new Error('Missing chunk: deadbeef'),
      },
      {
        chunks: [new Uint8Array([7, 8])],
        error: null,
      },
    ];

    ctx.dispatch({
      type: 'init',
      id: 'init-audio-retry',
      config: {
        relays: [],
        blossomServers: [],
      },
    });
    await flush();

    const mediaPort = new FakeMessagePort();
    ctx.dispatch({
      type: 'registerMediaPort',
      id: 'register-audio-retry-port',
      port: mediaPort,
    });
    await flush();

    mediaPort.dispatch({
      type: 'hashtree-file',
      requestId: 'audio-retry-1',
      nhash: 'nhash1audio',
      path: 'song.mp3',
      start: 0,
      mimeType: 'audio/mpeg',
    });

    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'done',
        requestId: 'audio-retry-1',
      }));
      expect(mediaPort.messages).not.toContainEqual(expect.objectContaining({
        type: 'error',
        requestId: 'audio-retry-1',
      }));
      expect(workerState.readFileStreamOffsets).toEqual([4, 6]);
      expect(workerState.readFileStreamPrefetches).toEqual([4, 4]);
    });
  });
});
