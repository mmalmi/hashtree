import { describe, expect, it, vi } from 'vitest';
import { HashTree, MemoryStore } from '@hashtree/core';
import { streamFileRangeChunks } from '../src/mediaStreaming.js';

function makeData(size: number): Uint8Array {
  const out = new Uint8Array(size);
  for (let i = 0; i < size; i++) {
    out[i] = i % 251;
  }
  return out;
}

async function collectChunks(stream: AsyncIterable<Uint8Array>): Promise<Uint8Array[]> {
  const chunks: Uint8Array[] = [];
  for await (const chunk of stream) {
    chunks.push(chunk);
  }
  return chunks;
}

function concat(chunks: Uint8Array[]): Uint8Array {
  const total = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.length;
  }
  return out;
}

describe('streamFileRangeChunks', () => {
  it('splits full reads into bounded chunks', async () => {
    const data = makeData(300_000);
    const tree = new HashTree({ store: new MemoryStore() });
    const { cid } = await tree.putFile(data);

    const chunks = await collectChunks(streamFileRangeChunks(tree, cid, 0, data.length - 1, 64 * 1024));
    expect(chunks.length).toBeGreaterThan(1);
    for (const chunk of chunks) {
      expect(chunk.length).toBeLessThanOrEqual(64 * 1024);
    }
    expect(concat(chunks)).toEqual(data);
  });

  it('resumes a timed-out encrypted read at the verified offset, preserving a partially filled output chunk', async () => {
    const store = new MemoryStore();
    const tree = new HashTree({ store, chunkSize: 71 });
    const bytes = makeData(500);
    const { cid } = await tree.putFile(bytes);
    const node = await tree.getTreeNode(cid);
    const interrupted = node!.links[3].hash;
    const get = store.get.bind(store);
    let reads = 0;
    vi.spyOn(store, 'get').mockImplementation(async (hash) => {
      if (hash.every((b, i) => b === interrupted[i]) && ++reads === 1) {
        throw new AggregateError([new Error('Blob route blossom: Blossom read timed out after 6000ms')], 'Blob retrieval was incomplete');
      }
      return get(hash);
    });
    const streams = vi.spyOn(tree, 'readFileStream');
    const data = await collectChunks(streamFileRangeChunks(tree, cid, 10, 399, 43, 1));
    expect(concat(data)).toEqual(bytes.slice(10, 400));
    expect(streams.mock.calls.map(([, options]) => options?.offset)).toEqual([10, 213]);
    expect(reads).toBe(2);
  });

  it('stops at the retry limit when the same offset stays unavailable', async () => {
    vi.useFakeTimers();
    try {
      const timeout = new Error('Blob retrieval timed out before the search completed');
      const source = { readFileStream: vi.fn(async function* (): AsyncGenerator<Uint8Array> { throw timeout; }) };
      const done = expect(collectChunks(streamFileRangeChunks(source, { hash: new Uint8Array(32) }, 0, 2, 2)))
        .rejects.toBe(timeout);
      await vi.runAllTimersAsync();
      await done;
      expect(source.readFileStream).toHaveBeenCalledTimes(5);
    } finally {
      vi.useRealTimers();
    }
  });

  it('propagates verification failures even when another source timed out', async () => {
    const failure = new AggregateError([new Error('Blob route blossom: Blossom read timed out after 6000ms'),
      new Error('Blob route peer returned content with the wrong hash')], 'Blob retrieval was incomplete');
    const source = { readFileStream: vi.fn(async function* (): AsyncGenerator<Uint8Array> { throw failure; }) };
    await expect(collectChunks(streamFileRangeChunks(source, { hash: new Uint8Array(32) }, 0, 2, 2))).rejects.toBe(failure);
    expect(source.readFileStream).toHaveBeenCalledTimes(1);
  });

  it('ends a satisfied range without touching the next unavailable block', async () => {
    const source = { async *readFileStream() {
      yield Uint8Array.of(0, 1, 2);
      throw new Error('unexpected read beyond range');
    } };
    const chunks = await collectChunks(streamFileRangeChunks(source, { hash: new Uint8Array(32) }, 0, 2, 2));
    expect(concat(chunks)).toEqual(Uint8Array.of(0, 1, 2));
  });

  it('reports an incomplete bounded response when the source ends early', async () => {
    const source = { async *readFileStream() { yield Uint8Array.of(0, 1); } };
    await expect(collectChunks(streamFileRangeChunks(source, { hash: new Uint8Array(32) }, 0, 2, 2)))
      .rejects.toThrow(/incomplete/i);
  });

  it('respects range start/end boundaries', async () => {
    const data = makeData(180_000);
    const tree = new HashTree({ store: new MemoryStore() });
    const { cid } = await tree.putFile(data);

    const start = 12_345;
    const end = 98_765;
    const chunks = await collectChunks(streamFileRangeChunks(tree, cid, start, end, 10_000));
    expect(concat(chunks)).toEqual(data.slice(start, end + 1));
  });

  it('works across internal hashtree chunk boundaries', async () => {
    const data = makeData(3 * 1024 * 1024 + 137);
    const tree = new HashTree({ store: new MemoryStore() });
    const { cid } = await tree.putFile(data);

    const start = 2 * 1024 * 1024 - 111;
    const end = 2 * 1024 * 1024 + 111_111;
    const chunks = await collectChunks(streamFileRangeChunks(tree, cid, start, end, 32 * 1024));
    expect(concat(chunks)).toEqual(data.slice(start, end + 1));
  });
});

