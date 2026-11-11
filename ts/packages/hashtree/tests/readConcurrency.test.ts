import { describe, it, expect, beforeEach } from 'vitest';
import { HashTree } from '../src/index.js';
import { MemoryStore } from '../src/store/memory.js';
import { encodeAndHash } from '../src/codec.js';
import { LinkType, toHex, type Hash, type Link, type Store } from '../src/types.js';
import { READ_AHEAD_BYTES, READ_CONCURRENCY } from '../src/tree/readAhead.js';

class TrackingStore implements Store {
  readonly inner = new MemoryStore();
  inFlight = 0;
  maxInFlight = 0;
  gets: string[] = [];
  delayFor: (hash: Hash, call: number) => number = () => 2;
  failOn = new Set<string>();

  put(hash: Hash, data: Uint8Array): Promise<boolean> {
    return this.inner.put(hash, data);
  }

  async get(hash: Hash): Promise<Uint8Array | null> {
    const call = this.gets.push(toHex(hash));
    this.inFlight++;
    this.maxInFlight = Math.max(this.maxInFlight, this.inFlight);
    try {
      await new Promise((resolve) => setTimeout(resolve, this.delayFor(hash, call)));
      if (this.failOn.has(toHex(hash))) throw new Error('store unavailable');
      return await this.inner.get(hash);
    } finally {
      this.inFlight--;
    }
  }

  has(hash: Hash): Promise<boolean> {
    return this.inner.has(hash);
  }

  delete(hash: Hash): Promise<boolean> {
    return this.inner.delete(hash);
  }

  reset(): void {
    this.gets = [];
    this.maxInFlight = 0;
  }
}

function patterned(length: number): Uint8Array {
  const data = new Uint8Array(length);
  for (let i = 0; i < length; i++) data[i] = (i * 31 + (i >> 8)) % 251;
  return data;
}

describe('concurrent chunk reads', () => {
  const chunkSize = 100;
  let store: TrackingStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new TrackingStore();
    tree = new HashTree({ store, chunkSize });
  });

  it('reads the chunks of a range concurrently and returns exact bytes', async () => {
    const data = patterned(40 * chunkSize + 37);
    const { cid } = await tree.putFile(data, { unencrypted: true });
    store.reset();

    const result = await tree.readFileRange(cid, 150, 3_650);

    expect(result).toEqual(data.slice(150, 3_650));
    expect(store.maxInFlight).toBe(READ_CONCURRENCY);
    // Root node plus chunks 1..36, each exactly once.
    expect(store.gets).toHaveLength(1 + 36);
    expect(new Set(store.gets).size).toBe(store.gets.length);
  });

  it('only fetches chunks that intersect the range', async () => {
    const data = patterned(40 * chunkSize);
    const { cid } = await tree.putFile(data, { unencrypted: true });
    store.reset();

    const result = await tree.readFileRange(cid, 1_000, 1_250);

    expect(result).toEqual(data.slice(1_000, 1_250));
    expect(store.gets).toHaveLength(1 + 3);
  });

  it('fetches nothing for an empty range', async () => {
    const data = patterned(10 * chunkSize);
    const { cid } = await tree.putFile(data, { unencrypted: true });
    store.reset();

    expect(await tree.readFileRange(cid, 500, 500)).toEqual(new Uint8Array(0));
    expect(store.gets).toHaveLength(1);
  });

  it('keeps byte order when later chunks arrive first', async () => {
    const data = patterned(30 * chunkSize + 5);
    const { cid } = await tree.putFile(data, { unencrypted: true });
    store.reset();
    store.delayFor = (_hash, call) => Math.max(0, 40 - call);

    expect(await tree.readFileRange(cid, 0)).toEqual(data);
    expect(await tree.readFile(cid)).toEqual(data);
  });

  it('assembles whole files concurrently within the limit', async () => {
    const data = patterned(50 * chunkSize + 1);
    const { cid } = await tree.putFile(data, { unencrypted: true });
    store.reset();

    const result = await tree.readFile(cid);

    expect(result).toEqual(data);
    expect(store.maxInFlight).toBe(READ_CONCURRENCY);
    expect(store.gets).toHaveLength(1 + 51);
  });

  it('reads encrypted ranges concurrently and returns exact bytes', async () => {
    const data = patterned(25 * chunkSize + 11);
    const { cid } = await tree.putFile(data);
    expect(cid.key).toBeDefined();
    store.reset();

    expect(await tree.readFileRange(cid, 333, 2_222)).toEqual(data.slice(333, 2_222));
    expect(store.maxInFlight).toBeGreaterThan(1);
    expect(store.maxInFlight).toBeLessThanOrEqual(READ_CONCURRENCY);
    expect(await tree.readFile(cid)).toEqual(data);
  });

  it('shares one limit across intermediate nodes', async () => {
    const data = patterned(60 * chunkSize);
    const subtrees: Link[] = [];
    for (let group = 0; group < 3; group++) {
      const links: Link[] = [];
      for (let i = 0; i < 20; i++) {
        const offset = (group * 20 + i) * chunkSize;
        const hash = await tree.putBlob(data.slice(offset, offset + chunkSize));
        links.push({ hash, size: chunkSize, type: LinkType.Blob });
      }
      const encoded = await encodeAndHash({ type: LinkType.File, links });
      await store.put(encoded.hash, encoded.data);
      subtrees.push({ hash: encoded.hash, size: 20 * chunkSize, type: LinkType.File });
    }
    const root = await encodeAndHash({ type: LinkType.File, links: subtrees });
    await store.put(root.hash, root.data);
    store.reset();

    expect(await tree.readFileRange({ hash: root.hash }, 1_950, 4_075)).toEqual(data.slice(1_950, 4_075));
    expect(store.maxInFlight).toBeGreaterThan(1);
    expect(store.maxInFlight).toBeLessThanOrEqual(READ_CONCURRENCY);
    // Root, three intermediate nodes, chunks 19..40.
    expect(store.gets).toHaveLength(1 + 3 + 22);

    store.reset();
    expect(await tree.readFile({ hash: root.hash })).toEqual(data);
    expect(store.maxInFlight).toBeLessThanOrEqual(READ_CONCURRENCY);
    expect(store.gets).toHaveLength(1 + 3 + 60);
  });

  it('bounds read-ahead by bytes for large chunks', async () => {
    const largeChunk = 1024 * 1024;
    const largeTree = new HashTree({ store, chunkSize: largeChunk });
    const data = patterned(20 * largeChunk);
    const { cid } = await largeTree.putFile(data, { unencrypted: true });
    store.reset();

    const result = await largeTree.readFileRange(cid, 10, data.length - 10);

    expect(Buffer.compare(result!, data.subarray(10, data.length - 10))).toBe(0);
    expect(store.maxInFlight).toBeGreaterThan(1);
    expect(store.maxInFlight).toBeLessThanOrEqual(READ_AHEAD_BYTES / largeChunk);
  });

  it('follows actual chunk lengths when declared sizes are unknown', async () => {
    const data = patterned(12 * chunkSize);
    const links: Link[] = [];
    for (let i = 0; i < 12; i++) {
      const hash = await tree.putBlob(data.slice(i * chunkSize, (i + 1) * chunkSize));
      links.push({ hash, size: 0, type: LinkType.Blob });
    }
    const root = await encodeAndHash({ type: LinkType.File, links });
    await store.put(root.hash, root.data);

    expect(await tree.readFileRange({ hash: root.hash }, 250, 875)).toEqual(data.slice(250, 875));
  });

  it('reports the first missing chunk in file order', async () => {
    const data = patterned(30 * chunkSize);
    const { cid } = await tree.putFile(data, { unencrypted: true });
    const node = await tree.getTreeNode(cid);
    await store.inner.delete(node!.links[4].hash);
    await store.inner.delete(node!.links[9].hash);

    await expect(tree.readFileRange(cid, 0, 2_000)).rejects.toThrow(`Missing chunk: ${toHex(node!.links[4].hash)}`);
    await expect(tree.readFile(cid)).rejects.toThrow(`Missing chunk: ${toHex(node!.links[4].hash)}`);
  });

  it('propagates store errors without leaving unhandled rejections', async () => {
    const data = patterned(30 * chunkSize);
    const { cid } = await tree.putFile(data, { unencrypted: true });
    const node = await tree.getTreeNode(cid);
    store.failOn.add(toHex(node!.links[3].hash));
    store.failOn.add(toHex(node!.links[7].hash));

    const unhandled: unknown[] = [];
    const onUnhandled = (reason: unknown) => unhandled.push(reason);
    process.on('unhandledRejection', onUnhandled);
    try {
      await expect(tree.readFileRange(cid, 0, 2_000)).rejects.toThrow('store unavailable');
      await new Promise((resolve) => setTimeout(resolve, 20));
    } finally {
      process.off('unhandledRejection', onUnhandled);
    }
    expect(unhandled).toEqual([]);
  });

  it('handles failures of reads that were queued behind the shared limit', async () => {
    const data = patterned(60 * chunkSize);
    const subtrees: Link[] = [];
    const leaves: Link[] = [];
    for (let group = 0; group < 20; group++) {
      const links: Link[] = [];
      for (let i = 0; i < 3; i++) {
        const offset = (group * 3 + i) * chunkSize;
        const hash = await tree.putBlob(data.slice(offset, offset + chunkSize));
        links.push({ hash, size: chunkSize, type: LinkType.Blob });
      }
      leaves.push(...links);
      const encoded = await encodeAndHash({ type: LinkType.File, links });
      await store.put(encoded.hash, encoded.data);
      subtrees.push({ hash: encoded.hash, size: 3 * chunkSize, type: LinkType.File });
    }
    const root = await encodeAndHash({ type: LinkType.File, links: subtrees });
    await store.put(root.hash, root.data);
    for (const leaf of leaves) store.failOn.add(toHex(leaf.hash));
    // Leaves of the first subtree queue behind the sixteen intermediate reads.
    store.delayFor = (hash) => (leaves.some((leaf) => toHex(leaf.hash) === toHex(hash)) ? 1 : 15);

    const unhandled: unknown[] = [];
    const onUnhandled = (reason: unknown) => unhandled.push(reason);
    process.on('unhandledRejection', onUnhandled);
    try {
      await expect(tree.readFile({ hash: root.hash })).rejects.toThrow('store unavailable');
      await expect(tree.readFileRange({ hash: root.hash }, 50, 5_000)).rejects.toThrow('store unavailable');
      await new Promise((resolve) => setTimeout(resolve, 60));
    } finally {
      process.off('unhandledRejection', onUnhandled);
    }
    expect(unhandled).toEqual([]);
    expect(store.maxInFlight).toBeLessThanOrEqual(READ_CONCURRENCY);
  });

  it('does not read ahead past the byte limit of a whole-file read', async () => {
    const data = patterned(40 * chunkSize);
    const subtrees: Link[] = [];
    for (let group = 0; group < 2; group++) {
      const links: Link[] = [];
      for (let i = 0; i < 20; i++) {
        const offset = (group * 20 + i) * chunkSize;
        const hash = await tree.putBlob(data.slice(offset, offset + chunkSize));
        links.push({ hash, size: chunkSize, type: LinkType.Blob });
      }
      const encoded = await encodeAndHash({ type: LinkType.File, links });
      await store.put(encoded.hash, encoded.data);
      // Understated subtree size: the limit is only discovered at the leaves.
      subtrees.push({ hash: encoded.hash, size: chunkSize, type: LinkType.File });
    }
    const root = await encodeAndHash({ type: LinkType.File, links: subtrees });
    await store.put(root.hash, root.data);
    store.reset();

    await expect(tree.readFile({ hash: root.hash }, { maxBytes: 5 * chunkSize })).rejects.toThrow('exceeds maxBytes');
    // Root, both intermediate nodes, and no more than the five chunks within the limit.
    expect(store.gets.length).toBeLessThanOrEqual(1 + 2 + 5);
  });
});
