import { describe, expect, it, vi } from 'vitest';
import { MemoryStore, sha256, type Hash } from '@hashtree/core';
import {
  createRequest,
  createResponse,
  encodeRequest,
  encodeResponse,
} from '@hashtree/mesh';
import {
  DEFAULT_FIPS_DISCOVERY_APP,
  FIPS_RESPONSE_FRAGMENT_SIZE,
  FipsTransportStore,
  HashtreeFipsTransport,
  createFipsNodeEndpoint,
  type FipsEndpoint,
  type FipsEndpointMessage,
  type FipsNodeEndpointData,
  type FipsNodePeerEvent,
} from '../src/index.js';

class FakeFipsEndpoint implements FipsEndpoint {
  readonly sent: Array<{ peerId: string; data: Uint8Array }> = [];
  private readonly handlers = new Set<(message: FipsEndpointMessage) => void | Promise<void>>();
  private dropNextSendCount = 0;
  private sendDelayMs = 0;
  private activeSends = 0;
  maxActiveSends = 0;

  constructor(
    private readonly id: string,
    private readonly network: Map<string, FakeFipsEndpoint>,
  ) {
    this.network.set(id, this);
  }

  localPeerId(): string {
    return this.id;
  }

  listPeerIds(): string[] {
    return Array.from(this.network.keys()).filter((id) => id !== this.id);
  }

  async send(peerId: string, data: Uint8Array): Promise<void> {
    this.sent.push({ peerId, data: data.slice() });
    if (this.dropNextSendCount > 0) {
      this.dropNextSendCount -= 1;
      return;
    }
    const remote = this.network.get(peerId);
    if (!remote) {
      throw new Error(`unknown peer ${peerId}`);
    }
    this.activeSends += 1;
    this.maxActiveSends = Math.max(this.maxActiveSends, this.activeSends);
    try {
      if (this.sendDelayMs > 0) {
        await new Promise((resolve) => setTimeout(resolve, this.sendDelayMs));
      }
      await remote.deliver({ peerId: this.id, data: data.slice() });
    } finally {
      this.activeSends -= 1;
    }
  }

  dropNextSends(count: number): void {
    this.dropNextSendCount = Math.max(0, count);
  }

  setSendDelay(ms: number): void {
    this.sendDelayMs = Math.max(0, ms);
  }

  onMessage(handler: (message: FipsEndpointMessage) => void | Promise<void>): () => void {
    this.handlers.add(handler);
    return () => {
      this.handlers.delete(handler);
    };
  }

  private async deliver(message: FipsEndpointMessage): Promise<void> {
    await Promise.all(Array.from(this.handlers, (handler) => handler(message)));
  }
}

class FakeFipsNode {
  readonly sentEndpointData: Array<{
    dst: string;
    payload: Uint8Array;
  }> = [];
  readonly identity = { publicKey: new Uint8Array([2, 1, 2, 3]) };
  private readonly endpointDataHandlers = new Set<(event: FipsNodeEndpointData) => void>();
  private readonly peerHandlers = new Set<(event: FipsNodePeerEvent) => void>();

  async sendEndpointData(args: {
    dst: string;
    payload: Uint8Array;
  }): Promise<void> {
    this.sentEndpointData.push({ ...args, payload: args.payload.slice() });
  }

  on(event: 'endpointData', handler: (event: FipsNodeEndpointData) => void): () => void;
  on(event: 'peer', handler: (event: FipsNodePeerEvent) => void): () => void;
  on(
    event: 'endpointData' | 'peer',
    handler: ((event: FipsNodeEndpointData) => void) | ((event: FipsNodePeerEvent) => void),
  ): () => void {
    if (event === 'endpointData') {
      const typed = handler as (event: FipsNodeEndpointData) => void;
      this.endpointDataHandlers.add(typed);
      return () => this.endpointDataHandlers.delete(typed);
    }
    const typed = handler as (event: FipsNodePeerEvent) => void;
    this.peerHandlers.add(typed);
    return () => this.peerHandlers.delete(typed);
  }

  emitEndpointData(event: FipsNodeEndpointData): void {
    for (const handler of this.endpointDataHandlers) handler(event);
  }

  emitPeer(event: FipsNodePeerEvent): void {
    for (const handler of this.peerHandlers) handler(event);
  }
}

describe('@hashtree/fips-transport', () => {
  it('uses the hashtree FIPS discovery app scope', () => {
    expect(DEFAULT_FIPS_DISCOVERY_APP).toBe('hashtree-v1');
  });

  it('adapts FIPS node endpoint data into the endpoint surface', async () => {
    const node = new FakeFipsNode();
    const endpoint = createFipsNodeEndpoint(node, { initialPeers: ['peer-a'] });
    const messages: FipsEndpointMessage[] = [];
    const off = endpoint.onMessage((message) => {
      messages.push(message);
    });

    await endpoint.send('peer-a', new Uint8Array([1, 2, 3]));
    expect(node.sentEndpointData).toEqual([{
      dst: 'peer-a',
      payload: new Uint8Array([1, 2, 3]),
    }]);

    node.emitPeer({ remotePubkey: 'peer-b', state: 'connected' });
    expect(endpoint.listPeerIds?.()).toEqual(['peer-a', 'peer-b']);
    node.emitEndpointData({
      src: 'peer-b',
      dst: 'local',
      payload: new Uint8Array([5, 6]),
    });

    expect(messages).toEqual([{ peerId: 'peer-b', data: new Uint8Array([5, 6]) }]);
    off();
    endpoint.close?.();
  });

  it('fetches a hash-verified blob over opaque FIPS endpoint bytes', async () => {
    const network = new Map<string, FakeFipsEndpoint>();
    const aEndpoint = new FakeFipsEndpoint('a', network);
    const bEndpoint = new FakeFipsEndpoint('b', network);
    const data = new TextEncoder().encode('hashtree over fips');
    const hash = await sha256(data) as Hash;
    const aStore = new MemoryStore();
    const bStore = new MemoryStore();
    await aStore.put(hash, data);

    const aTransport = new HashtreeFipsTransport({
      endpoint: aEndpoint,
      localStore: aStore,
    });
    const bTransport = new HashtreeFipsTransport({
      endpoint: bEndpoint,
      localStore: bStore,
      peers: ['a'],
      requestTimeoutMs: 100,
    });

    await expect(bTransport.get(hash)).resolves.toEqual(data);
    await expect(bStore.get(hash)).resolves.toEqual(data);
    expect(bEndpoint.sent).toHaveLength(1);

    aTransport.close();
    bTransport.close();
  });

  it('fragments and reassembles responses that exceed the FIPS endpoint payload budget', async () => {
    const network = new Map<string, FakeFipsEndpoint>();
    const aEndpoint = new FakeFipsEndpoint('a', network);
    const bEndpoint = new FakeFipsEndpoint('b', network);
    const data = new Uint8Array(FIPS_RESPONSE_FRAGMENT_SIZE * 2 + 17);
    for (let i = 0; i < data.byteLength; i += 1) {
      data[i] = i % 251;
    }
    const hash = await sha256(data) as Hash;
    const aStore = new MemoryStore();
    const bStore = new MemoryStore();
    await aStore.put(hash, data);

    const aTransport = new HashtreeFipsTransport({
      endpoint: aEndpoint,
      localStore: aStore,
    });
    const bTransport = new HashtreeFipsTransport({
      endpoint: bEndpoint,
      localStore: bStore,
      peers: ['a'],
      requestTimeoutMs: 100,
    });

    await expect(bTransport.get(hash)).resolves.toEqual(data);
    await expect(bStore.get(hash)).resolves.toEqual(data);
    expect(aEndpoint.sent.length).toBeGreaterThan(1);

    aTransport.close();
    bTransport.close();
  });

  it('coalesces duplicate block requests and serializes response fragments', async () => {
    const network = new Map<string, FakeFipsEndpoint>();
    const aEndpoint = new FakeFipsEndpoint('a', network);
    const bEndpoint = new FakeFipsEndpoint('b', network);
    aEndpoint.setSendDelay(1);
    const data = new Uint8Array(FIPS_RESPONSE_FRAGMENT_SIZE * 3 + 17).fill(0x5a);
    const hash = await sha256(data) as Hash;
    const aStore = new MemoryStore();
    await aStore.put(hash, data);
    const aTransport = new HashtreeFipsTransport({
      endpoint: aEndpoint,
      localStore: aStore,
    });
    const request = new Uint8Array(encodeRequest(createRequest(hash, 10)));

    await Promise.all([
      bEndpoint.send('a', request),
      bEndpoint.send('a', request),
    ]);

    await vi.waitFor(() => expect(aEndpoint.sent).toHaveLength(4));
    expect(aEndpoint.maxActiveSends).toBe(1);
    aTransport.close();
  });

  it('treats silence as unknown when the retry interval exceeds the request timeout', async () => {
    vi.useFakeTimers();
    const network = new Map<string, FakeFipsEndpoint>();
    const aEndpoint = new FakeFipsEndpoint('a', network);
    const bEndpoint = new FakeFipsEndpoint('b', network);
    const missingHash = new Uint8Array(32).fill(7) as Hash;
    const aTransport = new HashtreeFipsTransport({
      endpoint: aEndpoint,
      localStore: new MemoryStore(),
    });
    const bTransport = new HashtreeFipsTransport({
      endpoint: bEndpoint,
      localStore: new MemoryStore(),
      peers: ['a'],
      requestTimeoutMs: 25,
    });

    const pending = bTransport.get(missingHash);
    await vi.advanceTimersByTimeAsync(30);
    await expect(pending).resolves.toBeNull();
    expect(bEndpoint.sent).toHaveLength(1);

    aTransport.close();
    bTransport.close();
    vi.useRealTimers();
  });

  it('retries a dropped request to the same peer', async () => {
    vi.useFakeTimers();
    const network = new Map<string, FakeFipsEndpoint>();
    const aEndpoint = new FakeFipsEndpoint('a', network);
    const bEndpoint = new FakeFipsEndpoint('b', network);
    const data = new TextEncoder().encode('retried request');
    const hash = await sha256(data) as Hash;
    const aStore = new MemoryStore();
    await aStore.put(hash, data);
    bEndpoint.dropNextSends(1);
    const aTransport = new HashtreeFipsTransport({
      endpoint: aEndpoint,
      localStore: aStore,
    });
    const bTransport = new HashtreeFipsTransport({
      endpoint: bEndpoint,
      localStore: new MemoryStore(),
      peers: ['a'],
      requestTimeoutMs: 300,
      requestRetryIntervalMs: 50,
    });

    const pending = bTransport.get(hash);
    await vi.advanceTimersByTimeAsync(60);

    await expect(pending).resolves.toEqual(data);
    expect(bEndpoint.sent).toHaveLength(2);

    aTransport.close();
    bTransport.close();
    vi.useRealTimers();
  });

  it('sends an in-flight request to peers that appear from a dynamic source', async () => {
    vi.useFakeTimers();
    const network = new Map<string, FakeFipsEndpoint>();
    const aEndpoint = new FakeFipsEndpoint('a', network);
    const bEndpoint = new FakeFipsEndpoint('b', network);
    const data = new TextEncoder().encode('late peer');
    const hash = await sha256(data) as Hash;
    const aStore = new MemoryStore();
    await aStore.put(hash, data);
    let peers: string[] = [];
    const aTransport = new HashtreeFipsTransport({
      endpoint: aEndpoint,
      localStore: aStore,
    });
    const bTransport = new HashtreeFipsTransport({
      endpoint: bEndpoint,
      localStore: new MemoryStore(),
      peers: () => peers,
      requestTimeoutMs: 300,
    });

    const pending = bTransport.get(hash);
    await vi.advanceTimersByTimeAsync(100);
    expect(bEndpoint.sent).toHaveLength(0);

    peers = ['a'];
    await vi.advanceTimersByTimeAsync(100);

    await expect(pending).resolves.toEqual(data);
    expect(bEndpoint.sent).toHaveLength(1);

    aTransport.close();
    bTransport.close();
    vi.useRealTimers();
  });

  it('ignores poisoned responses and waits for another valid source', async () => {
    vi.useFakeTimers();
    const network = new Map<string, FakeFipsEndpoint>();
    const aEndpoint = new FakeFipsEndpoint('a', network);
    const bEndpoint = new FakeFipsEndpoint('b', network);
    const data = new TextEncoder().encode('clean');
    const hash = await sha256(data) as Hash;
    const bTransport = new HashtreeFipsTransport({
      endpoint: bEndpoint,
      localStore: new MemoryStore(),
      peers: ['a'],
      requestTimeoutMs: 25,
    });

    const pending = bTransport.get(hash);
    await aEndpoint.send('b', new Uint8Array(encodeResponse(createResponse(hash, new Uint8Array([1, 2, 3])))));
    await vi.advanceTimersByTimeAsync(30);

    await expect(pending).resolves.toBeNull();
    bTransport.close();
    vi.useRealTimers();
  });

  it('provides a Store wrapper for local-first reads', async () => {
    const network = new Map<string, FakeFipsEndpoint>();
    const endpoint = new FakeFipsEndpoint('local', network);
    const localStore = new MemoryStore();
    const data = new TextEncoder().encode('local');
    const hash = await sha256(data) as Hash;
    await localStore.put(hash, data);
    const store = new FipsTransportStore({
      endpoint,
      localStore,
      peers: [],
    });

    await expect(store.get(hash)).resolves.toEqual(data);
    expect(endpoint.sent).toHaveLength(0);
    store.close();
  });
});
