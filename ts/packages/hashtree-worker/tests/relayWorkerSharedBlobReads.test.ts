import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { HashTree, MemoryStore, sha256, toHex } from '@hashtree/core';
import { getPublicKey } from 'nostr-tools/pure';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import type { WorkerRequest, WorkerResponse } from '../src/relay/protocol.js';

const SERVER = 'https://blobs.test';
const pubkey = getPublicKey(new Uint8Array(32).fill(9));
const responses: WorkerResponse[] = [];
const transfers: Transferable[][] = [];
const scope = {
  onmessage: null as null | ((event: MessageEvent<WorkerRequest>) => Promise<void>),
  postMessage: (message: WorkerResponse, options?: { transfer?: Transferable[] }) => {
    responses.push(message);
    if (options?.transfer) transfers.push(options.transfer);
  },
};
const remoteBlobs = new Map<string, Uint8Array>();
const downloads = new Map<string, number>();
let storeName: string;
let sequence = 0;

async function request(payload: Record<string, unknown>): Promise<WorkerResponse | undefined> {
  const id = `shared-${++sequence}`;
  await scope.onmessage!({ data: { ...payload, id } } as MessageEvent<WorkerRequest>);
  return responses.find((message) => 'id' in message && message.id === id);
}

async function addRemoteBlob(data: Uint8Array): Promise<Uint8Array> {
  const hash = await sha256(data);
  remoteBlobs.set(toHex(hash), data);
  return hash;
}

function resultBytes(response: WorkerResponse | undefined): Uint8Array {
  if (response?.type !== 'result' || !(response.data instanceof Uint8Array)) {
    throw new Error(`Expected blob result, got ${JSON.stringify(response)}`);
  }
  return response.data;
}

beforeAll(async () => {
  vi.stubGlobal('self', scope);
  vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const match = /^https:\/\/blobs\.test\/([0-9a-f]{64})\.bin$/.exec(url);
    if (!match || (init?.method && init.method !== 'GET')) {
      return new Response(null, { status: 404 });
    }
    downloads.set(match[1], (downloads.get(match[1]) ?? 0) + 1);
    await new Promise((resolve) => setTimeout(resolve, 25));
    const data = remoteBlobs.get(match[1]);
    return data
      ? new Response(data.slice(), { status: 200, headers: { 'content-type': 'application/octet-stream' } })
      : new Response(null, { status: 404 });
  }));
  await import('../src/relay/worker.js');
});

beforeEach(async () => {
  responses.length = 0;
  transfers.length = 0;
  remoteBlobs.clear();
  downloads.clear();
  storeName = `shared-blob-reads-${crypto.randomUUID()}`;
  const result = await request({
    type: 'init',
    config: { storeName, pubkey, relays: [], blossomServers: [{ url: SERVER, read: true, write: false }] },
  });
  expect(result?.type).not.toBe('error');
});

afterEach(async () => {
  await request({ type: 'close' });
  await Dexie.delete(storeName);
  await Dexie.delete(`${storeName}-nostr-index`);
});

afterAll(() => { vi.unstubAllGlobals(); });

describe('relay worker shared blob reads', () => {
  it('downloads a blob once for concurrent reads and gives every caller its own bytes', async () => {
    const data = Uint8Array.from({ length: 4_096 }, (_, index) => (index * 7) % 256);
    const hash = await addRemoteBlob(data);

    const results = await Promise.all(
      Array.from({ length: 11 }, () => request({ type: 'get', hash })),
    );

    expect(downloads.get(toHex(hash))).toBe(1);
    const buffers = new Set<ArrayBufferLike>();
    for (const response of results) {
      const bytes = resultBytes(response);
      expect(bytes).toEqual(data);
      buffers.add(bytes.buffer);
    }
    expect(buffers.size).toBe(results.length);
    expect(transfers).toHaveLength(results.length);
  });

  it('shares one download between raw reads and file reads of the same block', async () => {
    const source = new HashTree({ store: new MemoryStore(), chunkSize: 512 });
    const content = Uint8Array.from({ length: 4 * 512 }, (_, index) => (index * 13 + (index >> 9)) % 251);
    const sourceStore = (source as unknown as { store: MemoryStore }).store;
    const { cid } = await source.putFile(content, { unencrypted: true });
    const node = await source.getTreeNode(cid);
    remoteBlobs.set(toHex(cid.hash), (await sourceStore.get(cid.hash))!);
    for (const link of node!.links) {
      remoteBlobs.set(toHex(link.hash), (await sourceStore.get(link.hash))!);
    }

    const results = await Promise.all([
      request({ type: 'readFile', cid }),
      request({ type: 'readFile', cid }),
      request({ type: 'readFileRange', cid, start: 100, end: 1_900 }),
      request({ type: 'get', hash: node!.links[1].hash }),
    ]);

    expect(resultBytes(results[0])).toEqual(content);
    expect(resultBytes(results[1])).toEqual(content);
    expect(resultBytes(results[2])).toEqual(content.slice(100, 1_900));
    expect(resultBytes(results[3])).toEqual(content.slice(512, 1_024));
    expect(Object.fromEntries(downloads)).toEqual(
      Object.fromEntries([cid.hash, ...node!.links.map((link) => link.hash)].map((hash) => [toHex(hash), 1])),
    );
  });

  it('shares a miss between concurrent callers without remembering it', async () => {
    const data = Uint8Array.from({ length: 64 }, (_, index) => index);
    const hash = await sha256(data);
    const otherHash = await sha256(Uint8Array.from([1, 2, 3]));
    await request({ type: 'get', hash: otherHash });
    const requestsPerMiss = downloads.get(toHex(otherHash));
    expect(requestsPerMiss).toBeGreaterThan(0);

    const missed = await Promise.all([
      request({ type: 'get', hash }),
      request({ type: 'get', hash }),
      request({ type: 'get', hash }),
    ]);
    expect(downloads.get(toHex(hash))).toBe(requestsPerMiss);
    for (const response of missed) {
      expect(response).toMatchObject({ type: 'result', data: undefined });
    }

    remoteBlobs.set(toHex(hash), data);
    expect(resultBytes(await request({ type: 'get', hash }))).toEqual(data);
    expect(downloads.get(toHex(hash))).toBe(requestsPerMiss! + 1);
  });
});
