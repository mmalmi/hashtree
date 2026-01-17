import { describe, it, expect, beforeEach } from 'vitest';
import { BTree, escapeKey, unescapeKey } from '../src/btree.js';
import { MemoryStore, HashTree, type CID } from 'hashtree';

describe('BTree', () => {
  let store: MemoryStore;
  let btree: BTree;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    btree = new BTree(store, { order: 4 }); // Small order for testing splits
    tree = new HashTree({ store });
  });

  describe('basic operations', () => {
    it('should insert and get a single key', async () => {
      const root = await btree.insert(null, 'hello', 'world');
      expect(root).not.toBeNull();
      expect(root.hash).toBeInstanceOf(Uint8Array);

      const value = await btree.get(root, 'hello');
      expect(value).toBe('world');
    });

    it('should return null for non-existent key', async () => {
      const root = await btree.insert(null, 'hello', 'world');
      const value = await btree.get(root, 'notfound');
      expect(value).toBeNull();
    });

    it('should update existing key', async () => {
      let root = await btree.insert(null, 'key', 'value1');
      root = await btree.insert(root, 'key', 'value2');

      const value = await btree.get(root, 'key');
      expect(value).toBe('value2');
    });

    it('should handle multiple keys', async () => {
      let root = await btree.insert(null, 'b', '2');
      root = await btree.insert(root, 'a', '1');
      root = await btree.insert(root, 'c', '3');

      expect(await btree.get(root, 'a')).toBe('1');
      expect(await btree.get(root, 'b')).toBe('2');
      expect(await btree.get(root, 'c')).toBe('3');
    });

    it('should delete a key', async () => {
      let root = await btree.insert(null, 'a', '1');
      root = await btree.insert(root, 'b', '2');
      root = await btree.insert(root, 'c', '3');

      root = (await btree.delete(root, 'b'))!;
      expect(await btree.get(root, 'a')).toBe('1');
      expect(await btree.get(root, 'b')).toBeNull();
      expect(await btree.get(root, 'c')).toBe('3');
    });

    it('should return same root when deleting non-existent key', async () => {
      const root = await btree.insert(null, 'a', '1');
      const rootAfterDelete = await btree.delete(root, 'nonexistent');
      expect(rootAfterDelete?.hash).toEqual(root.hash);
    });
  });

  describe('node splitting', () => {
    it('should split nodes when they exceed order', async () => {
      // With order 4, max keys = 3, so 4th insert triggers split
      let root = await btree.insert(null, 'd', '4');
      root = await btree.insert(root, 'b', '2');
      root = await btree.insert(root, 'f', '6');
      root = await btree.insert(root, 'a', '1');
      root = await btree.insert(root, 'c', '3');
      root = await btree.insert(root, 'e', '5');
      root = await btree.insert(root, 'g', '7');

      // All keys should still be retrievable after splits
      expect(await btree.get(root, 'a')).toBe('1');
      expect(await btree.get(root, 'b')).toBe('2');
      expect(await btree.get(root, 'c')).toBe('3');
      expect(await btree.get(root, 'd')).toBe('4');
      expect(await btree.get(root, 'e')).toBe('5');
      expect(await btree.get(root, 'f')).toBe('6');
      expect(await btree.get(root, 'g')).toBe('7');
    });

    it('should handle many insertions', async () => {
      let root: CID | null = null;
      const count = 100;

      for (let i = 0; i < count; i++) {
        const key = i.toString().padStart(5, '0');
        root = await btree.insert(root, key, `value${i}`);
      }

      // Verify all keys exist
      for (let i = 0; i < count; i++) {
        const key = i.toString().padStart(5, '0');
        expect(await btree.get(root, key)).toBe(`value${i}`);
      }
    });
  });

  describe('range queries', () => {
    it('should iterate all keys in order', async () => {
      let root = await btree.insert(null, 'c', '3');
      root = await btree.insert(root, 'a', '1');
      root = await btree.insert(root, 'b', '2');
      root = await btree.insert(root, 'd', '4');

      const entries: Array<[string, string]> = [];
      for await (const [k, v] of btree.entries(root)) {
        entries.push([k, v]);
      }

      expect(entries).toEqual([
        ['a', '1'],
        ['b', '2'],
        ['c', '3'],
        ['d', '4'],
      ]);
    });

    it('should range query with start and end', async () => {
      let root: CID | null = null;
      for (const k of ['a', 'b', 'c', 'd', 'e', 'f']) {
        root = await btree.insert(root, k, k.toUpperCase());
      }

      const entries: Array<[string, string]> = [];
      for await (const [k, v] of btree.range(root!, 'b', 'e')) {
        entries.push([k, v]);
      }

      // Range is inclusive of start, exclusive of end
      expect(entries).toEqual([
        ['b', 'B'],
        ['c', 'C'],
        ['d', 'D'],
      ]);
    });

    it('should range query with only start (gte)', async () => {
      let root: CID | null = null;
      for (const k of ['a', 'b', 'c', 'd', 'e']) {
        root = await btree.insert(root, k, k.toUpperCase());
      }

      const entries: Array<[string, string]> = [];
      for await (const [k, v] of btree.range(root!, 'c')) {
        entries.push([k, v]);
      }

      expect(entries).toEqual([
        ['c', 'C'],
        ['d', 'D'],
        ['e', 'E'],
      ]);
    });

    it('should range query with prefix', async () => {
      let root: CID | null = null;
      const keys = ['user:001', 'user:002', 'user:003', 'other:001', 'user:004'];
      for (const k of keys) {
        root = await btree.insert(root, k, k);
      }

      const entries: Array<[string, string]> = [];
      for await (const [k, v] of btree.prefix(root!, 'user:')) {
        entries.push([k, v]);
      }

      expect(entries).toEqual([
        ['user:001', 'user:001'],
        ['user:002', 'user:002'],
        ['user:003', 'user:003'],
        ['user:004', 'user:004'],
      ]);
    });
  });

  describe('compound keys for Nostr', () => {
    it('should support events_by_pubkey_and_time pattern', async () => {
      let root: CID | null = null;

      // Insert events with compound keys: pubkey:timestamp
      const pubkey1 = 'npub1abc';
      const pubkey2 = 'npub1def';

      root = await btree.insert(root, `${pubkey1}:0000001000`, 'event1');
      root = await btree.insert(root, `${pubkey1}:0000002000`, 'event2');
      root = await btree.insert(root, `${pubkey1}:0000003000`, 'event3');
      root = await btree.insert(root, `${pubkey2}:0000001500`, 'event4');
      root = await btree.insert(root, `${pubkey2}:0000002500`, 'event5');

      // Query all events from pubkey1
      const pubkey1Events: string[] = [];
      for await (const [, v] of btree.prefix(root!, `${pubkey1}:`)) {
        pubkey1Events.push(v);
      }
      expect(pubkey1Events).toEqual(['event1', 'event2', 'event3']);

      // Query events from pubkey1 in time range [1500, 2500)
      const rangeEvents: string[] = [];
      for await (const [, v] of btree.range(root!, `${pubkey1}:0000001500`, `${pubkey1}:0000002500`)) {
        rangeEvents.push(v);
      }
      expect(rangeEvents).toEqual(['event2']);
    });

    it('should support events_by_pubkey_and_kind_and_time pattern', async () => {
      let root: CID | null = null;

      const pubkey = 'npub1abc';
      // Format: pubkey:kind:timestamp
      root = await btree.insert(root, `${pubkey}:00001:0000001000`, 'note1');
      root = await btree.insert(root, `${pubkey}:00001:0000002000`, 'note2');
      root = await btree.insert(root, `${pubkey}:00007:0000001500`, 'reaction1');
      root = await btree.insert(root, `${pubkey}:00001:0000003000`, 'note3');

      // Query all kind:1 notes from pubkey
      const notes: string[] = [];
      for await (const [, v] of btree.prefix(root!, `${pubkey}:00001:`)) {
        notes.push(v);
      }
      expect(notes).toEqual(['note1', 'note2', 'note3']);

      // Query all kind:7 reactions from pubkey
      const reactions: string[] = [];
      for await (const [, v] of btree.prefix(root!, `${pubkey}:00007:`)) {
        reactions.push(v);
      }
      expect(reactions).toEqual(['reaction1']);
    });
  });

  describe('persistence', () => {
    it('should persist to store and be loadable', async () => {
      let root = await btree.insert(null, 'key1', 'value1');
      root = await btree.insert(root, 'key2', 'value2');

      // Create new BTree instance with same store
      const btree2 = new BTree(store, { order: 4 });

      // Should be able to read from the persisted root
      expect(await btree2.get(root, 'key1')).toBe('value1');
      expect(await btree2.get(root, 'key2')).toBe('value2');
    });

    it('should produce deterministic hashes for same content', async () => {
      const store1 = new MemoryStore();
      const store2 = new MemoryStore();
      const bt1 = new BTree(store1, { order: 4 });
      const bt2 = new BTree(store2, { order: 4 });

      let root1 = await bt1.insert(null, 'a', '1');
      root1 = await bt1.insert(root1, 'b', '2');

      let root2 = await bt2.insert(null, 'a', '1');
      root2 = await bt2.insert(root2, 'b', '2');

      // Same operations should produce same root hash
      expect(root1.hash).toEqual(root2.hash);
    });
  });

  describe('browsable structure', () => {
    it('should create browsable directory structure', async () => {
      let root = await btree.insert(null, 'apple', 'fruit');
      root = await btree.insert(root, 'banana', 'fruit');

      // Root should be a directory with entries named after keys
      const entries = await tree.listDirectory(root);
      expect(entries.length).toBe(2);

      const names = entries.map(e => e.name).sort();
      expect(names).toEqual(['apple', 'banana']);

      // Values should be readable as files
      const appleEntry = entries.find(e => e.name === 'apple')!;
      const value = await tree.readFile(appleEntry.cid);
      expect(new TextDecoder().decode(value!)).toBe('fruit');
    });

    it('should create nested directories after split', async () => {
      // Insert enough to trigger splits (order 4, max 3 keys)
      let root = await btree.insert(null, 'a', '1');
      root = await btree.insert(root, 'b', '2');
      root = await btree.insert(root, 'c', '3');
      root = await btree.insert(root, 'd', '4'); // This should trigger a split

      // Root should now have child directories
      const entries = await tree.listDirectory(root);

      // After split, root contains directories (internal node)
      const hasDirs = entries.some(e => e.type === 2); // LinkType.Dir = 2
      expect(hasDirs).toBe(true);

      // Child directories are named after first key in subtree
      const names = entries.map(e => e.name).sort();
      expect(names.length).toBe(2); // Two children after split
    });

    it('should use directory names as boundaries (no maxKey needed)', async () => {
      let root = await btree.insert(null, 'a', '1');
      root = await btree.insert(root, 'b', '2');
      root = await btree.insert(root, 'c', '3');
      root = await btree.insert(root, 'd', '4');
      root = await btree.insert(root, 'e', '5');

      const entries = await tree.listDirectory(root);

      // Directories are named after first key - that IS the boundary
      // No meta.maxKey needed - next sibling's name is the upper bound
      const names = entries.map(e => e.name).sort();
      expect(names.length).toBeGreaterThanOrEqual(2);

      // Verify structure is self-describing
      for (const entry of entries) {
        expect(entry.meta?.maxKey).toBeUndefined(); // No maxKey metadata
      }
    });
  });

  describe('key escaping', () => {
    it('should handle keys with slashes', async () => {
      let root = await btree.insert(null, 'path/to/file', 'content');
      expect(await btree.get(root, 'path/to/file')).toBe('content');

      // Verify escaped name in directory
      const entries = await tree.listDirectory(root);
      expect(entries[0].name).toBe('path%2Fto%2Ffile');
    });

    it('should handle keys with percent signs', async () => {
      let root = await btree.insert(null, '100%', 'value');
      expect(await btree.get(root, '100%')).toBe('value');

      const entries = await tree.listDirectory(root);
      expect(entries[0].name).toBe('100%25');
    });

    it('should roundtrip escape/unescape', () => {
      const cases = [
        'simple',
        'with/slash',
        'with%percent',
        'mixed/path%value',
        '100%',
        'a/b/c',
      ];

      for (const input of cases) {
        expect(unescapeKey(escapeKey(input))).toBe(input);
      }
    });
  });

  describe('CID link operations', () => {
    it('should insert and get a CID link', async () => {
      // Create a dummy CID by storing a file
      const { cid: targetCid } = await tree.putFile(new TextEncoder().encode('video content'));

      const root = await btree.insertLink(null, 'video:001', targetCid);
      expect(root).not.toBeNull();

      const retrieved = await btree.getLink(root, 'video:001');
      expect(retrieved).not.toBeNull();
      expect(retrieved!.hash).toEqual(targetCid.hash);
    });

    it('should return null for non-existent CID link', async () => {
      const { cid: targetCid } = await tree.putFile(new TextEncoder().encode('content'));
      const root = await btree.insertLink(null, 'key', targetCid);

      const retrieved = await btree.getLink(root, 'nonexistent');
      expect(retrieved).toBeNull();
    });

    it('should return same root when inserting same CID link', async () => {
      const { cid: targetCid } = await tree.putFile(new TextEncoder().encode('content'));

      const root1 = await btree.insertLink(null, 'key', targetCid);
      const root2 = await btree.insertLink(root1, 'key', targetCid);

      // Same CID inserted -> should return same root (early exit)
      expect(root1.hash).toEqual(root2.hash);
    });

    it('should update CID link with new value', async () => {
      const { cid: cid1 } = await tree.putFile(new TextEncoder().encode('content1'));
      const { cid: cid2 } = await tree.putFile(new TextEncoder().encode('content2'));

      let root = await btree.insertLink(null, 'key', cid1);
      root = await btree.insertLink(root, 'key', cid2);

      const retrieved = await btree.getLink(root, 'key');
      expect(retrieved!.hash).toEqual(cid2.hash);
    });

    it('should handle multiple CID links', async () => {
      const { cid: cid1 } = await tree.putFile(new TextEncoder().encode('video1'));
      const { cid: cid2 } = await tree.putFile(new TextEncoder().encode('video2'));
      const { cid: cid3 } = await tree.putFile(new TextEncoder().encode('video3'));

      let root = await btree.insertLink(null, 'video:002', cid2);
      root = await btree.insertLink(root, 'video:001', cid1);
      root = await btree.insertLink(root, 'video:003', cid3);

      expect((await btree.getLink(root, 'video:001'))!.hash).toEqual(cid1.hash);
      expect((await btree.getLink(root, 'video:002'))!.hash).toEqual(cid2.hash);
      expect((await btree.getLink(root, 'video:003'))!.hash).toEqual(cid3.hash);
    });

    it('should iterate all CID links in order', async () => {
      const { cid: cid1 } = await tree.putFile(new TextEncoder().encode('c'));
      const { cid: cid2 } = await tree.putFile(new TextEncoder().encode('a'));
      const { cid: cid3 } = await tree.putFile(new TextEncoder().encode('b'));

      let root = await btree.insertLink(null, 'c', cid1);
      root = await btree.insertLink(root, 'a', cid2);
      root = await btree.insertLink(root, 'b', cid3);

      const entries: Array<[string, CID]> = [];
      for await (const entry of btree.linksEntries(root)) {
        entries.push(entry);
      }

      expect(entries.length).toBe(3);
      expect(entries[0][0]).toBe('a');
      expect(entries[1][0]).toBe('b');
      expect(entries[2][0]).toBe('c');
    });

    it('should prefix search CID links', async () => {
      const { cid: cid1 } = await tree.putFile(new TextEncoder().encode('1'));
      const { cid: cid2 } = await tree.putFile(new TextEncoder().encode('2'));
      const { cid: cid3 } = await tree.putFile(new TextEncoder().encode('3'));
      const { cid: cid4 } = await tree.putFile(new TextEncoder().encode('4'));

      let root = await btree.insertLink(null, 'video:001', cid1);
      root = await btree.insertLink(root, 'video:002', cid2);
      root = await btree.insertLink(root, 'audio:001', cid3);
      root = await btree.insertLink(root, 'video:003', cid4);

      const videoLinks: string[] = [];
      for await (const [key] of btree.prefixLinks(root, 'video:')) {
        videoLinks.push(key);
      }

      expect(videoLinks).toEqual(['video:001', 'video:002', 'video:003']);
    });

    it('should merge CID link trees', async () => {
      const { cid: cid1 } = await tree.putFile(new TextEncoder().encode('1'));
      const { cid: cid2 } = await tree.putFile(new TextEncoder().encode('2'));
      const { cid: cid3 } = await tree.putFile(new TextEncoder().encode('3'));

      let base = await btree.insertLink(null, 'a', cid1);
      base = await btree.insertLink(base, 'b', cid2);

      let other = await btree.insertLink(null, 'b', cid3); // duplicate key with different value
      other = await btree.insertLink(other, 'c', cid3);

      // Default: keep base value for duplicates
      const merged = await btree.mergeLinks(base, other, false);

      expect((await btree.getLink(merged, 'a'))!.hash).toEqual(cid1.hash);
      expect((await btree.getLink(merged, 'b'))!.hash).toEqual(cid2.hash); // keeps base
      expect((await btree.getLink(merged, 'c'))!.hash).toEqual(cid3.hash);
    });

    it('should merge with preferOther=true to overwrite duplicates', async () => {
      const { cid: cid1 } = await tree.putFile(new TextEncoder().encode('base'));
      const { cid: cid2 } = await tree.putFile(new TextEncoder().encode('other'));

      const base = await btree.insertLink(null, 'key', cid1);
      const other = await btree.insertLink(null, 'key', cid2);

      const merged = await btree.mergeLinks(base, other, true);

      expect((await btree.getLink(merged, 'key'))!.hash).toEqual(cid2.hash); // prefers other
    });

    it('should handle CID with encryption key', async () => {
      // Create a CID with an encryption key
      const data = new TextEncoder().encode('encrypted content');
      const { cid: targetCid } = await tree.putFile(data);
      const cidWithKey: CID = {
        hash: targetCid.hash,
        key: new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
      };

      const root = await btree.insertLink(null, 'encrypted', cidWithKey);
      const retrieved = await btree.getLink(root, 'encrypted');

      expect(retrieved).not.toBeNull();
      expect(retrieved!.hash).toEqual(cidWithKey.hash);
      expect(retrieved!.key).toEqual(cidWithKey.key);
    });

    it('should detect CID equality including encryption key', async () => {
      const hash = new Uint8Array(32).fill(1);
      const key1 = new Uint8Array(16).fill(2);
      const key2 = new Uint8Array(16).fill(3);

      const cid1: CID = { hash };
      const cid2: CID = { hash, key: key1 };
      const cid3: CID = { hash, key: key2 };

      // Store the hash in the store for the BTree to work
      await store.put(hash, new TextEncoder().encode('dummy'));

      const root1 = await btree.insertLink(null, 'key', cid1);
      const root2 = await btree.insertLink(null, 'key', cid2);
      const root3 = await btree.insertLink(null, 'key', cid3);

      // Same hash without key -> same root
      const root1b = await btree.insertLink(root1, 'key', cid1);
      expect(root1b.hash).toEqual(root1.hash);

      // Same hash with same key -> same root
      const root2b = await btree.insertLink(root2, 'key', cid2);
      expect(root2b.hash).toEqual(root2.hash);

      // Same hash with different key -> different root
      const root2c = await btree.insertLink(root2, 'key', cid3);
      expect(root2c.hash).not.toEqual(root2.hash);
    });

    it('should split nodes with CID links when exceeding order', async () => {
      // With order 4, max keys = 3, so 4th insert triggers split
      const cids: CID[] = [];
      for (let i = 0; i < 7; i++) {
        const { cid } = await tree.putFile(new TextEncoder().encode(`content${i}`));
        cids.push(cid);
      }

      let root = await btree.insertLink(null, 'd', cids[3]);
      root = await btree.insertLink(root, 'b', cids[1]);
      root = await btree.insertLink(root, 'f', cids[5]);
      root = await btree.insertLink(root, 'a', cids[0]);
      root = await btree.insertLink(root, 'c', cids[2]);
      root = await btree.insertLink(root, 'e', cids[4]);
      root = await btree.insertLink(root, 'g', cids[6]);

      // All links should still be retrievable after splits
      expect((await btree.getLink(root, 'a'))!.hash).toEqual(cids[0].hash);
      expect((await btree.getLink(root, 'b'))!.hash).toEqual(cids[1].hash);
      expect((await btree.getLink(root, 'c'))!.hash).toEqual(cids[2].hash);
      expect((await btree.getLink(root, 'd'))!.hash).toEqual(cids[3].hash);
      expect((await btree.getLink(root, 'e'))!.hash).toEqual(cids[4].hash);
      expect((await btree.getLink(root, 'f'))!.hash).toEqual(cids[5].hash);
      expect((await btree.getLink(root, 'g'))!.hash).toEqual(cids[6].hash);
    });

    it('should handle many CID link insertions', async () => {
      let root: CID | null = null;
      const count = 50;
      const cids: Map<string, CID> = new Map();

      for (let i = 0; i < count; i++) {
        const key = `key:${i.toString().padStart(5, '0')}`;
        const { cid } = await tree.putFile(new TextEncoder().encode(`value${i}`));
        cids.set(key, cid);
        root = await btree.insertLink(root, key, cid);
      }

      // Verify all links exist
      for (const [key, expectedCid] of cids) {
        const retrieved = await btree.getLink(root, key);
        expect(retrieved).not.toBeNull();
        expect(retrieved!.hash).toEqual(expectedCid.hash);
      }
    });

    it('should use LinkType.File for CID links (browsable structure)', async () => {
      const { cid: targetCid } = await tree.putFile(new TextEncoder().encode('content'));

      const root = await btree.insertLink(null, 'mylink', targetCid);

      // Root should have an entry with LinkType.File (not Blob)
      const entries = await tree.listDirectory(root);
      expect(entries.length).toBe(1);
      expect(entries[0].name).toBe('mylink');
      expect(entries[0].type).toBe(1); // LinkType.File = 1
      expect(entries[0].cid.hash).toEqual(targetCid.hash);
    });
  });

  describe('edge cases', () => {
    it('should handle empty tree', async () => {
      const entries: Array<[string, string]> = [];
      for await (const entry of btree.entries(null)) {
        entries.push(entry);
      }
      expect(entries).toEqual([]);
    });

    it('should handle single key tree', async () => {
      const root = await btree.insert(null, 'only', 'one');

      const entries: Array<[string, string]> = [];
      for await (const entry of btree.entries(root)) {
        entries.push(entry);
      }
      expect(entries).toEqual([['only', 'one']]);
    });

    it('should handle keys with special characters', async () => {
      let root = await btree.insert(null, 'key:with:colons', 'v1');
      root = await btree.insert(root, 'key/with/slashes', 'v2');
      root = await btree.insert(root, 'key with spaces', 'v3');

      expect(await btree.get(root, 'key:with:colons')).toBe('v1');
      expect(await btree.get(root, 'key/with/slashes')).toBe('v2');
      expect(await btree.get(root, 'key with spaces')).toBe('v3');
    });

    it('should handle unicode keys', async () => {
      let root = await btree.insert(null, '日本語', 'japanese');
      root = await btree.insert(root, 'émoji🎉', 'party');

      expect(await btree.get(root, '日本語')).toBe('japanese');
      expect(await btree.get(root, 'émoji🎉')).toBe('party');

      // Unicode should be preserved in directory names (not escaped)
      const entries = await tree.listDirectory(root);
      const names = entries.map(e => e.name).sort();
      expect(names).toContain('日本語');
      expect(names).toContain('émoji🎉');
    });
  });
});
