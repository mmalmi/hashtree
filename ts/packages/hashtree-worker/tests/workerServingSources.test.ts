import 'fake-indexeddb/auto';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fromHex, HashTree, MemoryStore, sha256, toHex } from '@hashtree/core';
import { HashtreeWorkerClient } from '../src/client.js';
import { IdbBlobStorage } from '../src/capabilities/idbStorage.js';
import { attachHashtreeWorker, type HashtreeWorkerRuntime } from '../src/worker.js';
import type { WorkerRequest, WorkerResponse } from '../src/protocol.js';

class InProcessWorker {
  onmessage: ((event: MessageEvent<WorkerResponse>) => void) | null = null;
  onerror = null;
  readonly requests: WorkerRequest[] = [];
  readonly responses: WorkerResponse[] = [];
  runtime: HashtreeWorkerRuntime | null = null;
  private listener: EventListener | null = null;
  private readonly detach = attachHashtreeWorker({
    postMessage: (message) => {
      this.responses.push(message as WorkerResponse);
      this.onmessage?.({ data: message } as MessageEvent<WorkerResponse>);
    },
    addEventListener: (_type, listener) => { this.listener = listener as EventListener; },
    removeEventListener: () => { this.listener = null; },
  }, {
    handleExtensionRequest: (_request, runtime) => { this.runtime = runtime; return false; },
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

async function restartWorker(): Promise<void> {
  const init = worker.requests.find((message) => message.type === 'init');
  if (init?.type !== 'init') throw new Error('Expected init');
  await client.close();
  worker = new InProcessWorker();
  client = new HashtreeWorkerClient((class {
    constructor() { return worker; }
  }) as unknown as new () => Worker, init.config);
  await client.init();
}

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
  it.each(['p2p', 'blossom'] as const)('keeps a verified %s download peer-readable after the source disappears and worker restarts', async (source) => {
    const hash = toHex(await sha256(payload));
    if (source === 'blossom') upstream = payload;
    else client.setP2PProvider({ fetch: async () => payload });
    await expect(client.getBlob(hash, { sourceIds: [source] })).resolves.toEqual({ data: payload, source });
    await expect(storage.get(hash)).resolves.toEqual(payload);
    upstream = null;
    client.setP2PProvider(null);
    await expect(client.getBlobForPeer(hash, { sourceIds: [] })).resolves.toEqual(payload);
    await restartWorker();
    await expect(client.getBlobForPeer(hash, { sourceIds: [] })).resolves.toEqual(payload);
  });

  it('persists sharing for every downloaded encrypted tree block, without sharing local raw blocks', async () => {
    const privateBlock = await client.putBlock(payload);
    const originStore = new MemoryStore();
    const origin = new HashTree({ store: originStore, chunkSize: 64 });
    const contents = Uint8Array.from({ length: 256 }, (_, index) => index);
    const { cid } = await origin.putFile(contents);
    client.setP2PProvider({ fetch: (hashHex) => originStore.get(fromHex(hashHex)) });
    await expect(worker.runtime!.tree!.readFile(cid)).resolves.toEqual(contents);
    const blocks = [];
    for await (const block of origin.walkBlocks(cid)) blocks.push(block);
    expect(blocks.length).toBeGreaterThan(1);
    client.setP2PProvider(null);
    await restartWorker();
    for (const block of blocks) {
      const hash = toHex(block.hash);
      await expect(client.getBlobForPeer(hash, { sourceIds: [] })).resolves.toEqual(await originStore.get(block.hash));
    }
    await expect(client.getBlobForPeer(privateBlock.hashHex, { sourceIds: [] })).resolves.toBeNull();
    const remoteStore = new MemoryStore();
    remoteStore.get = (hash) => client.getBlobForPeer(toHex(hash), { sourceIds: [] });
    await expect(new HashTree({ store: remoteStore }).readFile(cid)).resolves.toEqual(contents);
  });

  it.each(['miss', 'corrupt', 'timeout'] as const)('does not authorize private local bytes after a remote %s', async (outcome) => {
    const block = await client.putBlock(payload);
    client.setP2PProvider({ fetch: async () => {
      if (outcome === 'timeout') throw new DOMException('Peer timed out', 'TimeoutError');
      return outcome === 'miss' ? null : new Uint8Array([99]);
    } });
    await expect(client.getBlob(block.hashHex, { sourceIds: ['p2p'], skipPrimary: true }))
      .rejects.toThrow(outcome === 'miss' ? /not found/i : outcome === 'timeout' ? /incomplete|timed out/i : /wrong hash/i);
    await expect(storage.get(block.hashHex)).resolves.toEqual(payload);
    await expect(client.getBlobForPeer(block.hashHex, { sourceIds: [] })).resolves.toBeNull();
    await restartWorker();
    await expect(client.getBlobForPeer(block.hashHex, { sourceIds: [] })).resolves.toBeNull();
  });

  it.each(['putByHashTrusted', 'authorizePeerSharing'] as const)('still returns verified network bytes when %s fails, without an unsafe grant', async (method) => {
    const hash = toHex(await sha256(payload));
    client.setP2PProvider({ fetch: async () => payload });
    const write = vi.spyOn(IdbBlobStorage.prototype, method)
      .mockRejectedValue(new DOMException('Cache full', 'QuotaExceededError'));
    try {
      await expect(client.getBlob(hash, { sourceIds: ['p2p'] })).resolves.toEqual({ data: payload, source: 'p2p' });
      await expect(client.getBlobForPeer(hash, { sourceIds: [] })).resolves.toBeNull();
    } finally { write.mockRestore(); }
    await restartWorker();
    await expect(client.getBlobForPeer(hash, { sourceIds: [] })).resolves.toBeNull();
  });

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
    await restartWorker();
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

  it.each(['single', 'batch'] as const)('persists explicit sharing of %s raw blocks without sharing unrelated cache', async (kind) => {
    const privateData = new Uint8Array([40, 50, 60]);
    const privateBlock = await client.putBlock(privateData);
    const hashHex = toHex(await sha256(payload));
    const shared = kind === 'single'
      ? [await client.putBlock(payload, { hashHex, peerShare: true })]
      : await client.putBlocks([{ data: payload, hashHex }], { peerShare: true });
    expect(shared[0].hashHex).toBe(hashHex);
    await expect(client.getBlobForPeer(hashHex, { sourceIds: [] })).resolves.toEqual(payload);
    await restartWorker();
    await expect(client.getBlobForPeer(hashHex, { sourceIds: [] })).resolves.toEqual(payload);
    await expect(client.getBlobForPeer(privateBlock.hashHex, { sourceIds: [] })).resolves.toBeNull();
    expect(fetch.mock.calls.filter(([, init]) => init?.method !== 'HEAD')).toEqual([]);
  });

  it('does not authorize any hashes from a batch with an invalid supplied hash', async () => {
    const existing = await client.putBlock(payload);
    const other = new Uint8Array([70, 80, 90]);
    const hashHex = toHex(await sha256(other));
    await expect(client.putBlocks([
      { data: payload, hashHex: existing.hashHex },
      { data: payload, hashHex },
    ], { peerShare: true })).rejects.toThrow('Hash mismatch');
    await restartWorker();
    await expect(client.getBlobForPeer(existing.hashHex, { sourceIds: [] })).resolves.toBeNull();
    await expect(client.getBlobForPeer(hashHex, { sourceIds: [] })).resolves.toBeNull();
  });

  it('keeps a raw batch private when peer sharing is not requested', async () => {
    const [block] = await client.putBlocks([{ data: payload }]);
    await restartWorker();
    await expect(client.getBlobForPeer(block.hashHex, { sourceIds: [] })).resolves.toBeNull();
  });

});
