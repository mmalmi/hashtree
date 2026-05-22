import { describe, expect, it, vi } from 'vitest';
import { HashtreeWorkerClient } from '../src/client.js';
import { ManagedWebRTCMeshHost } from '../src/p2p/managedMeshHost.js';
import type { WorkerRequest, WorkerResponse } from '../src/protocol.js';

class FakeWorker {
  onmessage: ((event: MessageEvent<WorkerResponse>) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  postedMessages: Array<{ message: WorkerRequest; transfer?: Transferable[] }> = [];

  postMessage(message: WorkerRequest, transfer?: Transferable[]): void {
    this.postedMessages.push({ message, transfer });
    if (message.type === 'init') {
      this.emitMessage({ type: 'ready', id: message.id });
      return;
    }

    if (message.type === 'close') {
      this.emitMessage({ type: 'void', id: message.id });
    }
  }

  terminate(): void {
    // no-op
  }

  emitMessage(message: WorkerResponse): void {
    this.onmessage?.({ data: message } as MessageEvent<WorkerResponse>);
  }
}

class FakeSignalPool {
  destroyed = false;
  subscriptions: Array<{ closed: boolean }> = [];
  connectionStatus = new Map<string, boolean>();

  subscribe(): { close: () => void } {
    const subscription = { closed: false };
    this.subscriptions.push(subscription);
    return {
      close: () => {
        subscription.closed = true;
      },
    };
  }

  listConnectionStatus(): Map<string, boolean> {
    return new Map(this.connectionStatus);
  }

  destroy(): void {
    this.destroyed = true;
  }
}

class FakeProxy {
  closed = false;
  uploadLimit: number | null = null;

  handleCommand(): void {
    // no-op
  }

  setUploadLimitBytesPerSecond(limit?: number | null): void {
    this.uploadLimit = limit ?? null;
  }

  close(): void {
    this.closed = true;
  }
}

class FakeController {
  started = false;
  stopped = false;
  loadPeerMetadataCalls = 0;
  persistPeerMetadataCalls = 0;
  poolConfigCalls: unknown[] = [];
  broadcastHelloCalls = 0;
  connectedCount = 0;
  peerIds = ['peer-a'];

  async loadPeerMetadata(): Promise<boolean> {
    this.loadPeerMetadataCalls += 1;
    return true;
  }

  async persistPeerMetadata(): Promise<Uint8Array | null> {
    this.persistPeerMetadataCalls += 1;
    return null;
  }

  start(): void {
    this.started = true;
  }

  stop(): void {
    this.stopped = true;
  }

  setPoolConfig(config: unknown): void {
    this.poolConfigCalls.push(config);
  }

  getConnectedCount(): number {
    return this.connectedCount;
  }

  broadcastHello(): void {
    this.broadcastHelloCalls += 1;
  }

  getPeerStats(): Array<{ peerId: string }> {
    return this.peerIds.map((peerId) => ({ peerId }));
  }

  async handleSignalingMessage(): Promise<void> {
    // no-op
  }

  handleProxyEvent(): void {
    // no-op
  }

  async get(): Promise<Uint8Array | null> {
    return new Uint8Array([1, 2, 3]);
  }

  async getFromPeer(peerId: string): Promise<Uint8Array | null> {
    return peerId === 'peer-a' ? new Uint8Array([4, 5, 6]) : null;
  }

  getConnectedHashGetPeerIds(): string[] {
    return this.peerIds.slice();
  }
}

async function flushMicrotasks(): Promise<void> {
  for (let idx = 0; idx < 6; idx += 1) {
    await Promise.resolve();
  }
  await new Promise((resolve) => setTimeout(resolve, 0));
}

describe('ManagedWebRTCMeshHost', () => {
  it('attaches a worker provider backed by the active mesh session', async () => {
    const worker = new FakeWorker();
    const WorkerFactory = class {
      constructor() {
        return worker;
      }
    } as unknown as new () => Worker;
    const client = new HashtreeWorkerClient(WorkerFactory);
    const signalPool = new FakeSignalPool();
    const proxy = new FakeProxy();
    const controller = new FakeController();
    const host = new ManagedWebRTCMeshHost({
      createSignalPool: () => signalPool as unknown as any,
      createProxy: (_onEvent, uploadLimit) => {
        proxy.setUploadLimitBytesPerSecond(uploadLimit);
        return proxy as unknown as any;
      },
      createController: () => controller as unknown as any,
    });

    host.attachWorkerClient(client);
    await client.init();
    await host.setSession({
      signature: 'session-a',
      pubkey: 'pubkey-a',
      relayUrls: ['wss://relay.example'],
      localStore: {} as any,
      sendSignaling: async () => undefined,
      unwrapGift: async () => null,
    });

    worker.emitMessage({
      type: 'p2pPeerList',
      requestId: 'peers-1',
    });
    worker.emitMessage({
      type: 'p2pFetch',
      requestId: 'req-1',
      hashHex: '11'.repeat(32),
      peerId: 'peer-a',
    });
    worker.emitMessage({
      type: 'p2pFetch',
      requestId: 'req-2',
      hashHex: '22'.repeat(32),
    });
    await flushMicrotasks();

    expect(worker.postedMessages.map((entry) => entry.message)).toEqual(expect.arrayContaining([
      {
        type: 'p2pPeerListResult',
        id: expect.any(String),
        requestId: 'peers-1',
        peerIds: ['peer-a'],
      },
      {
        type: 'p2pFetchResult',
        id: expect.any(String),
        requestId: 'req-1',
        data: new Uint8Array([4, 5, 6]),
      },
      {
        type: 'p2pFetchResult',
        id: expect.any(String),
        requestId: 'req-2',
        data: new Uint8Array([1, 2, 3]),
      },
    ]));

    await client.close();
    await host.close();
  });

  it('creates a standalone worker provider for composed clients', async () => {
    const signalPool = new FakeSignalPool();
    const proxy = new FakeProxy();
    const controller = new FakeController();
    const host = new ManagedWebRTCMeshHost({
      createSignalPool: () => signalPool as unknown as any,
      createProxy: (_onEvent, uploadLimit) => {
        proxy.setUploadLimitBytesPerSecond(uploadLimit);
        return proxy as unknown as any;
      },
      createController: () => controller as unknown as any,
    });

    await host.setSession({
      signature: 'session-a',
      pubkey: 'pubkey-a',
      relayUrls: ['wss://relay.example'],
      localStore: {} as any,
      sendSignaling: async () => undefined,
      unwrapGift: async () => null,
    });

    const provider = host.createWorkerP2PProvider();
    await expect(provider.listPeerIds()).resolves.toEqual(['peer-a']);
    await expect(provider.fetch('11'.repeat(32), 'peer-a')).resolves.toEqual(new Uint8Array([4, 5, 6]));
    await expect(provider.fetch('22'.repeat(32))).resolves.toEqual(new Uint8Array([1, 2, 3]));

    await host.close();
  });

  it('replaces active sessions cleanly when the signature changes', async () => {
    const signalPools: FakeSignalPool[] = [];
    const proxies: FakeProxy[] = [];
    const controllers: FakeController[] = [];
    const closeLocalStoreA = vi.fn();
    const closeLocalStoreB = vi.fn();
    const host = new ManagedWebRTCMeshHost({
      createSignalPool: () => {
        const pool = new FakeSignalPool();
        signalPools.push(pool);
        return pool as unknown as any;
      },
      createProxy: (_onEvent, uploadLimit) => {
        const proxy = new FakeProxy();
        proxy.setUploadLimitBytesPerSecond(uploadLimit);
        proxies.push(proxy);
        return proxy as unknown as any;
      },
      createController: () => {
        const controller = new FakeController();
        controllers.push(controller);
        return controller as unknown as any;
      },
    });

    host.setUploadLimitBytesPerSecond(321);
    host.setPoolConfig({
      follows: { max: 2, satisfied: 1 },
      other: { max: 1, satisfied: 1 },
    });

    await host.setSession({
      signature: 'session-a',
      pubkey: 'pubkey-a',
      relayUrls: ['wss://relay-a.example'],
      localStore: {} as any,
      closeLocalStore: closeLocalStoreA,
      sendSignaling: async () => undefined,
      unwrapGift: async () => null,
    });
    await host.setSession({
      signature: 'session-a',
      pubkey: 'pubkey-a',
      relayUrls: ['wss://relay-a.example'],
      localStore: {} as any,
      closeLocalStore: closeLocalStoreA,
      sendSignaling: async () => undefined,
      unwrapGift: async () => null,
    });

    expect(controllers).toHaveLength(1);
    expect(controllers[0]?.loadPeerMetadataCalls).toBe(1);
    expect(proxies[0]?.uploadLimit).toBe(321);
    expect(controllers[0]?.poolConfigCalls).toEqual([{
      follows: { max: 2, satisfied: 1 },
      other: { max: 1, satisfied: 1 },
    }]);

    host.setPoolConfig(null);
    expect(controllers[0]?.poolConfigCalls.at(-1)).toBeNull();

    await host.setSession({
      signature: 'session-b',
      pubkey: 'pubkey-b',
      relayUrls: ['wss://relay-b.example'],
      localStore: {} as any,
      closeLocalStore: closeLocalStoreB,
      sendSignaling: async () => undefined,
      unwrapGift: async () => null,
    });

    expect(controllers).toHaveLength(2);
    expect(controllers[0]?.persistPeerMetadataCalls).toBe(1);
    expect(controllers[0]?.stopped).toBe(true);
    expect(proxies[0]?.closed).toBe(true);
    expect(signalPools[0]?.destroyed).toBe(true);
    expect(signalPools[0]?.subscriptions.every((subscription) => subscription.closed)).toBe(true);
    expect(closeLocalStoreA).toHaveBeenCalledTimes(1);
    expect(closeLocalStoreB).not.toHaveBeenCalled();

    await host.close();
    expect(controllers[1]?.persistPeerMetadataCalls).toBe(1);
    expect(controllers[1]?.stopped).toBe(true);
    expect(proxies[1]?.closed).toBe(true);
    expect(signalPools[1]?.destroyed).toBe(true);
    expect(closeLocalStoreB).toHaveBeenCalledTimes(1);
  });

  it('reannounces instead of restarting while a relay is connected but no peers have connected yet', async () => {
    vi.useFakeTimers();
    try {
      const signalPools: FakeSignalPool[] = [];
      const controllers: FakeController[] = [];
      const host = new ManagedWebRTCMeshHost({
        healthCheckIntervalMs: 10,
        reannounceIntervalMs: 20,
        restartIntervalMs: 40,
        createSignalPool: () => {
          const pool = new FakeSignalPool();
          pool.connectionStatus.set('wss://relay.example', true);
          signalPools.push(pool);
          return pool as unknown as any;
        },
        createProxy: () => new FakeProxy() as unknown as any,
        createController: () => {
          const controller = new FakeController();
          controllers.push(controller);
          return controller as unknown as any;
        },
      });

      await host.setSession({
        signature: 'session-a',
        pubkey: 'pubkey-a',
        relayUrls: ['wss://relay.example'],
        localStore: {} as any,
        sendSignaling: async () => undefined,
        unwrapGift: async () => null,
      });

      expect(controllers).toHaveLength(1);
      await vi.advanceTimersByTimeAsync(120);

      expect(controllers).toHaveLength(1);
      expect(controllers[0]?.stopped).toBe(false);
      expect(controllers[0]?.broadcastHelloCalls).toBeGreaterThan(0);

      await host.close();
      expect(signalPools[0]?.destroyed).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });
});
