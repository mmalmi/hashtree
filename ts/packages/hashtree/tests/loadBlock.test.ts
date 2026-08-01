import { describe, it, expect, beforeEach } from 'vitest';
import { HashTree, MemoryStore, LinkType, loadBlock, type CID } from '../src/index.js';

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
    await new Promise(resolve => setTimeout(resolve, 20));
    expect(settled).toBe(false);

    await store.put(hash, data);
    const result = await pending;
    expect(result).toEqual(data);
  });

  it('rejects with the abort reason when the signal aborts', async () => {
    const store = new MemoryStore();
    const hash = new Uint8Array(32).fill(9);
    const controller = new AbortController();

    const pending = loadBlock(store, hash, controller.signal);
    controller.abort(new Error('caller-cancelled'));

    await expect(pending).rejects.toThrow('caller-cancelled');
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
});

describe('listDirectory waits for missing data', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store, chunkSize: 100 });
  });

  it('listDirectory resolves once the directory block is put', async () => {
    const sourceTree = new HashTree({ store: new MemoryStore(), chunkSize: 100 });
    const fileResult = await sourceTree.putFile(new Uint8Array([42]), { unencrypted: true });
    const dirResult = await sourceTree.putDirectory(
      [{ name: 'release.json', cid: fileResult.cid, size: 1, type: LinkType.File }],
      { unencrypted: true }
    );

    const pending = tree.listDirectory(dirResult.cid);
    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    await new Promise(resolve => setTimeout(resolve, 20));
    expect(settled).toBe(false);

    // Migrate the directory block from the source tree's store to the target store.
    const sourceStore = (sourceTree as unknown as { store: MemoryStore }).store;
    const dirBlock = await sourceStore.get(dirResult.cid.hash);
    expect(dirBlock).not.toBeNull();
    await store.put(dirResult.cid.hash, dirBlock!);

    const entries = await pending;
    expect(entries).toHaveLength(1);
    expect(entries[0].name).toBe('release.json');
  });

  it('resolvePath resolves once each directory block in the path is put', async () => {
    const sourceTree = new HashTree({ store: new MemoryStore(), chunkSize: 100 });
    const fileResult = await sourceTree.putFile(new TextEncoder().encode('hi'), { unencrypted: true });
    const dirResult = await sourceTree.putDirectory(
      [{ name: 'hello.txt', cid: fileResult.cid, size: 2, type: LinkType.File }],
      { unencrypted: true }
    );

    const pending = tree.resolvePath(dirResult.cid, 'hello.txt');
    let settled = false;
    pending.then(() => { settled = true; }, () => { settled = true; });
    await new Promise(resolve => setTimeout(resolve, 20));
    expect(settled).toBe(false);

    const sourceStore = (sourceTree as unknown as { store: MemoryStore }).store;
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
