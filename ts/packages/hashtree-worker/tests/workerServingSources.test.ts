import 'fake-indexeddb/auto';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { sha256, toHex } from '@hashtree/core';
import { HashtreeWorkerClient } from '../src/client.js';
import { IdbBlobStorage } from '../src/capabilities/idbStorage.js';
import { attachHashtreeWorker } from '../src/worker.js';
import type { WorkerRequest, WorkerResponse } from '../src/protocol.js';

class InProcessWorker {
  onmessage: ((event: MessageEvent<WorkerResponse>) => void) | null = null;
  onerror = null;
  readonly requests: WorkerRequest[] = [];
  readonly responses: WorkerResponse[] = [];
  private listener: EventListener | null = null;
  private readonly detach = attachHashtreeWorker({
    postMessage: (message) => {
      this.responses.push(message as WorkerResponse);
      this.onmessage?.({ data: message } as MessageEvent<WorkerResponse>);
    },
    addEventListener: (_type, listener) => { this.listener = listener as EventListener; },
    removeEventListener: () => { this.listener = null; },
  });
  postMessage(message: WorkerRequest) {
    this.requests.push(message);
    this.listener?.({ data: message } as MessageEvent<WorkerRequest>);
  }
  terminate() { this.detach(); }
}

let client: HashtreeWorkerClient;
let worker: InProcessWorker;
let storage: IdbBlobStorage;
let upstream: Uint8Array | null;
let upstreamError: Error | null;
const payload = new Uint8Array([10, 20, 30]);
const fetch = vi.fn();

beforeEach(async () => {
  upstream = null;
  upstreamError = null;
  fetch.mockReset().mockImplementation(async (_url: string, init?: RequestInit) => {
    if (init?.method === 'HEAD') return new Response(null, { status: 404 });
    if (upstreamError) throw upstreamError;
    return upstream ? new Response(upstream.slice().buffer) : new Response(null, { status: 404 });
  });
  vi.stubGlobal('fetch', fetch);
  const storeName = `peer-serving-${crypto.randomUUID()}`;
  storage = new IdbBlobStorage(storeName, 1024 * 1024);
  worker = new InProcessWorker();
  client = new HashtreeWorkerClient((class {
    constructor() { return worker; }
  }) as unknown as new () => Worker, {
    storeName, relays: [], blossomServers: [{ url: 'https://blossom.example', read: true }],
  });
  client.setP2PProvider({ fetch: async () => null, listPeerIds: () => ['peer'] });
  await client.init();
});

afterEach(async () => {
  await client.close();
  storage.close();
  vi.unstubAllGlobals();
});

describe('worker peer-serving source scope', () => {
  it('refuses an untrusted local block without treating denial as an operational error', async () => {
    const hash = toHex(await sha256(payload));
    await storage.putByHash(hash, payload);
    await expect(client.getBlobForPeer(hash, { sourceIds: ['blossom'] })).resolves.toBeNull();
    expect(worker.responses.find((message) => message.type === 'blob')).toEqual({
      type: 'blob', id: expect.any(String),
    });
    expect(worker.responses.some((message) => message.type === 'p2pFetch')).toBe(false);
  });

  it('serves an uncached hash only after the allowed upstream returns verified bytes', async () => {
    upstream = payload;
    const hash = toHex(await sha256(payload));
    await expect(client.getBlobForPeer(hash, { sourceIds: ['blossom'] })).resolves.toEqual(payload);
    await expect(storage.get(hash)).resolves.toEqual(payload);
    expect(worker.responses.some((message) => message.type === 'p2pFetch')).toBe(false);
  });

  it('does not recurse into P2P for a trusted hash missing from the local cache', async () => {
    upstream = payload;
    const hashHex = toHex(await sha256(payload));
    await expect(client.getBlobForPeer(hashHex)).resolves.toEqual(payload);
    upstream = null;
    // Retain the worker's proven read-source authorization while removing cached bytes.
    const { DexieStore } = await import('@hashtree/dexie');
    const request = worker.requests.find((message) => message.type === 'init');
    if (request?.type !== 'init') throw new Error('Expected init');
    const raw = new DexieStore(request.config.storeName!);
    try {
      const { fromHex } = await import('@hashtree/core');
      await raw.delete(fromHex(hashHex));
    } finally { raw.close(); }
    await expect(client.getBlobForPeer(hashHex, { sourceIds: ['blossom'] })).resolves.toBeNull();
    expect(worker.responses.filter((message) => ['p2pFetch', 'p2pPeerList'].includes(message.type))).toEqual([]);
  });

  it('does not widen an explicit empty scope to the default upstream source', async () => {
    upstream = payload;
    await expect(client.getBlobForPeer(toHex(await sha256(payload)), { sourceIds: [] })).resolves.toBeNull();
    expect(fetch.mock.calls.filter(([, init]) => init?.method !== 'HEAD')).toEqual([]);
  });

  it('preserves an upstream failure instead of reporting an authenticated miss', async () => {
    upstreamError = new DOMException('upstream timed out', 'TimeoutError');
    await expect(client.getBlobForPeer(toHex(await sha256(payload)), { sourceIds: ['blossom'] }))
      .rejects.toThrow(/incomplete|timed out/i);
  });

  it('creates a local encrypted file that remains safely peer-readable after worker restart', async () => {
    const { HashTree, MemoryStore, nhashDecode } = await import('@hashtree/core');
    const file = await client.putFile(payload, { upload: false });
    const cid = nhashDecode(file.nhash);
    expect(cid.key).toHaveLength(32);
    const ciphertext = await client.getBlobForPeer(file.hashHex, { sourceIds: [] });
    expect(ciphertext).not.toBeNull();
    expect(ciphertext).not.toEqual(payload);
    const init = worker.requests.find((message) => message.type === 'init');
    if (init?.type !== 'init') throw new Error('Expected init');
    await client.close();
    worker = new InProcessWorker();
    client = new HashtreeWorkerClient((class {
      constructor() { return worker; }
    }) as unknown as new () => Worker, init.config);
    await client.init();
    const remoteStore = new MemoryStore();
    remoteStore.get = (hash) => client.getBlobForPeer(toHex(hash), { sourceIds: [] });
    const reader = new HashTree({ store: remoteStore });
    await expect(reader.readFile(cid)).resolves.toEqual(payload);
    expect(fetch.mock.calls.filter(([, init]) => init?.method !== 'HEAD')).toEqual([]);
  });

  it('keeps the existing raw local putBlob operation unencrypted and unshared', async () => {
    const raw = await client.putBlob(payload, undefined, false);
    expect(raw.hashHex).toBe(toHex(await sha256(payload)));
    await expect(client.getBlobForPeer(raw.hashHex, { sourceIds: [] })).resolves.toBeNull();
  });

});
