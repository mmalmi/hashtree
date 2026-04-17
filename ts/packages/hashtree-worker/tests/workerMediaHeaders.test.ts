import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const endpointMessages = vi.hoisted(() => [] as unknown[]);
const workerState = vi.hoisted(() => ({
  isDirectory: false,
  isDirectoryPlans: [] as boolean[],
  readFileRangeImpl: vi.fn<(...args: unknown[]) => Promise<Uint8Array | null>>(),
  readFileStreamChunks: [] as Uint8Array[],
  readFileStreamPlans: [] as Array<{ chunks?: Uint8Array[]; error?: Error | null }>,
  readFileStreamOffsets: [] as number[],
  resolvePathImpl: vi.fn<(...args: unknown[]) => Promise<{ cid: { hash: Uint8Array }; type: number } | null>>(),
}));

class FakeWorkerGlobal {
  private listener: EventListener | null = null;

  postMessage(message: unknown): void {
    endpointMessages.push(message);
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
  constructor(_config: unknown) {}

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
    return 512 * 1024;
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

  setServers(_servers: unknown): void {}

  async fetch(_hashHex: string): Promise<Uint8Array | null> {
    return null;
  }
}

vi.mock('@hashtree/core', () => ({
  HashTree: FakeHashTree,
  decryptChk: vi.fn(),
  fromHex: vi.fn(),
  nhashDecode: vi.fn(() => ({ hash: new Uint8Array([1, 2, 3]) })),
  nhashEncode: vi.fn(),
  toHex: vi.fn(() => 'deadbeef'),
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

describe('worker media headers', () => {
  beforeEach(() => {
    vi.resetModules();
    endpointMessages.length = 0;
    workerState.isDirectory = false;
    workerState.isDirectoryPlans = [];
    workerState.readFileRangeImpl.mockReset();
    workerState.readFileStreamPlans = [];
    workerState.readFileStreamOffsets = [];
    workerState.resolvePathImpl.mockReset();
    workerState.readFileStreamChunks = [];
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

  it('sends audio startup range headers before the first mesh chunk finishes loading', async () => {
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
      status: 200,
    }));

    resolveRange?.(new Uint8Array([1, 2, 3, 4]));
    await vi.waitFor(() => {
      expect(mediaPort.messages).toContainEqual(expect.objectContaining({
        type: 'done',
        requestId: 'audio-1',
      }));
    });
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
      rangeHeader: 'bytes=0-',
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
    });
  });
});
