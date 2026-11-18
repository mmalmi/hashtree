import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import type { MobileGraphReceipt } from 'nostr-social-graph/mobileGraphSnapshot';
import { loadBundledGraphBootstrap } from '../src/relay/graphBootstrap.js';

let bytes: Uint8Array, receipt: MobileGraphReceipt;
beforeAll(async () => {
  const require = createRequire(import.meta.url);
  bytes = new Uint8Array(await readFile(require.resolve('nostr-social-graph/data/socialGraph.bin')));
  receipt = JSON.parse(await readFile(require.resolve('nostr-social-graph/data/socialGraph.json'), 'utf8'));
});
afterEach(() => vi.unstubAllGlobals());
const load = (r = receipt) => loadBundledGraphBootstrap({ url: '/assets/graph.bin', receipt: r }, new AbortController().signal);

it('checks the shipped root, hash and counts before making discovery available', async () => {
  vi.stubGlobal('fetch', vi.fn(async () => new Response(bytes)));
  const graph = await load();
  expect(graph.getRoot()).toBe(receipt.root);
  expect(graph.size().users).toBe(receipt.users);
  expect(graph.getFollowedByUser(receipt.root).size).toBe(receipt.directFollows);
});

it('rejects implausibly small and oversized public seeds before requesting bytes', async () => {
  const fetch = vi.fn(); vi.stubGlobal('fetch', fetch);
  for (const altered of [{ ...receipt, users: 1000 }, { ...receipt, bytes: 4 * 1024 * 1024 + 1 }]) {
    await expect(load(altered)).rejects.toThrow('budget');
  }
  expect(fetch).not.toHaveBeenCalled();
});

it('rejects an oversized response even when its declared length looks valid', async () => {
  let cancelled = false;
  vi.stubGlobal('fetch', vi.fn(async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new Uint8Array(receipt.bytes + 1)); },
    cancel() { cancelled = true; },
  }), { headers: { 'content-length': String(receipt.bytes) } })));
  await expect(load()).rejects.toThrow('byte limit');
  expect(cancelled).toBe(true);
});

it('rejects corrupt, truncated and misbound bytes without accepting their receipt', async () => {
  const corrupt = bytes.slice(); corrupt[corrupt.length - 1] ^= 1;
  for (const value of [corrupt, bytes.slice(0, 10)]) {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(value)));
    await expect(load()).rejects.toThrow();
  }
  vi.stubGlobal('fetch', vi.fn(async () => new Response(bytes)));
  await expect(load({ ...receipt, root: '0'.repeat(64) })).rejects.toThrow();
});
