import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { CID } from '@hashtree/core';

const resolveRootPathFromRelaysMock = vi.hoisted(() => vi.fn());
const watchRootPathFromRelaysMock = vi.hoisted(() => vi.fn());
const postMessageMock = vi.hoisted(() => vi.fn());
const closeWatchMock = vi.hoisted(() => vi.fn());

const ROOT: CID = {
  hash: Uint8Array.from({ length: 32 }, (_, index) => index + 1),
};

class FakeWorkerGlobal {
  private listener: EventListener | null = null;

  postMessage(message: unknown): void {
    postMessageMock(message);
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

  getServers(): [] {
    return [];
  }

  setServers(_servers: unknown): void {}
}

class FakeStoreBlobRoute {
  constructor(readonly id: string, private readonly store: {
    get(hash: Uint8Array): Promise<Uint8Array | null>;
  }) {}

  async read(request: { hash: Uint8Array }) {
    const data = await this.store.get(request.hash);
    return data === null ? { type: 'no-result' } : { type: 'data', data };
  }
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
  fromHex: vi.fn((value: string) => new Uint8Array(value.length / 2)),
  nhashDecode: vi.fn(),
  nhashEncode: vi.fn(),
  sha256: vi.fn(async () => new Uint8Array(32)),
  toHex: vi.fn(),
  tryDecodeTreeNode: vi.fn(),
  verifyBlobData: async (_hash: Uint8Array, data: Uint8Array) => data,
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
  resolveRootPathFromRelays: resolveRootPathFromRelaysMock,
  watchRootPathFromRelays: watchRootPathFromRelaysMock,
}));

vi.mock('../src/privacyGuards.js', () => ({
  assertEncryptedUploadCid: vi.fn(),
  markEncryptedHashes: vi.fn(),
  shouldServeHashToPeer: vi.fn(() => true),
}));

vi.mock('../src/mediaStreaming.js', () => ({
  streamFileRangeChunks: vi.fn(),
}));

function flush(): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

describe('worker root resolution message flow', () => {
  beforeEach(() => {
    vi.resetModules();
    resolveRootPathFromRelaysMock.mockReset();
    watchRootPathFromRelaysMock.mockReset();
    closeWatchMock.mockReset();
    postMessageMock.mockReset();
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

  it('routes resolveRoot requests through the worker protocol', async () => {
    resolveRootPathFromRelaysMock.mockResolvedValue(ROOT);
    const { attachHashtreeWorker } = await import('../src/worker.js');

    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    ctx.dispatch({
      type: 'init',
      id: 'init-1',
      config: {
        relays: ['wss://relay.example'],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'resolveRoot',
      id: 'resolve-1',
      npub: 'npub1example',
      path: 'audio-catalog/root.json',
      timeoutMs: 4_500,
      settleMs: 500,
    });
    await flush();

    expect(resolveRootPathFromRelaysMock).toHaveBeenCalledWith(
      expect.any(FakeHashTree),
      ['wss://relay.example'],
      'npub1example',
      'audio-catalog/root.json',
      4_500,
      500,
    );
    expect(postMessageMock).toHaveBeenCalledWith({
      type: 'cid',
      id: 'resolve-1',
      cid: ROOT,
    });
  });

  it('starts, emits, and stops root watches through the worker protocol', async () => {
    watchRootPathFromRelaysMock.mockResolvedValue({
      initialCid: ROOT,
      close: closeWatchMock,
    });
    const { attachHashtreeWorker } = await import('../src/worker.js');

    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    ctx.dispatch({
      type: 'init',
      id: 'init-2',
      config: {
        relays: ['wss://relay.example'],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'watchRoot',
      id: 'watch-1',
      npub: 'npub1example',
      path: 'audio-catalog/root.json',
      timeoutMs: 4_500,
      settleMs: 500,
    });
    await flush();

    const started = postMessageMock.mock.calls
      .map((call) => call[0] as { type?: string; watchId?: string })
      .find((message) => message.type === 'rootWatchStarted');
    expect(started?.watchId).toBeTruthy();

    const onUpdate = watchRootPathFromRelaysMock.mock.calls[0]?.[4] as ((cid: CID | null) => void) | undefined;
    expect(onUpdate).toBeTypeOf('function');
    onUpdate?.(null);
    await flush();

    expect(postMessageMock).toHaveBeenCalledWith({
      type: 'rootUpdate',
      watchId: started!.watchId,
      cid: undefined,
    });

    ctx.dispatch({
      type: 'unwatchRoot',
      id: 'unwatch-1',
      watchId: started!.watchId,
    });
    await flush();

    expect(closeWatchMock).toHaveBeenCalledTimes(1);
    expect(postMessageMock).toHaveBeenCalledWith({
      type: 'void',
      id: 'unwatch-1',
    });
  });

  it('starts root watches even when the initial cid is not ready yet', async () => {
    watchRootPathFromRelaysMock.mockResolvedValue({
      initialCid: null,
      close: closeWatchMock,
    });
    const { attachHashtreeWorker } = await import('../src/worker.js');

    const ctx = globalThis.self as FakeWorkerGlobal;
    attachHashtreeWorker(ctx);

    ctx.dispatch({
      type: 'init',
      id: 'init-3',
      config: {
        relays: ['wss://relay.example'],
      },
    });
    await flush();

    ctx.dispatch({
      type: 'watchRoot',
      id: 'watch-2',
      npub: 'npub1example',
      path: 'audio-catalog/root.json',
      timeoutMs: 4_500,
      settleMs: 500,
    });
    await flush();

    const started = postMessageMock.mock.calls
      .map((call) => call[0] as { type?: string; watchId?: string; cid?: CID })
      .find((message) => message.type === 'rootWatchStarted' && message.watchId);
    expect(started?.watchId).toBeTruthy();
    expect(started).not.toHaveProperty('cid');

    const onUpdate = watchRootPathFromRelaysMock.mock.calls[0]?.[4] as ((cid: CID | null) => void) | undefined;
    expect(onUpdate).toBeTypeOf('function');
    onUpdate?.(ROOT);
    await flush();

    expect(postMessageMock).toHaveBeenCalledWith({
      type: 'rootUpdate',
      watchId: started!.watchId,
      cid: ROOT,
    });
  });
});
