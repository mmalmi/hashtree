import { describe, expect, it, vi } from 'vitest';
import { HashtreeWorkerClient } from '../src/client.js';
import { createWebRTCWorkerP2PProvider } from '../src/p2p/clientBridge.js';
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
    // no-op for tests
  }

  emitMessage(message: WorkerResponse): void {
    this.onmessage?.({ data: message } as MessageEvent<WorkerResponse>);
  }
}

async function flushMicrotasks(): Promise<void> {
  for (let idx = 0; idx < 6; idx += 1) {
    await Promise.resolve();
  }
  await new Promise((resolve) => setTimeout(resolve, 0));
}

describe('createWebRTCWorkerP2PProvider', () => {
  it('bridges peer-specific fetches and peer-list requests through the controller', async () => {
    const worker = new FakeWorker();
    const WorkerFactory = class {
      constructor() {
        return worker;
      }
    } as unknown as new () => Worker;
    const client = new HashtreeWorkerClient(WorkerFactory);
    const controller = {
      get: async (_hash: Uint8Array) => new Uint8Array([1, 2, 3]),
      getFromPeer: async (peerId: string, _hash: Uint8Array) => (
        peerId === 'peer-a' ? new Uint8Array([4, 5, 6]) : null
      ),
      getConnectedHashGetPeerIds: () => ['peer-a', 'peer-b'],
    };

    client.setP2PProvider(createWebRTCWorkerP2PProvider({
      getController: () => controller as any,
    }));
    await client.init();

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

    const peerListResult = worker.postedMessages
      .map((entry) => entry.message)
      .find((message): message is Extract<WorkerRequest, { type: 'p2pPeerListResult' }> => (
        message.type === 'p2pPeerListResult' && message.requestId === 'peers-1'
      ));
    expect(peerListResult).toEqual({
      type: 'p2pPeerListResult',
      id: expect.any(String),
      requestId: 'peers-1',
      peerIds: ['peer-a', 'peer-b'],
    });

    const peerFetchResult = worker.postedMessages
      .map((entry) => entry.message)
      .find((message): message is Extract<WorkerRequest, { type: 'p2pFetchResult' }> => (
        message.type === 'p2pFetchResult' && message.requestId === 'req-1'
      ));
    expect(peerFetchResult).toEqual({
      type: 'p2pFetchResult',
      id: expect.any(String),
      requestId: 'req-1',
      data: new Uint8Array([4, 5, 6]),
    });

    const routedFetchResult = worker.postedMessages
      .map((entry) => entry.message)
      .find((message): message is Extract<WorkerRequest, { type: 'p2pFetchResult' }> => (
        message.type === 'p2pFetchResult' && message.requestId === 'req-2'
      ));
    expect(routedFetchResult).toEqual({
      type: 'p2pFetchResult',
      id: expect.any(String),
      requestId: 'req-2',
      data: new Uint8Array([1, 2, 3]),
    });

    await client.close();
  });

  it('can skip fetches while still exposing the live peer list', async () => {
    const worker = new FakeWorker();
    const WorkerFactory = class {
      constructor() {
        return worker;
      }
    } as unknown as new () => Worker;
    const client = new HashtreeWorkerClient(WorkerFactory);
    let fetchAllowed = false;
    const controller = {
      get: async () => new Uint8Array([9]),
      getFromPeer: async () => new Uint8Array([8]),
      getConnectedHashGetPeerIds: () => ['peer-a'],
    };

    client.setP2PProvider(createWebRTCWorkerP2PProvider({
      getController: () => controller as any,
      canFetch: () => fetchAllowed,
    }));
    await client.init();

    worker.emitMessage({
      type: 'p2pPeerList',
      requestId: 'peers-2',
    });
    worker.emitMessage({
      type: 'p2pFetch',
      requestId: 'req-3',
      hashHex: '33'.repeat(32),
      peerId: 'peer-a',
    });
    await flushMicrotasks();

    const guardedPeerListResult = worker.postedMessages
      .map((entry) => entry.message)
      .find((message): message is Extract<WorkerRequest, { type: 'p2pPeerListResult' }> => (
        message.type === 'p2pPeerListResult' && message.requestId === 'peers-2'
      ));
    expect(guardedPeerListResult).toEqual({
      type: 'p2pPeerListResult',
      id: expect.any(String),
      requestId: 'peers-2',
      peerIds: ['peer-a'],
    });

    const skippedFetchResult = worker.postedMessages
      .map((entry) => entry.message)
      .find((message): message is Extract<WorkerRequest, { type: 'p2pFetchResult' }> => (
        message.type === 'p2pFetchResult' && message.requestId === 'req-3'
      ));
    expect(skippedFetchResult).toEqual({
      type: 'p2pFetchResult',
      id: expect.any(String),
      requestId: 'req-3',
    });

    fetchAllowed = true;
    worker.emitMessage({
      type: 'p2pFetch',
      requestId: 'req-4',
      hashHex: '44'.repeat(32),
      peerId: 'peer-a',
    });
    await flushMicrotasks();

    const resumedFetch = worker.postedMessages.find((entry) => (
      entry.message.type === 'p2pFetchResult' && entry.message.requestId === 'req-4'
    ));
    expect(resumedFetch?.message).toEqual({
      type: 'p2pFetchResult',
      id: expect.any(String),
      requestId: 'req-4',
      data: new Uint8Array([8]),
    });
    expect(resumedFetch?.transfer).toHaveLength(1);

    await client.close();
  });

  it('waits briefly for a generic fetch when peers are still reconnecting', async () => {
    vi.useFakeTimers();
    try {
      let connectedPeerIds: string[] = [];
      let genericFetchCalls = 0;
      const provider = createWebRTCWorkerP2PProvider({
        getController: () => ({
          get: async (_hash: Uint8Array) => {
            genericFetchCalls += 1;
            return new Uint8Array([7, 7, 7]);
          },
          getFromPeer: async () => null,
          getConnectedHashGetPeerIds: () => connectedPeerIds,
        }) as any,
        startupPeerWaitMs: 500,
        peerPollIntervalMs: 50,
      });

      const pendingFetch = provider.fetch('55'.repeat(32));
      await vi.advanceTimersByTimeAsync(200);
      expect(genericFetchCalls).toBe(0);

      connectedPeerIds = ['peer-a'];
      await vi.advanceTimersByTimeAsync(50);

      await expect(pendingFetch).resolves.toEqual(new Uint8Array([7, 7, 7]));
      expect(genericFetchCalls).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it('waits briefly for the controller to start before giving up on a generic fetch', async () => {
    vi.useFakeTimers();
    try {
      let controller: {
        get: (_hash: Uint8Array) => Promise<Uint8Array | null>;
        getFromPeer: (_peerId: string, _hash: Uint8Array) => Promise<Uint8Array | null>;
        getConnectedHashGetPeerIds: () => string[];
      } | null = null;
      const provider = createWebRTCWorkerP2PProvider({
        getController: () => controller as any,
        ensureController: async () => controller as any,
        startupPeerWaitMs: 500,
        peerPollIntervalMs: 50,
      });

      const pendingFetch = provider.fetch('66'.repeat(32));
      await vi.advanceTimersByTimeAsync(200);

      controller = {
        get: async (_hash: Uint8Array) => new Uint8Array([6, 6, 6]),
        getFromPeer: async () => null,
        getConnectedHashGetPeerIds: () => ['peer-a'],
      };
      await vi.advanceTimersByTimeAsync(50);

      await expect(pendingFetch).resolves.toEqual(new Uint8Array([6, 6, 6]));
    } finally {
      vi.useRealTimers();
    }
  });
});
