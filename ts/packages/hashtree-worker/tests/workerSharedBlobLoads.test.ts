import 'fake-indexeddb/auto';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { sha256, toHex } from '@hashtree/core';
import { HashtreeWorkerClient } from '../src/client.js';
import { attachHashtreeWorker } from '../src/worker.js';
import type { WorkerRequest, WorkerResponse } from '../src/protocol.js';

class InProcessWorker {
  onmessage: ((event: MessageEvent<WorkerResponse>) => void) | null = null;
  onerror = null;
  private listener: EventListener | null = null;
  private readonly detach = attachHashtreeWorker({
    postMessage: (message) => {
      this.onmessage?.({ data: message } as MessageEvent<WorkerResponse>);
    },
    addEventListener: (_type, listener) => { this.listener = listener as EventListener; },
    removeEventListener: () => { this.listener = null; },
  });
  postMessage(message: WorkerRequest) {
    this.listener?.({ data: message } as MessageEvent<WorkerRequest>);
  }
  terminate() { this.detach(); }
}

const payload = Uint8Array.from({ length: 2_048 }, (_, index) => (index * 5) % 251);
const fetch = vi.fn();
let client: HashtreeWorkerClient;
let hashHex: string;
let peerFetches: number;
let blossomFetches: number;

beforeEach(async () => {
  peerFetches = 0;
  blossomFetches = 0;
  hashHex = toHex(await sha256(payload));
  fetch.mockReset().mockImplementation(async (_url: string, init?: RequestInit) => {
    if (init?.method === 'HEAD') return new Response(null, { status: 404 });
    blossomFetches++;
    await new Promise((resolve) => setTimeout(resolve, 25));
    return new Response(payload.slice().buffer);
  });
  vi.stubGlobal('fetch', fetch);
  const worker = new InProcessWorker();
  client = new HashtreeWorkerClient((class {
    constructor() { return worker; }
  }) as unknown as new () => Worker, {
    storeName: `shared-blob-loads-${crypto.randomUUID()}`,
    relays: [],
    blossomServers: [{ url: 'https://blossom.example', read: true }],
  });
  client.setP2PProvider({
    fetch: async () => {
      peerFetches++;
      await new Promise((resolve) => setTimeout(resolve, 25));
      return payload.slice();
    },
    listPeerIds: () => ['peer'],
  });
  await client.init();
});

afterEach(async () => {
  await client.close();
  vi.unstubAllGlobals();
});

describe('worker shared blob loads', () => {
  it('asks peers once for concurrent identical reads', async () => {
    const results = await Promise.all(
      Array.from({ length: 6 }, () => client.getBlob(hashHex, { sourceIds: ['p2p'] })),
    );

    expect(peerFetches).toBe(1);
    expect(blossomFetches).toBe(0);
    for (const result of results) {
      expect(result).toEqual({ data: payload, source: 'p2p' });
    }
  });

  it('keeps reads with different source selections separate', async () => {
    const [fromPeer, fromBlossom] = await Promise.all([
      client.getBlob(hashHex, { sourceIds: ['p2p'], skipPrimary: true }),
      client.getBlob(hashHex, { sourceIds: ['blossom'], skipPrimary: true }),
    ]);

    expect(fromPeer).toEqual({ data: payload, source: 'p2p' });
    expect(fromBlossom).toEqual({ data: payload, source: 'blossom' });
    expect(peerFetches).toBe(1);
    expect(blossomFetches).toBe(1);
  });
});
