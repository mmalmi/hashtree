import { describe, it, expect, beforeEach } from 'vitest';
import { HashTree, DEFAULT_CHUNK_SIZE, DEFAULT_MAX_LINKS } from '../src/index.js';
import { MemoryStore } from '../src/store/memory.js';
import { toHex, cid, LinkType } from '../src/types.js';
import { sha256 } from '../src/hash.js';
import { decodeTreeNode } from '../src/codec.js';

describe('HashTree write operations', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store });
  });

  describe('putBlob', () => {
    it('should store blob and return hash', async () => {
      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const hash = await tree.putBlob(data);

      expect(hash.length).toBe(32);
      expect(await store.has(hash)).toBe(true);

      const retrieved = await store.get(hash);
      expect(retrieved).toEqual(data);
    });

    it('should compute correct hash', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const hash = await tree.putBlob(data);
      const expectedHash = await sha256(data);

      expect(toHex(hash)).toBe(toHex(expectedHash));
    });
  });

  describe('putFile', () => {
    it('should store small file as single blob', async () => {
      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const { cid: fileCid, size } = await tree.putFile(data, { unencrypted: true });

      expect(size).toBe(5);
      expect(await tree.readFile(fileCid)).toEqual(data);
    });

    it('should chunk large files', async () => {
      // Create data larger than chunk size
      const chunkSize = 1024;
      const smallTree = new HashTree({ store, chunkSize });

      const data = new Uint8Array(chunkSize * 2 + 100);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }

      const { cid: fileCid, size } = await smallTree.putFile(data, { unencrypted: true });

      expect(size).toBe(data.length);

      // Should be a tree node (File), not raw blob
      const nodeType = await tree.getType(fileCid);
      expect(nodeType).toBe(LinkType.File);

      // Should reassemble correctly
      const retrieved = await tree.readFile(fileCid);
      expect(retrieved).toEqual(data);
    });

    it('should handle file exactly chunk size', async () => {
      const chunkSize = 256;
      const smallTree = new HashTree({ store, chunkSize });

      const data = new Uint8Array(chunkSize);
      data.fill(42);

      const { cid: fileCid, size } = await smallTree.putFile(data, { unencrypted: true });

      expect(size).toBe(chunkSize);
      expect(await tree.readFile(fileCid)).toEqual(data);
    });

    it('should create balanced tree for many chunks', async () => {
      const chunkSize = 100;
      const maxLinks = 4;
      const smallTree = new HashTree({ store, chunkSize, maxLinks });

      // Create 10 chunks worth of data (will need multiple tree levels)
      const data = new Uint8Array(chunkSize * 10);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }

      const { cid: fileCid, size } = await smallTree.putFile(data, { unencrypted: true });

      expect(size).toBe(data.length);
      expect(await tree.readFile(fileCid)).toEqual(data);
    });
  });

  describe('putDirectory', () => {
    it('should create directory from entries', async () => {
      const file1 = new Uint8Array([1, 2, 3]);
      const file2 = new Uint8Array([4, 5, 6, 7]);

      const hash1 = await tree.putBlob(file1);
      const hash2 = await tree.putBlob(file2);

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'a.txt', cid: cid(hash1), size: file1.length },
        { name: 'b.txt', cid: cid(hash2), size: file2.length },
      ], { unencrypted: true });

      const entries = await tree.listDirectory(dirCid);
      expect(entries.length).toBe(2);
      expect(entries.find(e => e.name === 'a.txt')).toBeDefined();
      expect(entries.find(e => e.name === 'b.txt')).toBeDefined();
    });

    it('should sort entries by name', async () => {
      const hash = await tree.putBlob(new Uint8Array([1]));

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'zebra', cid: cid(hash) },
        { name: 'apple', cid: cid(hash) },
        { name: 'mango', cid: cid(hash) },
      ], { unencrypted: true });

      const node = await tree.getTreeNode(dirCid);
      expect(node!.links.map(l => l.name)).toEqual(['apple', 'mango', 'zebra']);
    });

    it('should create nested directories', async () => {
      const fileData = new Uint8Array([1, 2, 3]);
      const fileHash = await tree.putBlob(fileData);

      const { cid: subDirCid } = await tree.putDirectory([
        { name: 'file.txt', cid: cid(fileHash), size: 3 },
      ], { unencrypted: true });

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'subdir', cid: subDirCid },
      ], { unencrypted: true });

      const resolved = await tree.resolvePath(rootCid, 'subdir/file.txt');
      expect(resolved).not.toBeNull();
      expect(toHex(resolved!.cid.hash)).toBe(toHex(fileHash));
    });

    it('should split large directories', async () => {
      const maxLinks = 4;
      const smallTree = new HashTree({ store, maxLinks });

      const entries = [];
      for (let i = 0; i < 10; i++) {
        const data = new Uint8Array([i]);
        const hash = await smallTree.putBlob(data);
        entries.push({ name: `file${i.toString().padStart(2, '0')}.txt`, cid: cid(hash), size: 1 });
      }

      const { cid: dirCid } = await smallTree.putDirectory(entries, { unencrypted: true });

      // Should be able to list all entries even though dir is split
      const listed = await tree.listDirectory(dirCid);
      expect(listed.length).toBe(10);
    });
  });
});

describe('StreamWriter via createStream()', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store });
  });

  describe('append', () => {
    it('should build file from multiple appends', async () => {
      const stream = new HashTree({ store, chunkSize: 100 }).createStream();

      await stream.append(new Uint8Array([1, 2, 3]));
      await stream.append(new Uint8Array([4, 5]));
      await stream.append(new Uint8Array([6, 7, 8, 9]));

      const { hash, size, key } = await stream.finalize();

      expect(size).toBe(9);
      const data = await tree.readFile(cid(hash, key));
      expect(data).toEqual(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9]));
    });

    it('should handle appends crossing chunk boundaries', async () => {
      const chunkSize = 10;
      const stream = new HashTree({ store, chunkSize }).createStream();

      // Append 25 bytes in various sizes
      await stream.append(new Uint8Array(7).fill(1));
      await stream.append(new Uint8Array(8).fill(2));
      await stream.append(new Uint8Array(10).fill(3));

      const { hash, size, key } = await stream.finalize();
      expect(size).toBe(25);

      const data = await tree.readFile(cid(hash, key));
      expect(data!.length).toBe(25);
      expect(data![0]).toBe(1);
      expect(data![7]).toBe(2);
      expect(data![15]).toBe(3);
    });

    it('should track stats', async () => {
      const stream = new HashTree({ store, chunkSize: 100 }).createStream();

      expect(stream.stats.chunks).toBe(0);
      expect(stream.stats.buffered).toBe(0);
      expect(stream.stats.totalSize).toBe(0);

      await stream.append(new Uint8Array(50));
      expect(stream.stats.buffered).toBe(50);
      expect(stream.stats.totalSize).toBe(50);

      await stream.append(new Uint8Array(60)); // Crosses boundary
      expect(stream.stats.chunks).toBe(1);
      expect(stream.stats.buffered).toBe(10);
      expect(stream.stats.totalSize).toBe(110);
    });
  });

  describe('currentRoot', () => {
    it('should return current root CID without finalizing', async () => {
      const stream = new HashTree({ store, chunkSize: 100 }).createStream();

      await stream.append(new Uint8Array([1, 2, 3]));
      const root1 = await stream.currentRoot();

      await stream.append(new Uint8Array([4, 5, 6]));
      const root2 = await stream.currentRoot();

      // Roots should be different (CID now includes hash + key)
      expect(toHex(root1!.hash)).not.toBe(toHex(root2!.hash));

      // Can still finalize
      const { hash } = await stream.finalize();
      expect(hash.length).toBe(32);
    });

    it('should return null for empty stream', async () => {
      const stream = new HashTree({ store }).createStream();
      const root = await stream.currentRoot();
      expect(root).toBeNull();
    });
  });

  describe('finalize', () => {
    it('should handle empty stream', async () => {
      const stream = new HashTree({ store }).createStream();
      const { hash, size, key } = await stream.finalize();

      expect(size).toBe(0);
      const data = await tree.readFile(cid(hash, key));
      expect(data).toEqual(new Uint8Array(0));
    });

    it('should create balanced tree for large streams', async () => {
      const chunkSize = 100;
      const maxLinks = 4;
      const stream = new HashTree({ store, chunkSize, maxLinks }).createStream();

      // Add 20 chunks worth
      for (let i = 0; i < 20; i++) {
        const chunk = new Uint8Array(chunkSize);
        chunk.fill(i);
        await stream.append(chunk);
      }

      const { hash, size, key } = await stream.finalize();
      expect(size).toBe(2000);

      // Verify can read back
      const data = await tree.readFile(cid(hash, key));
      expect(data!.length).toBe(2000);
      expect(data![0]).toBe(0);
      expect(data![100]).toBe(1);
      expect(data![1900]).toBe(19);
    });
  });

  describe('link meta', () => {
    it('should store meta on directory entries', async () => {
      const fileHash = await tree.putBlob(new TextEncoder().encode('test'));
      const meta = {
        createdAt: 1700000000,
        version: '1.0',
        author: 'test-user',
      };

      const { cid: dirCid } = await tree.putDirectory(
        [{ name: 'file.txt', cid: cid(fileHash), size: 4, type: LinkType.Blob, meta }],
        { unencrypted: true }
      );

      // Read back the tree node and verify link meta
      const encoded = await store.get(dirCid.hash);
      expect(encoded).not.toBeNull();

      const node = decodeTreeNode(encoded!);
      expect(node.links[0].meta).toEqual(meta);
      expect(node.links[0].meta!.createdAt).toBe(1700000000);
      expect(node.links[0].meta!.version).toBe('1.0');
    });

    it('should preserve link meta on large directories', async () => {
      const smallTree = new HashTree({ store, maxLinks: 4 });

      // Create enough entries to trigger sub-tree creation
      const entries = [];
      for (let i = 0; i < 10; i++) {
        const hash = await smallTree.putBlob(new Uint8Array([i]));
        entries.push({ name: `file${i}.txt`, cid: cid(hash), size: 1, type: LinkType.Blob, meta: { createdAt: 1700000000 + i } });
      }

      const { cid: dirCid } = await smallTree.putDirectory(entries, { unencrypted: true });

      // Read back directory and verify link meta is preserved
      const listing = await smallTree.listDirectory(dirCid);
      const file5 = listing.find(e => e.name === 'file5.txt');
      expect(file5?.meta).toEqual({ createdAt: 1700000005 });
    });
  });
});
