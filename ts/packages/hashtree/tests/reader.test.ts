import { describe, it, expect, beforeEach } from 'vitest';
import { HashTree, verifyTree } from '../src/index.js';
import { MemoryStore } from '../src/store/memory.js';
import { toHex, LinkType } from '../src/types.js';

describe('HashTree read operations', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store, chunkSize: 100 });
  });

  describe('getBlob', () => {
    it('should return blob data', async () => {
      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const hash = await tree.putBlob(data);

      const result = await tree.getBlob(hash);
      expect(result).toEqual(data);
    });

    it('should return null for non-existent hash', async () => {
      const hash = new Uint8Array(32).fill(0);
      const result = await tree.getBlob(hash);
      expect(result).toBeNull();
    });
  });

  describe('getTreeNode', () => {
    it('should return decoded tree node', async () => {
      const { cid: fileCid } = await tree.putFile(new Uint8Array([1]), { unencrypted: true });
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'test.txt', cid: fileCid, size: 1 },
      ], { unencrypted: true });

      const node = await tree.getTreeNode(dirCid);
      expect(node).not.toBeNull();
      expect(node!.type).toBe(LinkType.Dir);
      expect(node!.links.length).toBe(1);
    });

    it('should return null for blob hash', async () => {
      const hash = await tree.putBlob(new Uint8Array([1, 2, 3]));
      const node = await tree.getTreeNode({ hash });
      expect(node).toBeNull();
    });
  });

  describe('getType', () => {
    it('should return Dir for directory nodes', async () => {
      const { cid: fileCid } = await tree.putFile(new Uint8Array([1]), { unencrypted: true });
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'test.txt', cid: fileCid },
      ], { unencrypted: true });

      expect(await tree.getType(dirCid)).toBe(LinkType.Dir);
    });

    it('should return Blob for raw blobs', async () => {
      const hash = await tree.putBlob(new Uint8Array([1, 2, 3]));
      expect(await tree.getType({ hash })).toBe(LinkType.Blob);
    });
  });

  describe('readFile', () => {
    it('should read small file directly', async () => {
      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const result = await tree.readFile(cid);
      expect(result).toEqual(data);
    });

    it('should reassemble chunked file', async () => {
      const data = new Uint8Array(350); // More than 3 chunks
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }

      const { cid } = await tree.putFile(data, { unencrypted: true });
      const result = await tree.readFile(cid);

      expect(result).toEqual(data);
    });

    it('should return null for non-existent hash', async () => {
      const hash = new Uint8Array(32).fill(0);
      const result = await tree.readFile({ hash });
      expect(result).toBeNull();
    });
  });

  describe('readFileStream', () => {
    it('should stream file chunks', async () => {
      const data = new Uint8Array(350);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }

      const { cid } = await tree.putFile(data, { unencrypted: true });

      const chunks: Uint8Array[] = [];
      for await (const chunk of tree.readFileStream(cid)) {
        chunks.push(chunk);
      }

      expect(chunks.length).toBeGreaterThan(1);

      // Concatenate and verify
      const total = chunks.reduce((sum, c) => sum + c.length, 0);
      expect(total).toBe(350);
    });

    it('should handle empty iteration for non-existent hash', async () => {
      const hash = new Uint8Array(32).fill(0);
      const chunks: Uint8Array[] = [];

      for await (const chunk of tree.readFileStream({ hash })) {
        chunks.push(chunk);
      }

      expect(chunks.length).toBe(0);
    });
  });

  describe('readFileRange', () => {
    it('should read a range from small file', async () => {
      const data = new Uint8Array([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const result = await tree.readFileRange(cid, 2, 6);
      expect(result).toEqual(new Uint8Array([2, 3, 4, 5]));
    });

    it('should read a range from chunked file', async () => {
      const data = new Uint8Array(350);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }
      const { cid } = await tree.putFile(data, { unencrypted: true });

      // Read across chunk boundaries (chunk size is 100)
      const result = await tree.readFileRange(cid, 90, 110);
      expect(result).toEqual(data.slice(90, 110));
    });

    it('should read from middle of file to end', async () => {
      const data = new Uint8Array(350);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const result = await tree.readFileRange(cid, 300);
      expect(result).toEqual(data.slice(300));
    });

    it('should handle end beyond file length', async () => {
      const data = new Uint8Array([0, 1, 2, 3, 4]);
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const result = await tree.readFileRange(cid, 2, 100);
      expect(result).toEqual(new Uint8Array([2, 3, 4]));
    });

    it('should return empty for start beyond file length', async () => {
      const data = new Uint8Array([0, 1, 2, 3, 4]);
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const result = await tree.readFileRange(cid, 100, 200);
      expect(result).toEqual(new Uint8Array(0));
    });

    it('should return null for non-existent hash', async () => {
      const hash = new Uint8Array(32).fill(0);
      const result = await tree.readFileRange({ hash }, 0, 10);
      expect(result).toBeNull();
    });
  });

  describe('readFileStream with offset', () => {
    it('should stream from offset', async () => {
      const data = new Uint8Array(350);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const chunks: Uint8Array[] = [];
      for await (const chunk of tree.readFileStream(cid, { offset: 200 })) {
        chunks.push(chunk);
      }

      const total = new Uint8Array(chunks.reduce((sum, c) => sum + c.length, 0));
      let offset = 0;
      for (const chunk of chunks) {
        total.set(chunk, offset);
        offset += chunk.length;
      }

      expect(total).toEqual(data.slice(200));
    });

    it('should skip chunks before offset', async () => {
      const data = new Uint8Array(350);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }
      const { cid } = await tree.putFile(data, { unencrypted: true });

      // Track which chunks are yielded
      const chunks: Uint8Array[] = [];
      for await (const chunk of tree.readFileStream(cid, { offset: 250 })) {
        chunks.push(chunk);
      }

      // Should not yield the first 2 full chunks (0-99, 100-199)
      // Should yield partial of chunk 3 (200-249 skipped, 250-299) and full chunk 4 (300-349)
      const totalBytes = chunks.reduce((sum, c) => sum + c.length, 0);
      expect(totalBytes).toBe(100); // 350 - 250
    });

    it('should handle offset beyond file length', async () => {
      const data = new Uint8Array([1, 2, 3]);
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const chunks: Uint8Array[] = [];
      for await (const chunk of tree.readFileStream(cid, { offset: 100 })) {
        chunks.push(chunk);
      }

      expect(chunks.length).toBe(0);
    });

    it('should stream with prefetch option', async () => {
      // Create a file with multiple chunks
      const data = new Uint8Array(500);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const chunks: Uint8Array[] = [];
      for await (const chunk of tree.readFileStream(cid, { prefetch: 3 })) {
        chunks.push(chunk);
      }

      const total = new Uint8Array(chunks.reduce((sum, c) => sum + c.length, 0));
      let offset = 0;
      for (const chunk of chunks) {
        total.set(chunk, offset);
        offset += chunk.length;
      }

      expect(total).toEqual(data);
    });

    it('should stream with prefetch and offset', async () => {
      const data = new Uint8Array(500);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const chunks: Uint8Array[] = [];
      for await (const chunk of tree.readFileStream(cid, { offset: 200, prefetch: 3 })) {
        chunks.push(chunk);
      }

      const total = new Uint8Array(chunks.reduce((sum, c) => sum + c.length, 0));
      let offset = 0;
      for (const chunk of chunks) {
        total.set(chunk, offset);
        offset += chunk.length;
      }

      expect(total).toEqual(data.slice(200));
    });
  });

  describe('listDirectory', () => {
    it('should list directory entries', async () => {
      const { cid: c1 } = await tree.putFile(new Uint8Array([1]), { unencrypted: true });
      const { cid: c2 } = await tree.putFile(new Uint8Array([2]), { unencrypted: true });

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'first.txt', cid: c1, size: 1 },
        { name: 'second.txt', cid: c2, size: 1 },
      ], { unencrypted: true });

      const entries = await tree.listDirectory(dirCid);

      expect(entries.length).toBe(2);
      expect(entries.find(e => e.name === 'first.txt')).toBeDefined();
      expect(entries.find(e => e.name === 'second.txt')).toBeDefined();
    });

    it('should indicate which entries are trees', async () => {
      const { cid: fileCid } = await tree.putFile(new Uint8Array([1]), { unencrypted: true });
      const { cid: subDirCid } = await tree.putDirectory([
        { name: 'sub.txt', cid: fileCid },
      ], { unencrypted: true });

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, type: LinkType.Blob },
        { name: 'subdir', cid: subDirCid, type: LinkType.Dir },
      ], { unencrypted: true });

      const entries = await tree.listDirectory(rootCid);

      const fileEntry = entries.find(e => e.name === 'file.txt');
      const dirEntry = entries.find(e => e.name === 'subdir');

      expect(fileEntry!.type).toBe(LinkType.Blob);
      expect(dirEntry!.type).toBe(LinkType.Dir);
    });

    it('should flatten internal chunk nodes', async () => {
      const smallTree = new HashTree({ store, maxLinks: 3 });

      const entries = [];
      for (let i = 0; i < 10; i++) {
        const { cid } = await smallTree.putFile(new Uint8Array([i]), { unencrypted: true });
        entries.push({ name: `file${i}.txt`, cid, size: 1 });
      }

      const { cid: dirCid } = await smallTree.putDirectory(entries, { unencrypted: true });
      const listed = await tree.listDirectory(dirCid);

      // Should see all 10 files, not the internal chunk nodes
      expect(listed.length).toBe(10);
      expect(listed.every(e => e.name.startsWith('file'))).toBe(true);
    });
  });

  describe('resolvePath', () => {
    it('should resolve simple path', async () => {
      const fileData = new Uint8Array([1, 2, 3]);
      const { cid: fileCid } = await tree.putFile(fileData, { unencrypted: true });

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'test.txt', cid: fileCid },
      ], { unencrypted: true });

      const resolved = await tree.resolvePath(dirCid, 'test.txt');
      expect(toHex(resolved!.cid.hash)).toBe(toHex(fileCid.hash));
    });

    it('should resolve nested path', async () => {
      const { cid: fileCid } = await tree.putFile(new Uint8Array([1]), { unencrypted: true });

      const { cid: subSubCid } = await tree.putDirectory([
        { name: 'deep.txt', cid: fileCid },
      ], { unencrypted: true });

      const { cid: subCid } = await tree.putDirectory([
        { name: 'level2', cid: subSubCid, type: LinkType.Dir },
      ], { unencrypted: true });

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'level1', cid: subCid, type: LinkType.Dir },
      ], { unencrypted: true });

      const resolved = await tree.resolvePath(rootCid, 'level1/level2/deep.txt');
      expect(resolved).not.toBeNull();
      expect(toHex(resolved!.cid.hash)).toBe(toHex(fileCid.hash));
    });

    it('should return null for non-existent path', async () => {
      const { cid: dirCid } = await tree.putDirectory([], { unencrypted: true });
      const resolved = await tree.resolvePath(dirCid, 'missing.txt');
      expect(resolved).toBeNull();
    });

    it('should handle leading/trailing slashes', async () => {
      const { cid: fileCid } = await tree.putFile(new Uint8Array([1]), { unencrypted: true });
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'test.txt', cid: fileCid },
      ], { unencrypted: true });

      expect(await tree.resolvePath(dirCid, '/test.txt')).not.toBeNull();
      expect(await tree.resolvePath(dirCid, 'test.txt/')).not.toBeNull();
      expect(await tree.resolvePath(dirCid, '/test.txt/')).not.toBeNull();
    });
  });

  describe('getSize', () => {
    it('should return blob size', async () => {
      const data = new Uint8Array(123);
      const hash = await tree.putBlob(data);

      expect(await tree.getSize(hash)).toBe(123);
    });

    it('should return tree totalSize', async () => {
      const data = new Uint8Array(350);
      const { cid } = await tree.putFile(data, { unencrypted: true });

      expect(await tree.getSize(cid.hash)).toBe(350);
    });
  });

  describe('walk', () => {
    it('should walk entire tree', async () => {
      const { cid: f1 } = await tree.putFile(new Uint8Array([1]), { unencrypted: true });
      const { cid: f2 } = await tree.putFile(new Uint8Array([2, 3]), { unencrypted: true });

      const { cid: subCid } = await tree.putDirectory([
        { name: 'nested.txt', cid: f2, size: 2 },
      ], { unencrypted: true });

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'root.txt', cid: f1, size: 1, type: LinkType.Blob },
        { name: 'sub', cid: subCid, type: LinkType.Dir },
      ], { unencrypted: true });

      const walked: string[] = [];
      for await (const entry of tree.walk(rootCid.hash)) {
        walked.push(entry.path);
      }

      expect(walked).toContain('');
      expect(walked).toContain('root.txt');
      expect(walked).toContain('sub');
      expect(walked).toContain('sub/nested.txt');
    });
  });
});

describe('verifyTree', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store, chunkSize: 100 });
  });

  it('should return valid for complete tree', async () => {
    const data = new Uint8Array(350);
    const { cid } = await tree.putFile(data, { unencrypted: true });

    const result = await verifyTree(store, cid.hash);
    expect(result.valid).toBe(true);
    expect(result.missing).toEqual([]);
  });

  it('should detect missing chunks', async () => {
    const data = new Uint8Array(350);
    const { cid } = await tree.putFile(data, { unencrypted: true });

    // Delete one of the chunks
    const keys = store.keys();
    const chunkToDelete = keys.find(k => toHex(k) !== toHex(cid.hash));
    if (chunkToDelete) {
      await store.delete(chunkToDelete);
    }

    const result = await verifyTree(store, cid.hash);
    expect(result.valid).toBe(false);
    expect(result.missing.length).toBeGreaterThan(0);
  });

  it('should handle single blob', async () => {
    const hash = await tree.putBlob(new Uint8Array([1, 2, 3]));

    const result = await verifyTree(store, hash);
    expect(result.valid).toBe(true);
  });
});
