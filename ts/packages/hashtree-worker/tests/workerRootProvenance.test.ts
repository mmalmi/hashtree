import 'fake-indexeddb/auto';
import Dexie from 'dexie';
import { fromHex, toHex } from '@hashtree/core';
import { finalizeEvent, getPublicKey } from 'nostr-tools/pure';
import { nip19 } from 'nostr-tools';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import type { WorkerRequest, WorkerResponse } from '../src/relay/protocol.js';
import { handleTreeRootEvent } from '../src/relay/treeRootSubscription.js';

const secret = new Uint8Array(32).fill(7);
const pubkey = getPublicKey(secret);
const npub = nip19.npubEncode(pubkey);
const oldHash = '11'.repeat(32);
const newHash = '22'.repeat(32);
const responses: WorkerResponse[] = [];
const scope = {
  onmessage: null as null | ((event: MessageEvent<WorkerRequest>) => Promise<void>),
  postMessage: (message: WorkerResponse) => { responses.push(message); },
};
let storeName: string;
let sequence = 0;

async function request(payload: Record<string, unknown>): Promise<WorkerResponse | undefined> {
  const id = `root-${++sequence}`;
  await scope.onmessage!({ data: { ...payload, id } } as MessageEvent<WorkerRequest>);
  return responses.find((message) => 'id' in message && message.id === id);
}

async function init(): Promise<void> {
  const result = await request({ type: 'init', config: { storeName, pubkey, relays: [], blossomServers: [] } });
  expect(result?.type).not.toBe('error');
}

function sync(hash: string, metadata: Record<string, unknown> = {}) {
  return request({ type: 'setTreeRootCache', npub, treeName: 'shared', hash: fromHex(hash), visibility: 'public', ...metadata });
}

async function info() {
  const response = await request({ type: 'getTreeRootInfo', npub, treeName: 'shared' });
  if (response?.type !== 'treeRootInfo') throw new Error('Expected root info');
  return response.record;
}

async function remoteEvent(hash: string, timestamp: number) {
  await handleTreeRootEvent(finalizeEvent({
    kind: 30064, created_at: timestamp, content: '',
    tags: [['d', 'shared'], ['l', 'hashtree'], ['hash', hash]],
  }, secret));
}

beforeAll(async () => {
  vi.stubGlobal('self', scope);
  await import('../src/relay/worker.js');
});

beforeEach(async () => {
  responses.length = 0;
  storeName = `root-provenance-${crypto.randomUUID()}`;
  await init();
});

afterEach(async () => {
  await request({ type: 'close' });
  await Dexie.delete(storeName);
  await Dexie.delete(`${storeName}-nostr-index`);
});

afterAll(() => { vi.unstubAllGlobals(); });

describe('relay worker root provenance', () => {
  it('retains remote event time through offline restart and accepts the next signed root', async () => {
    await sync(oldHash, { source: 'remote', updatedAt: 100 });
    expect((await info())?.updatedAt).toBe(100);
    await request({ type: 'close' });
    await init();
    expect((await info())?.updatedAt).toBe(100);
    await remoteEvent(newHash, 101);
    const latest = await info();
    expect(latest?.hash && toHex(latest.hash)).toBe(newHash);
    expect(latest?.updatedAt).toBe(101);
  });

  it('does not let stale remote hydration replace a newer signed root', async () => {
    await remoteEvent(newHash, 200);
    await sync(oldHash, { source: 'remote', updatedAt: 100 });
    const latest = await info();
    expect(latest?.hash && toHex(latest.hash)).toBe(newHash);
    expect(latest?.updatedAt).toBe(200);
  });

  it('preserves explicit local time and authority over a same-second signed root', async () => {
    await remoteEvent(oldHash, 200);
    await sync(newHash, { source: 'local-write', updatedAt: 200 });
    const latest = await info();
    expect(latest?.hash && toHex(latest.hash)).toBe(newHash);
    expect(latest?.updatedAt).toBe(200);
  });

  it('keeps omitted provenance backward compatible with local writes', async () => {
    await remoteEvent(oldHash, Math.floor(Date.now() / 1000));
    await sync(newHash);
    const latest = await info();
    expect(latest?.hash && toHex(latest.hash)).toBe(newHash);
    expect(latest?.updatedAt).toBeGreaterThanOrEqual(Math.floor(Date.now() / 1000) - 1);
  });

  it.each([undefined, NaN, Infinity, -1, 1.5, Number.MAX_SAFE_INTEGER + 1])('rejects remote time %s without changing the cache', async (updatedAt) => {
    await remoteEvent(newHash, 200);
    const response = await sync(oldHash, { source: 'remote', updatedAt });
    expect(response).toMatchObject({ type: 'void', error: expect.stringMatching(/timestamp/i) });
    const latest = await info();
    expect(latest?.hash && toHex(latest.hash)).toBe(newHash);
    expect(latest?.updatedAt).toBe(200);
  });
});
