import { describe, expect, it } from 'vitest';
import { HashTree, LinkType, MemoryStore, toHex, type CID } from '../src/index.js';

async function collect(tree: HashTree, root: CID) {
  const blocks = [];
  for await (const block of tree.walkBlocks(root)) blocks.push(block);
  return blocks;
}

describe.each([false, true])('walkBlocks (unencrypted=%s)', (unencrypted) => {
  it.each(['root', 'descendant'])('rejects a missing %s instead of reporting a complete traversal', async (missing) => {
    const store = new MemoryStore();
    const tree = new HashTree({ store });
    const file = await tree.putFile(new TextEncoder().encode('recoverable content'), { unencrypted });
    const root = await tree.putDirectory([
      { name: 'file', cid: file.cid, size: file.size, type: LinkType.Blob },
    ], { unencrypted });
    const absent = missing === 'root' ? root.cid : file.cid;
    await store.delete(absent.hash);
    await expect(collect(tree, root.cid)).rejects.toThrow(`Missing block: ${toHex(absent.hash)}`);
    // Restoring the data must allow the same tree to be walked again.
    await tree.putFile(new TextEncoder().encode('recoverable content'), { unencrypted });
    await tree.putDirectory([{ name: 'file', cid: file.cid, size: file.size, type: LinkType.Blob }], { unencrypted });
    expect(await collect(tree, root.cid)).toHaveLength(2);
  });

  it('reads each shared block once, including empty files', async () => {
    const store = new MemoryStore();
    const tree = new HashTree({ store });
    const file = await tree.putFile(new Uint8Array(), { unencrypted });
    const root = await tree.putDirectory(['one', 'two'].map(name => ({
      name, cid: file.cid, size: file.size, type: LinkType.Blob,
    })), { unencrypted });
    const reads = new Map<string, number>();
    const get = store.get.bind(store);
    store.get = async hash => {
      const key = toHex(hash);
      reads.set(key, (reads.get(key) ?? 0) + 1);
      return get(hash);
    };
    const blocks = await collect(tree, root.cid);
    expect(blocks).toHaveLength(2);
    expect([...reads.values()]).toEqual([1, 1]);
  });
});

it('walks legacy plaintext nodes carrying a key and encrypted children', async () => {
  const store = new MemoryStore();
  const tree = new HashTree({ store });
  const file = await tree.putFile(new TextEncoder().encode('encrypted child'));
  const root = await tree.putDirectory([
    { name: 'file', cid: file.cid, size: file.size, type: LinkType.Blob },
  ], { unencrypted: true });
  expect(await collect(tree, { ...root.cid, key: file.cid.key })).toHaveLength(2);
});
