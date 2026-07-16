import { afterEach, describe, expect, it, vi } from 'vitest';
import type {
  PeerStats,
  SignedEvent,
  UnsignedEvent,
  WorkerRequest,
  WorkerResponse,
} from '../src/relay/protocol.js';
import { RelayWorkerClient, type TreeRootInfo, type TreeRootUpdate } from '../src/relay-client.js';

const ROOT_INFO: TreeRootInfo = {
  hash: Uint8Array.from({ length: 32 }, (_, index) => index + 1),
  key: Uint8Array.from({ length: 32 }, (_, index) => 255 - index),
  visibility: 'link-visible',
  labels: ['sites'],
  updatedAt: 1700000000,
  snapshotNhash: 'nhash1snapshot',
  encryptedKey: 'ab'.repeat(32),
};

class FakeRelayWorker {
  onmessage: ((event: MessageEvent<WorkerResponse>) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  readonly messages: WorkerRequest[] = [];

  postMessage(message: WorkerRequest, _transfer?: Transferable[]): void {
    this.messages.push(message);

    if (message.type === 'init') {
      this.emit({ type: 'ready' });
      return;
    }

    if (message.type === 'getTreeRootInfo') {
      this.emit({ type: 'treeRootInfo', id: message.id, record: ROOT_INFO });
      return;
    }

    if (message.type === 'getPeerStats') {
      this.emit({
        type: 'peerStats',
        id: message.id,
        stats: [{
          peerId: 'peer-a',
          pubkey: '22'.repeat(32),
          connected: true,
          pool: 'follows',
          requestsSent: 1,
          requestsReceived: 2,
          responsesSent: 3,
          responsesReceived: 4,
          bytesSent: 5,
          bytesReceived: 6,
          forwardedRequests: 7,
          forwardedResolved: 8,
          forwardedSuppressed: 9,
        }] satisfies PeerStats[],
      });
      return;
    }

    if (message.type === 'subscribeTreeRoots') {
      this.emit({ type: 'void', id: message.id });
      queueMicrotask(() => {
        this.emit({
          type: 'treeRootUpdate',
          npub: message.pubkey,
          treeName: 'sites/example',
          ...ROOT_INFO,
        } satisfies TreeRootUpdate);
      });
      return;
    }

    if (message.type === 'unsubscribeTreeRoots' || message.type === 'close') {
      this.emit({ type: 'void', id: message.id });
    }
  }

  terminate(): void {
    // no-op
  }

  emit(message: WorkerResponse): void {
    this.onmessage?.({ data: message } as MessageEvent<WorkerResponse>);
  }
}

class DelayedReadyRelayWorker extends FakeRelayWorker {
  override postMessage(message: WorkerRequest, transfer?: Transferable[]): void {
    if (message.type === 'init') {
      this.messages.push(message);
      return;
    }
    super.postMessage(message, transfer);
  }
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('RelayWorkerClient', () => {
  it('returns tree root info through the relay worker protocol', async () => {
    const client = new RelayWorkerClient(FakeRelayWorker as unknown as new () => Worker, {
      storeName: 'demo-sites-worker',
      relays: ['wss://relay.example'],
      blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
      pubkey: '11'.repeat(32),
    });

    await expect(client.getTreeRootInfo('npub1example', 'sites/example')).resolves.toEqual(ROOT_INFO);

    await client.close();
  });

  it('emits tree root updates from the worker', async () => {
    const client = new RelayWorkerClient(FakeRelayWorker as unknown as new () => Worker, {
      storeName: 'demo-sites-worker',
      relays: ['wss://relay.example'],
      blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
      pubkey: '11'.repeat(32),
    });
    const updates: TreeRootUpdate[] = [];

    const unsubscribe = client.onTreeRootUpdate((update) => {
      updates.push(update);
    });

    await client.subscribeTreeRoots('npub1example');
    await Promise.resolve();

    expect(updates).toEqual([
      {
        npub: 'npub1example',
        treeName: 'sites/example',
        ...ROOT_INFO,
      },
    ]);

    unsubscribe();
    await client.close();
  });

  it('registers media ports without waiting for a worker response', async () => {
    const worker = new FakeRelayWorker();
    const client = new RelayWorkerClient((class {
      constructor() {
        return worker;
      }
    }) as unknown as new () => Worker, {
      storeName: 'demo-sites-worker',
      relays: ['wss://relay.example'],
      blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
      pubkey: '11'.repeat(32),
    });

    const { port1, port2 } = new MessageChannel();
    await client.registerMediaPort(port1, true);

    expect(worker.messages.at(-1)).toMatchObject({
      type: 'registerMediaPort',
      debug: true,
    });

    port2.close();
    await client.close();
  });

  it('returns peer stats through the relay worker protocol', async () => {
    const client = new RelayWorkerClient(FakeRelayWorker as unknown as new () => Worker, {
      storeName: 'demo-sites-worker',
      relays: ['wss://relay.example'],
      blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
      pubkey: '11'.repeat(32),
    });

    await expect(client.getPeerStats()).resolves.toEqual([{
      peerId: 'peer-a',
      pubkey: '22'.repeat(32),
      connected: true,
      pool: 'follows',
      requestsSent: 1,
      requestsReceived: 2,
      responsesSent: 3,
      responsesReceived: 4,
      bytesSent: 5,
      bytesReceived: 6,
      forwardedRequests: 7,
      forwardedResolved: 8,
      forwardedSuppressed: 9,
    }]);

    await client.close();
  });

  it('bridges relay worker reads through the configured FIPS provider surface', async () => {
    const worker = new FakeRelayWorker();
    const client = new RelayWorkerClient((class {
      constructor() {
        return worker;
      }
    }) as unknown as new () => Worker, {
      storeName: 'demo-sites-worker',
      relays: ['wss://relay.example'],
      pubkey: '11'.repeat(32),
    });
    const fetch = vi.fn(async (hashHex: string, peerId?: string, htl?: number) => {
      expect(hashHex).toBe('ab'.repeat(32));
      expect(peerId).toBe('fips-peer');
      expect(htl).toBe(5);
      return new Uint8Array(0);
    });
    const listPeerIds = vi.fn(async () => ['fips-peer']);
    client.setP2PProvider({ fetch, listPeerIds });

    await client.init();
    expect(worker.messages.find((message) => message.type === 'init')).toMatchObject({
      p2pProviderEnabled: true,
    });
    worker.emit({
      type: 'p2pFetch',
      requestId: 'fips-fetch',
      hashHex: 'ab'.repeat(32),
      htl: 5,
      peerId: 'fips-peer',
    });
    await vi.waitFor(() => {
      expect(worker.messages.at(-1)).toMatchObject({
        type: 'p2pFetchResult',
        requestId: 'fips-fetch',
        data: new Uint8Array(0),
      });
    });

    worker.emit({ type: 'p2pPeerList', requestId: 'fips-peers' });
    await vi.waitFor(() => {
      expect(worker.messages.at(-1)).toMatchObject({
        type: 'p2pPeerListResult',
        requestId: 'fips-peers',
        peerIds: ['fips-peer'],
      });
    });

    expect(fetch).toHaveBeenCalledOnce();
    expect(listPeerIds).toHaveBeenCalledOnce();
    await client.close();
  });

  it('replays the latest p2p provider state after relay worker initialization', async () => {
    const worker = new DelayedReadyRelayWorker();
    const client = new RelayWorkerClient((class {
      constructor() {
        return worker;
      }
    }) as unknown as new () => Worker, {
      storeName: 'demo-sites-worker',
      relays: [],
      pubkey: '11'.repeat(32),
    });
    const initializing = client.init();

    client.setP2PProvider({ fetch: async () => null, listPeerIds: () => [] });
    expect(worker.messages.filter((message) => message.type === 'setP2PProviderState')).toEqual([]);

    worker.emit({ type: 'ready' });
    await initializing;
    expect(worker.messages.filter((message) => message.type === 'setP2PProviderState'))
      .toMatchObject([{ enabled: true }]);
    await client.close();
  });

  it('bridges signEvent requests to the nostr extension', async () => {
    const worker = new FakeRelayWorker();
    const client = new RelayWorkerClient((class {
      constructor() {
        return worker;
      }
    }) as unknown as new () => Worker, {
      storeName: 'demo-sites-worker',
      relays: ['wss://relay.example'],
      blossomServers: [{ url: 'https://upload.example', read: false, write: true }],
      pubkey: '11'.repeat(32),
    });

    const signedEvent: SignedEvent = {
      id: '1'.repeat(64),
      pubkey: '11'.repeat(32),
      created_at: 1700000000,
      kind: 1,
      tags: [],
      content: 'hello',
      sig: '2'.repeat(128),
    };
    vi.stubGlobal('window', {
      nostr: {
        signEvent: async (_event: UnsignedEvent) => signedEvent,
      },
    });

    await client.init();
    worker.emit({
      type: 'signEvent',
      id: 'sign-request',
      event: {
        created_at: 1700000000,
        kind: 1,
        tags: [],
        content: 'hello',
      },
    });
    await Promise.resolve();

    expect(worker.messages.at(-1)).toEqual({
      type: 'signed',
      id: 'sign-request',
      event: signedEvent,
    });

    await client.close();
  });
});
