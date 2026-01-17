import { describe, it, expect, beforeEach } from 'vitest';
import { HashTree, MemoryStore, toHex, LinkType, type CID } from '../src/index.js';

describe('HashTree', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store });
  });

  describe('create', () => {
    it('should store small file as single blob', async () => {
      const data = new TextEncoder().encode('hello world');
      const { cid, size } = await tree.putFile(data, { unencrypted: true });

      expect(size).toBe(11);
      expect(cid.hash).toBeInstanceOf(Uint8Array);
      expect(cid.hash.length).toBe(32);

      // Should be retrievable
      const retrieved = await tree.readFile(cid);
      expect(retrieved).toEqual(data);
    });

    it('should chunk large files', async () => {
      const smallTree = new HashTree({ store, chunkSize: 10 });
      const data = new TextEncoder().encode('this is a longer message that will be chunked');
      const { cid, size } = await smallTree.putFile(data, { unencrypted: true });

      expect(size).toBe(data.length);

      // Should be retrievable
      const retrieved = await smallTree.readFile(cid);
      expect(retrieved).toEqual(data);
    });

    it('should create empty directory', async () => {
      const { cid } = await tree.putDirectory([], { unencrypted: true });

      const entries = await tree.listDirectory(cid);
      expect(entries).toHaveLength(0);
    });

    it('should create directory with entries', async () => {
      const { cid: file1 } = await tree.putFile(new TextEncoder().encode('content1'), { unencrypted: true });
      const { cid: file2 } = await tree.putFile(new TextEncoder().encode('content2'), { unencrypted: true });

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'a.txt', cid: file1, size: 8 },
        { name: 'b.txt', cid: file2, size: 8 },
      ], { unencrypted: true });

      const entries = await tree.listDirectory(dirCid);
      expect(entries).toHaveLength(2);
      expect(entries.map(e => e.name).sort()).toEqual(['a.txt', 'b.txt']);
    });
  });

  describe('read', () => {
    it('should read file', async () => {
      const data = new TextEncoder().encode('test content');
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const result = await tree.readFile(cid);
      expect(result).toEqual(data);
    });

    it('should list directory', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('data'), { unencrypted: true });
      const { cid: dirCid } = await tree.putDirectory([{ name: 'file.txt', cid: fileCid, size: 4 }], { unencrypted: true });

      const entries = await tree.listDirectory(dirCid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('file.txt');
    });

    it('should resolve path', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('nested'), { unencrypted: true });
      const { cid: subDirCid } = await tree.putDirectory([{ name: 'file.txt', cid: fileCid, size: 6 }], { unencrypted: true });
      const { cid: rootCid } = await tree.putDirectory([{ name: 'subdir', cid: subDirCid, size: 6, type: LinkType.Dir }], { unencrypted: true });

      const resolved = await tree.resolvePath(rootCid, 'subdir/file.txt');
      expect(resolved).not.toBeNull();
      expect(toHex(resolved!.cid.hash)).toBe(toHex(fileCid.hash));
    });

    it('should check if hash is directory', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('data'), { unencrypted: true });
      const { cid: dirCid } = await tree.putDirectory([], { unencrypted: true });

      expect(await tree.isDirectory(fileCid)).toBe(false);
      expect(await tree.isDirectory(dirCid)).toBe(true);
    });

    it('should stream file', async () => {
      const smallTree = new HashTree({ store, chunkSize: 5 });
      const data = new TextEncoder().encode('hello world!');
      const { cid } = await smallTree.putFile(data, { unencrypted: true });

      const chunks: Uint8Array[] = [];
      for await (const chunk of smallTree.readFileStream(cid)) {
        chunks.push(chunk);
      }

      const combined = new Uint8Array(chunks.reduce((sum, c) => sum + c.length, 0));
      let offset = 0;
      for (const chunk of chunks) {
        combined.set(chunk, offset);
        offset += chunk.length;
      }

      expect(combined).toEqual(data);
    });
  });

  describe('edit', () => {
    it('should add entry to directory', async () => {
      const { cid: rootCid } = await tree.putDirectory([], { unencrypted: true });
      const { cid: fileCid, size } = await tree.putFile(new TextEncoder().encode('hello'), { unencrypted: true });

      const newRoot = await tree.setEntry(rootCid, [], 'test.txt', fileCid, size);

      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('test.txt');
    });

    it('should update existing entry', async () => {
      const { cid: file1 } = await tree.putFile(new TextEncoder().encode('v1'), { unencrypted: true });
      const { cid: rootCid } = await tree.putDirectory([{ name: 'file.txt', cid: file1, size: 2 }], { unencrypted: true });

      const { cid: file2, size } = await tree.putFile(new TextEncoder().encode('v2 updated'), { unencrypted: true });
      const newRoot = await tree.setEntry(rootCid, [], 'file.txt', file2, size);

      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(toHex(entries[0].cid.hash)).toBe(toHex(file2.hash));
    });

    it('should remove entry', async () => {
      const { cid: file1 } = await tree.putFile(new TextEncoder().encode('a'), { unencrypted: true });
      const { cid: file2 } = await tree.putFile(new TextEncoder().encode('b'), { unencrypted: true });
      const { cid: rootCid } = await tree.putDirectory([
        { name: 'a.txt', cid: file1, size: 1 },
        { name: 'b.txt', cid: file2, size: 1 },
      ], { unencrypted: true });

      const newRoot = await tree.removeEntry(rootCid, [], 'a.txt');

      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('b.txt');
    });

    it('should rename entry', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('content'), { unencrypted: true });
      const { cid: rootCid } = await tree.putDirectory([{ name: 'old.txt', cid: fileCid, size: 7 }], { unencrypted: true });

      const newRoot = await tree.renameEntry(rootCid, [], 'old.txt', 'new.txt');

      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('new.txt');
      expect(toHex(entries[0].cid.hash)).toBe(toHex(fileCid.hash));
    });

    it('should move entry between directories', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('content'), { unencrypted: true });
      const { cid: dir1Cid } = await tree.putDirectory([{ name: 'file.txt', cid: fileCid, size: 7 }], { unencrypted: true });
      const { cid: dir2Cid } = await tree.putDirectory([], { unencrypted: true });
      const { cid: rootCid } = await tree.putDirectory([
        { name: 'dir1', cid: dir1Cid, size: 7, type: LinkType.Dir },
        { name: 'dir2', cid: dir2Cid, size: 0, type: LinkType.Dir },
      ], { unencrypted: true });

      const newRoot = await tree.moveEntry(rootCid, ['dir1'], 'file.txt', ['dir2']);

      expect(await tree.listDirectory(newRoot)).toHaveLength(2);

      const dir1Resolved = await tree.resolvePath(newRoot, 'dir1');
      const dir1Entries = await tree.listDirectory(dir1Resolved!.cid);
      expect(dir1Entries).toHaveLength(0);

      const dir2Resolved = await tree.resolvePath(newRoot, 'dir2');
      const dir2Entries = await tree.listDirectory(dir2Resolved!.cid);
      expect(dir2Entries).toHaveLength(1);
      expect(dir2Entries[0].name).toBe('file.txt');
    });

    it('should add encrypted file to public directory (streaming use case)', async () => {
      // This mirrors the streaming scenario: public directory with encrypted file entries
      const { cid: rootCid } = await tree.putDirectory([], { unencrypted: true });

      // Create encrypted file (default behavior)
      const data = new TextEncoder().encode('stream content');
      const { cid: fileCid, size } = await tree.putFile(data);

      // Add encrypted file to public directory at root path
      const newRoot = await tree.setEntry(rootCid, [], 'stream.webm', fileCid, size);

      // Verify file was added
      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('stream.webm');

      // Verify file is readable
      const content = await tree.readFile(entries[0].cid);
      expect(content).toEqual(data);
    });

    it('should handle nested path edits', async () => {
      const { cid: cCid } = await tree.putDirectory([], { unencrypted: true });
      const { cid: bCid } = await tree.putDirectory([{ name: 'c', cid: cCid, size: 0, type: LinkType.Dir }], { unencrypted: true });
      const { cid: aCid } = await tree.putDirectory([{ name: 'b', cid: bCid, size: 0, type: LinkType.Dir }], { unencrypted: true });
      const { cid: rootCid } = await tree.putDirectory([{ name: 'a', cid: aCid, size: 0, type: LinkType.Dir }], { unencrypted: true });

      const { cid: fileCid, size } = await tree.putFile(new TextEncoder().encode('deep'), { unencrypted: true });
      const newRoot = await tree.setEntry(rootCid, ['a', 'b', 'c'], 'file.txt', fileCid, size);

      // Verify nested file
      const cResolved = await tree.resolvePath(newRoot, 'a/b/c');
      const entries = await tree.listDirectory(cResolved!.cid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('file.txt');

      // Verify parent structure intact
      const aResolved = await tree.resolvePath(newRoot, 'a');
      const aEntries = await tree.listDirectory(aResolved!.cid);
      expect(aEntries).toHaveLength(1);
      expect(aEntries[0].name).toBe('b');
    });
  });

  describe('encrypted (default)', () => {
    it('should encrypt file by default', async () => {
      const data = new TextEncoder().encode('secret content');
      const { cid, size } = await tree.putFile(data);

      expect(cid.key).toBeDefined();
      expect(cid.key!.length).toBe(32);
      expect(size).toBe(14);

      // Should be retrievable with CID (has key)
      const retrieved = await tree.readFile(cid);
      expect(retrieved).toEqual(data);

      // Should NOT be readable without key (returns encrypted data or null)
      const withoutKey = await tree.readFile({ hash: cid.hash });
      expect(withoutKey).not.toEqual(data);
    });

    it('should encrypt directory by default', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('data'));
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 4 },
      ]);

      expect(dirCid.key).toBeDefined();
      expect(dirCid.key!.length).toBe(32);

      // Should list with CID (has key)
      const entries = await tree.listDirectory(dirCid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('file.txt');
      expect(entries[0].cid.key).toBeDefined();
    });

    it('should preserve isTree flag in encrypted directory', async () => {
      // Create an encrypted subdirectory
      const { cid: subDirCid } = await tree.putDirectory([]);

      // Create root directory with subdirectory entry
      const { cid: rootCid } = await tree.putDirectory([
        { name: 'subdir', cid: subDirCid, size: 0, type: LinkType.Dir },
      ]);

      // List root and check isTree
      const entries = await tree.listDirectory(rootCid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('subdir');
      expect(entries[0].type).toBe(LinkType.Dir);
      expect(entries[0].cid.key).toBeDefined();
    });

    it('should preserve isTree=false for files in encrypted directory', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('data'));
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 4, type: LinkType.Blob },
      ]);

      const entries = await tree.listDirectory(dirCid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('file.txt');
      expect(entries[0].type).toBe(LinkType.Blob);
    });

    it('should add entry to encrypted directory with setEntry', async () => {
      const { cid: rootCid } = await tree.putDirectory([]);
      const { cid: fileCid, size } = await tree.putFile(new TextEncoder().encode('hello'));

      const newRoot = await tree.setEntry(
        rootCid,
        [],
        'test.txt',
        fileCid,
        size,
        LinkType.Blob
      );

      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('test.txt');
      expect(entries[0].type).toBe(LinkType.Blob);
    });

    it('should add subdirectory to encrypted directory with setEntry', async () => {
      const { cid: rootCid } = await tree.putDirectory([]);
      const { cid: subDirCid, size } = await tree.putDirectory([]);

      const newRoot = await tree.setEntry(
        rootCid,
        [],
        'subdir',
        subDirCid,
        size,
        LinkType.Dir
      );

      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('subdir');
      expect(entries[0].type).toBe(LinkType.Dir);
    });

    it('should handle nested encrypted directories', async () => {
      // Create nested structure: root/a/b/file.txt
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('nested'));

      const { cid: bCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 6, type: LinkType.Blob },
      ]);

      const { cid: aCid } = await tree.putDirectory([
        { name: 'b', cid: bCid, size: 6, type: LinkType.Dir },
      ]);

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'a', cid: aCid, size: 6, type: LinkType.Dir },
      ]);

      // Navigate to root/a
      const rootEntries = await tree.listDirectory(rootCid);
      expect(rootEntries[0].name).toBe('a');
      expect(rootEntries[0].type).toBe(LinkType.Dir);

      // Navigate to root/a/b
      const aEntries = await tree.listDirectory(rootEntries[0].cid);
      expect(aEntries[0].name).toBe('b');
      expect(aEntries[0].type).toBe(LinkType.Dir);

      // Navigate to root/a/b/file.txt
      const bEntries = await tree.listDirectory(aEntries[0].cid);
      expect(bEntries[0].name).toBe('file.txt');
      expect(bEntries[0].type).toBe(LinkType.Blob);

      // Read the file
      const content = await tree.readFile(bEntries[0].cid);
      expect(content).toEqual(new TextEncoder().encode('nested'));
    });

    it('should resolve path through encrypted tree with resolvePath', async () => {
      // Create nested structure: root/a/b/file.txt
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('resolved'));

      const { cid: bCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 8, type: LinkType.Blob },
      ]);

      const { cid: aCid } = await tree.putDirectory([
        { name: 'b', cid: bCid, size: 8, type: LinkType.Dir },
      ]);

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'a', cid: aCid, size: 8, type: LinkType.Dir },
      ]);

      // Resolve the file path
      const resolved = await tree.resolvePath(rootCid, ['a', 'b', 'file.txt']);
      expect(resolved).not.toBeNull();
      expect(resolved!.type).toBe(LinkType.Blob);
      expect(resolved!.cid.key).toBeDefined();

      // Read the file using the resolved CID
      const content = await tree.readFile(resolved!.cid);
      expect(content).toEqual(new TextEncoder().encode('resolved'));
    });

    it('should resolve intermediate directory with resolvePath', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('data'));

      const { cid: subCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 4, type: LinkType.Blob },
      ]);

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'sub', cid: subCid, size: 4, type: LinkType.Dir },
      ]);

      // Resolve to the subdirectory
      const resolved = await tree.resolvePath(rootCid, ['sub']);
      expect(resolved).not.toBeNull();
      expect(resolved!.type).toBe(LinkType.Dir);
      expect(resolved!.cid.key).toBeDefined();

      // List the resolved directory
      const entries = await tree.listDirectory(resolved!.cid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('file.txt');
    });

    it('should return null for non-existent path in resolvePath', async () => {
      const { cid: rootCid } = await tree.putDirectory([]);

      const resolved = await tree.resolvePath(rootCid, ['nonexistent']);
      expect(resolved).toBeNull();
    });

    it('should correctly identify empty encrypted directory with isDirectory', async () => {
      // This is the bug: isDirectory returns false for empty encrypted directories
      const { cid: emptyDirCid } = await tree.putDirectory([]);

      expect(emptyDirCid.key).toBeDefined(); // Confirm it's encrypted
      expect(await tree.isDirectory(emptyDirCid)).toBe(true);
    });

    it('should resolve empty path to root directory with resolvePath', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('root'));

      const { cid: rootCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 4, type: LinkType.Blob },
      ]);

      // Empty path should return the root
      const resolved = await tree.resolvePath(rootCid, []);
      expect(resolved).not.toBeNull();
      expect(resolved!.cid.hash).toEqual(rootCid.hash);
      expect(resolved!.cid.key).toEqual(rootCid.key);
      expect(resolved!.type).toBe(LinkType.Dir);
    });
  });

  describe('pull', () => {
    it('should pull all blocks for a simple file', async () => {
      const data = new TextEncoder().encode('hello world');
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const result = await tree.pull(cid);

      expect(result.cid).toEqual(cid);
      expect(result.chunks).toBe(1);
      expect(result.bytes).toBe(11);
    });

    it('should pull all blocks for a chunked file', async () => {
      const smallTree = new HashTree({ store, chunkSize: 10 });
      const data = new TextEncoder().encode('this is a longer message that will be chunked');
      const { cid } = await smallTree.putFile(data, { unencrypted: true });

      const result = await smallTree.pull(cid);

      expect(result.cid).toEqual(cid);
      expect(result.chunks).toBeGreaterThan(1);
      expect(result.bytes).toBeGreaterThan(data.length); // includes tree nodes
    });

    it('should pull all blocks for directory with files', async () => {
      const { cid: file1 } = await tree.putFile(new TextEncoder().encode('content1'), { unencrypted: true });
      const { cid: file2 } = await tree.putFile(new TextEncoder().encode('content2'), { unencrypted: true });
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'a.txt', cid: file1, size: 8, type: LinkType.Blob },
        { name: 'b.txt', cid: file2, size: 8, type: LinkType.Blob },
      ], { unencrypted: true });

      const result = await tree.pull(dirCid);

      expect(result.cid).toEqual(dirCid);
      expect(result.chunks).toBe(3); // dir + 2 files
    });

    it('should pull encrypted directory', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('secret'));
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'secret.txt', cid: fileCid, size: 6, type: LinkType.Blob },
      ]);

      const result = await tree.pull(dirCid);

      expect(result.cid).toEqual(dirCid);
      expect(result.cid.key).toBeDefined();
      expect(result.chunks).toBeGreaterThanOrEqual(2);
    });
  });

  describe('directory metadata', () => {
    it('should store and retrieve metadata on directory entries', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('content'), { unencrypted: true });

      const createdAt = Math.floor(Date.now() / 1000);
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'video.mp4', cid: fileCid, size: 7, type: LinkType.File, meta: { createdAt, originalDate: 1609459200 } },
      ], { unencrypted: true });

      const entries = await tree.listDirectory(dirCid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('video.mp4');
      expect(entries[0].meta).toBeDefined();
      expect(entries[0].meta?.createdAt).toBe(createdAt);
      expect(entries[0].meta?.originalDate).toBe(1609459200);
    });

    it('should support metadata on directory entries in encrypted trees', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('secret'));

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 6, type: LinkType.File, meta: { author: 'test', version: 1 } },
      ]);

      expect(dirCid.key).toBeDefined();
      const entries = await tree.listDirectory(dirCid);
      expect(entries).toHaveLength(1);
      expect(entries[0].meta).toBeDefined();
      expect(entries[0].meta?.author).toBe('test');
      expect(entries[0].meta?.version).toBe(1);
    });

    it('should preserve metadata through setEntry operations', async () => {
      const { cid: rootCid } = await tree.putDirectory([], { unencrypted: true });
      const { cid: fileCid, size } = await tree.putFile(new TextEncoder().encode('hello'), { unencrypted: true });

      const meta = { createdAt: 1700000000, tags: ['video', 'imported'] };
      const newRoot = await tree.setEntry(rootCid, [], 'test.txt', fileCid, size, LinkType.File, meta);

      const entries = await tree.listDirectory(newRoot);
      expect(entries).toHaveLength(1);
      expect(entries[0].meta).toEqual(meta);
    });

    it('should allow entries without metadata', async () => {
      const { cid: file1 } = await tree.putFile(new TextEncoder().encode('with meta'), { unencrypted: true });
      const { cid: file2 } = await tree.putFile(new TextEncoder().encode('no meta'), { unencrypted: true });

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'with-meta.txt', cid: file1, size: 9, type: LinkType.File, meta: { createdAt: 123 } },
        { name: 'no-meta.txt', cid: file2, size: 7, type: LinkType.File },
      ], { unencrypted: true });

      const entries = await tree.listDirectory(dirCid);
      expect(entries).toHaveLength(2);

      const withMeta = entries.find(e => e.name === 'with-meta.txt');
      const noMeta = entries.find(e => e.name === 'no-meta.txt');

      expect(withMeta?.meta?.createdAt).toBe(123);
      expect(noMeta?.meta).toBeUndefined();
    });

    it('should support nested metadata values', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('data'), { unencrypted: true });

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'file.txt', cid: fileCid, size: 4, type: LinkType.File, meta: {
          dates: { created: 1700000000, modified: 1700001000 },
          stats: { views: 100, likes: 50 },
        } },
      ], { unencrypted: true });

      const entries = await tree.listDirectory(dirCid);
      expect(entries[0].meta?.dates).toEqual({ created: 1700000000, modified: 1700001000 });
      expect(entries[0].meta?.stats).toEqual({ views: 100, likes: 50 });
    });
  });

  describe('push', () => {
    it('should push all blocks to target store', async () => {
      const data = new TextEncoder().encode('hello world');
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const targetStore = new MemoryStore();
      const result = await tree.push(cid, targetStore);

      expect(result.cid).toEqual(cid);
      expect(result.pushed).toBe(1);
      expect(result.skipped).toBe(0);
      expect(result.bytes).toBe(11);

      // Verify data is in target store
      const retrieved = await targetStore.get(cid.hash);
      expect(retrieved).toEqual(data);
    });

    it('should push chunked file to target store', async () => {
      const smallTree = new HashTree({ store, chunkSize: 10 });
      const data = new TextEncoder().encode('this is a longer message that will be chunked');
      const { cid } = await smallTree.putFile(data, { unencrypted: true });

      const targetStore = new MemoryStore();
      const result = await smallTree.push(cid, targetStore);

      expect(result.cid).toEqual(cid);
      expect(result.pushed).toBeGreaterThan(1);

      // Verify tree can be read from target store
      const targetTree = new HashTree({ store: targetStore, chunkSize: 10 });
      const retrieved = await targetTree.readFile(cid);
      expect(retrieved).toEqual(data);
    });

    it('should push directory with files to target store', async () => {
      const { cid: file1 } = await tree.putFile(new TextEncoder().encode('content1'), { unencrypted: true });
      const { cid: file2 } = await tree.putFile(new TextEncoder().encode('content2'), { unencrypted: true });
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'a.txt', cid: file1, size: 8, type: LinkType.Blob },
        { name: 'b.txt', cid: file2, size: 8, type: LinkType.Blob },
      ], { unencrypted: true });

      const targetStore = new MemoryStore();
      const result = await tree.push(dirCid, targetStore);

      expect(result.cid).toEqual(dirCid);
      expect(result.pushed).toBe(3); // dir + 2 files

      // Verify directory can be read from target store
      const targetTree = new HashTree({ store: targetStore });
      const entries = await targetTree.listDirectory(dirCid);
      expect(entries).toHaveLength(2);
    });

    it('should skip blocks that already exist in target', async () => {
      const data = new TextEncoder().encode('hello world');
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const targetStore = new MemoryStore();

      // Push once
      const result1 = await tree.push(cid, targetStore);
      expect(result1.pushed).toBe(1);
      expect(result1.skipped).toBe(0);
      expect(result1.failed).toBe(0);

      // Push again - should skip
      const result2 = await tree.push(cid, targetStore);
      expect(result2.pushed).toBe(0);
      expect(result2.skipped).toBe(1);
      expect(result2.failed).toBe(0);
    });

    it('should handle errors and return them in errors array', async () => {
      const data = new TextEncoder().encode('hello world');
      const { cid } = await tree.putFile(data, { unencrypted: true });

      // Create a store that always throws
      const failingStore: MemoryStore = {
        ...new MemoryStore(),
        put: async () => { throw new Error('Upload failed'); },
        get: async () => null,
      };

      const result = await tree.push(cid, failingStore);

      expect(result.pushed).toBe(0);
      expect(result.failed).toBe(1);
      expect(result.errors).toHaveLength(1);
      expect(result.errors[0].error.message).toBe('Upload failed');
    });

    it('should call onBlock callback for each block', async () => {
      const data = new TextEncoder().encode('hello world');
      const { cid } = await tree.putFile(data, { unencrypted: true });

      const targetStore = new MemoryStore();
      const blockStatuses: Array<{ status: string }> = [];

      await tree.push(cid, targetStore, {
        onBlock: (hash, status) => blockStatuses.push({ status }),
      });

      expect(blockStatuses).toHaveLength(1);
      expect(blockStatuses[0].status).toBe('success');
    });

    it('should call onProgress callback', async () => {
      const smallTree = new HashTree({ store, chunkSize: 10 });
      const data = new TextEncoder().encode('this is a longer message');
      const { cid } = await smallTree.putFile(data, { unencrypted: true });

      const targetStore = new MemoryStore();
      const progressCalls: Array<[number, number]> = [];

      await smallTree.push(cid, targetStore, {
        onProgress: (current, total) => progressCalls.push([current, total]),
      });

      expect(progressCalls.length).toBeGreaterThan(0);
      // Last call should be (total, total)
      const lastCall = progressCalls[progressCalls.length - 1];
      expect(lastCall[0]).toBe(lastCall[1]);
    });

    it('should push encrypted tree to target store', async () => {
      const { cid: fileCid } = await tree.putFile(new TextEncoder().encode('secret'));
      const { cid: dirCid } = await tree.putDirectory([
        { name: 'secret.txt', cid: fileCid, size: 6, type: LinkType.Blob },
      ]);

      const targetStore = new MemoryStore();
      const result = await tree.push(dirCid, targetStore);

      expect(result.cid).toEqual(dirCid);
      expect(result.cid.key).toBeDefined();
      expect(result.pushed).toBeGreaterThanOrEqual(2);

      // Verify encrypted tree can be read from target store
      const targetTree = new HashTree({ store: targetStore });
      const entries = await targetTree.listDirectory(dirCid);
      expect(entries).toHaveLength(1);
      expect(entries[0].name).toBe('secret.txt');
    });
  });
});
