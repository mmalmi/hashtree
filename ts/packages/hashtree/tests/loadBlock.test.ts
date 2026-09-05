import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { HashTree, MemoryStore, LinkType, loadBlock, type CID, type Store } from '../src/index.js';

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

describe('loadBlock', () => {
  it('returns immediately when the block is already in the store', async () => {
    const store = new MemoryStore();
    const data = new Uint8Array([1, 2, 3]);
    const hash = new Uint8Array(32).fill(7);
    await store.put(hash, data);

    const result = await loadBlock(store, hash);
    expect(result).toEqual(data);
  });

  it('waits until the block is put, then resolves with the data', async () => {
    const store = new MemoryStore();
    const data = new Uint8Array([4, 5, 6]);
    const hash = new Uint8Array(32).fill(8);

    const pending = loadBlock(store, hash);

    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    await vi.advanceTimersByTimeAsync(0);
    expect(settled).toBe(false);

    await store.put(hash, data);
    const result = await pending;
    expect(result).toEqual(data);
  });

  it('unsubscribes when the signal aborts after watching starts', async () => {
    const store = new MemoryStore();
    const hash = new Uint8Array(32).fill(9);
    const controller = new AbortController();
    const watch = store.watch.bind(store);
    const unsubscribe = vi.fn();
    vi.spyOn(store, 'watch').mockImplementation((hash, callback) => {
      const unwatch = watch(hash, callback);
      return () => { unwatch(); unsubscribe(); };
    });

    const pending = loadBlock(store, hash, controller.signal);
    await vi.advanceTimersByTimeAsync(0);
    controller.abort(new Error('caller-cancelled'));

    await expect(pending).rejects.toThrow('caller-cancelled');
    expect(unsubscribe).toHaveBeenCalledOnce();
  });

  it('rejects when aborted while the initial store read is still pending', async () => {
    const hash = new Uint8Array(32).fill(12);
    const controller = new AbortController();
    const store = {
      get: () => new Promise<Uint8Array | null>(() => {}),
    } as MemoryStore;

    const pending = loadBlock(store, hash, controller.signal);
    controller.abort(new Error('cancel-pending-initial-read'));

    await expect(pending).rejects.toThrow('cancel-pending-initial-read');
  });

  it('rejects synchronously when the signal is already aborted', async () => {
    const store = new MemoryStore();
    const hash = new Uint8Array(32).fill(10);
    const controller = new AbortController();
    controller.abort(new Error('pre-cancelled'));

    await expect(loadBlock(store, hash, controller.signal)).rejects.toThrow('pre-cancelled');
  });

  it('cleans up a synchronous watch delivery without reading the block again', async () => {
    const store = new MemoryStore();
    const hash = new Uint8Array(32).fill(13);
    const data = new Uint8Array([7, 8, 9]);
    const get = vi.spyOn(store, 'get').mockImplementationOnce(async () => {
      await store.put(hash, data);
      return null;
    });
    const watch = store.watch.bind(store);
    const unsubscribe = vi.fn();
    vi.spyOn(store, 'watch').mockImplementation((hash, callback) => {
      const unwatch = watch(hash, callback);
      return () => { unwatch(); unsubscribe(); };
    });

    await expect(loadBlock(store, hash)).resolves.toEqual(data);
    expect(get).toHaveBeenCalledTimes(1);
    expect(unsubscribe).toHaveBeenCalledOnce();
  });

  it.each(['get', 'watch'] as const)('cleans up abort listeners when %s throws synchronously', async (method) => {
    const store = new MemoryStore();
    const controller = new AbortController();
    const added = vi.spyOn(controller.signal, 'addEventListener');
    const removed = vi.spyOn(controller.signal, 'removeEventListener');
    const error = new Error('store failure');
    vi.spyOn(store, method).mockImplementation(() => { throw error; });

    await expect(loadBlock(store, new Uint8Array(32), controller.signal)).rejects.toBe(error);
    for (const [event, listener] of added.mock.calls) {
      expect(removed).toHaveBeenCalledWith(event, listener);
    }
  });

  it('cleans up after watch delivery even when the follow-up read stalls', async () => {
    const store = new MemoryStore();
    const hash = new Uint8Array(32).fill(15);
    const data = new Uint8Array([13, 14, 15]);
    const controller = new AbortController();
    const added = vi.spyOn(controller.signal, 'addEventListener');
    const removed = vi.spyOn(controller.signal, 'removeEventListener');
    vi.spyOn(store, 'get')
      .mockResolvedValueOnce(null)
      .mockImplementation(() => new Promise(() => {}));

    const pending = loadBlock(store, hash, controller.signal);
    await vi.advanceTimersByTimeAsync(0);
    await store.put(hash, data);

    await expect(pending).resolves.toEqual(data);
    for (const [event, listener] of added.mock.calls) {
      expect(removed).toHaveBeenCalledWith(event, listener);
    }
  });

  it('keeps polling through transient errors until data arrives', async () => {
    const store: Store = new MemoryStore();
    store.watch = undefined;
    const hash = new Uint8Array(32).fill(14);
    const data = new Uint8Array([10, 11, 12]);
    const get = vi.spyOn(store, 'get');
    const pending = loadBlock(store, hash);
    await vi.advanceTimersByTimeAsync(0);

    get.mockRejectedValueOnce(new Error('temporary failure'));
    await vi.advanceTimersByTimeAsync(500);
    await store.put(hash, data);
    await vi.advanceTimersByTimeAsync(500);

    await expect(pending).resolves.toEqual(data);
    expect(get).toHaveBeenCalledTimes(3);
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each([0, 500])('cancels polling after %i ms even if a store read stalls', async (elapsed) => {
    const store: Store = new MemoryStore();
    store.watch = undefined;
    const controller = new AbortController();
    const get = vi.spyOn(store, 'get')
      .mockResolvedValueOnce(null)
      .mockImplementation(() => new Promise(() => {}));
    const pending = loadBlock(store, new Uint8Array(32), controller.signal);
    await vi.advanceTimersByTimeAsync(elapsed);
    controller.abort(new Error('cancel-polling'));

    await expect(pending).rejects.toThrow('cancel-polling');
    expect(get).toHaveBeenCalledTimes(elapsed === 0 ? 1 : 2);
    expect(vi.getTimerCount()).toBe(0);
  });
});

describe('listDirectory waits for missing data', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store, chunkSize: 100 });
  });

  it('listDirectory resolves once the directory block is put', async () => {
    const sourceStore = new MemoryStore();
    const sourceTree = new HashTree({ store: sourceStore, chunkSize: 100 });
    const fileResult = await sourceTree.putFile(new Uint8Array([42]), { unencrypted: true });
    const dirResult = await sourceTree.putDirectory(
      [{ name: 'release.json', cid: fileResult.cid, size: 1, type: LinkType.File }],
      { unencrypted: true }
    );

    const pending = tree.listDirectory(dirResult.cid);
    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    await vi.advanceTimersByTimeAsync(0);
    expect(settled).toBe(false);

    // Migrate the directory block from the source tree's store to the target store.
    const dirBlock = await sourceStore.get(dirResult.cid.hash);
    expect(dirBlock).not.toBeNull();
    await store.put(dirResult.cid.hash, dirBlock!);

    const entries = await pending;
    expect(entries).toHaveLength(1);
    expect(entries[0].name).toBe('release.json');
  });

  it('resolvePath resolves once each directory block in the path is put', async () => {
    const sourceStore = new MemoryStore();
    const sourceTree = new HashTree({ store: sourceStore, chunkSize: 100 });
    const fileResult = await sourceTree.putFile(new TextEncoder().encode('hi'), { unencrypted: true });
    const dirResult = await sourceTree.putDirectory(
      [{ name: 'hello.txt', cid: fileResult.cid, size: 2, type: LinkType.File }],
      { unencrypted: true }
    );

    const pending = tree.resolvePath(dirResult.cid, 'hello.txt');
    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    await vi.advanceTimersByTimeAsync(0);
    expect(settled).toBe(false);

    const dirBlock = await sourceStore.get(dirResult.cid.hash);
    await store.put(dirResult.cid.hash, dirBlock!);

    const resolved = await pending;
    expect(resolved).not.toBeNull();
    expect(resolved!.type).toBe(LinkType.File);
  });

  it('listDirectory aborts via signal without leaking timers', async () => {
    const hash = new Uint8Array(32).fill(11);
    const cid: CID = { hash };
    const controller = new AbortController();

    const pending = tree.listDirectory(cid, controller.signal);
    controller.abort(new Error('abort-list'));

    await expect(pending).rejects.toThrow('abort-list');
  });

  it('resolvePath returns null when an entry truly is not in a loaded directory', async () => {
    const fileResult = await tree.putFile(new Uint8Array([1]), { unencrypted: true });
    const dirResult = await tree.putDirectory(
      [{ name: 'a.txt', cid: fileResult.cid, size: 1, type: LinkType.File }],
      { unencrypted: true }
    );

    const resolved = await tree.resolvePath(dirResult.cid, 'b.txt');
    expect(resolved).toBeNull();
  });
});
