import { describe, it, expect, beforeEach } from 'vitest';
import {
  BEP52_BLOCK_SIZE,
  ZERO_HASH,
  Bep52TreeBuilder,
  Bep52StreamBuilder,
  merkleNumLeafs,
  merkleGetParent,
  merkleGetSibling,
  merkleGetFirstChild,
  merkleFirstLeaf,
  merkleNumNodes,
  merkleHashPair,
  merklePadHash,
  merkleRoot,
  merkleBuildTree,
  merkleGetProof,
  merkleVerifyProof,
} from '../src/bep52.js';
import { MemoryStore } from '../src/store/memory.js';
import { toHex, hashEquals } from '../src/types.js';
import { sha256 } from '../src/hash.js';

describe('BEP52 Constants', () => {
  it('should have correct block size', () => {
    expect(BEP52_BLOCK_SIZE).toBe(16 * 1024); // 16 KiB
  });

  it('should have correct zero hash', () => {
    expect(ZERO_HASH.length).toBe(32);
    expect(ZERO_HASH.every(b => b === 0)).toBe(true);
  });
});

describe('BEP52 Merkle Tree Functions', () => {
  describe('merkleNumLeafs', () => {
    // Based on libtorrent test_merkle.cpp
    it('should round up to power of 2', () => {
      expect(merkleNumLeafs(1)).toBe(1);
      expect(merkleNumLeafs(2)).toBe(2);
      expect(merkleNumLeafs(3)).toBe(4);
      expect(merkleNumLeafs(4)).toBe(4);
      expect(merkleNumLeafs(5)).toBe(8);
      expect(merkleNumLeafs(6)).toBe(8);
      expect(merkleNumLeafs(7)).toBe(8);
      expect(merkleNumLeafs(8)).toBe(8);
      expect(merkleNumLeafs(9)).toBe(16);
      expect(merkleNumLeafs(10)).toBe(16);
    });

    it('should return 0 for empty', () => {
      expect(merkleNumLeafs(0)).toBe(0);
    });
  });

  describe('merkleGetParent', () => {
    // Tree structure:
    //             0
    //      1              2
    //   3      4       5       6
    //  7 8    9 10   11 12   13 14
    it('should return correct parents', () => {
      expect(merkleGetParent(1)).toBe(0);
      expect(merkleGetParent(2)).toBe(0);
      expect(merkleGetParent(3)).toBe(1);
      expect(merkleGetParent(4)).toBe(1);
      expect(merkleGetParent(5)).toBe(2);
      expect(merkleGetParent(6)).toBe(2);
      expect(merkleGetParent(7)).toBe(3);
      expect(merkleGetParent(8)).toBe(3);
      expect(merkleGetParent(9)).toBe(4);
      expect(merkleGetParent(10)).toBe(4);
      expect(merkleGetParent(11)).toBe(5);
      expect(merkleGetParent(12)).toBe(5);
      expect(merkleGetParent(13)).toBe(6);
      expect(merkleGetParent(14)).toBe(6);
    });
  });

  describe('merkleGetSibling', () => {
    it('should return correct siblings', () => {
      expect(merkleGetSibling(1)).toBe(2);
      expect(merkleGetSibling(2)).toBe(1);
      expect(merkleGetSibling(3)).toBe(4);
      expect(merkleGetSibling(4)).toBe(3);
      expect(merkleGetSibling(5)).toBe(6);
      expect(merkleGetSibling(6)).toBe(5);
      expect(merkleGetSibling(7)).toBe(8);
      expect(merkleGetSibling(8)).toBe(7);
    });
  });

  describe('merkleGetFirstChild', () => {
    it('should return correct first child', () => {
      expect(merkleGetFirstChild(0)).toBe(1);
      expect(merkleGetFirstChild(1)).toBe(3);
      expect(merkleGetFirstChild(2)).toBe(5);
      expect(merkleGetFirstChild(3)).toBe(7);
      expect(merkleGetFirstChild(4)).toBe(9);
      expect(merkleGetFirstChild(5)).toBe(11);
      expect(merkleGetFirstChild(6)).toBe(13);
    });
  });

  describe('merkleFirstLeaf', () => {
    it('should return correct first leaf index', () => {
      expect(merkleFirstLeaf(1)).toBe(0);
      expect(merkleFirstLeaf(2)).toBe(1);
      expect(merkleFirstLeaf(4)).toBe(3);
      expect(merkleFirstLeaf(8)).toBe(7);
      expect(merkleFirstLeaf(16)).toBe(15);
    });
  });

  describe('merkleNumNodes', () => {
    it('should return correct total nodes', () => {
      expect(merkleNumNodes(1)).toBe(1);
      expect(merkleNumNodes(2)).toBe(3);
      expect(merkleNumNodes(4)).toBe(7);
      expect(merkleNumNodes(8)).toBe(15);
      expect(merkleNumNodes(16)).toBe(31);
    });
  });

  describe('merkleHashPair', () => {
    it('should hash two hashes together', async () => {
      const left = new Uint8Array(32).fill(1);
      const right = new Uint8Array(32).fill(2);

      const combined = new Uint8Array(64);
      combined.set(left, 0);
      combined.set(right, 32);
      const expected = await sha256(combined);

      const result = await merkleHashPair(left, right);
      expect(toHex(result)).toBe(toHex(expected));
    });

    it('should produce different results for different order', async () => {
      const a = new Uint8Array(32).fill(1);
      const b = new Uint8Array(32).fill(2);

      const ab = await merkleHashPair(a, b);
      const ba = await merkleHashPair(b, a);

      expect(toHex(ab)).not.toBe(toHex(ba));
    });
  });

  describe('merklePadHash', () => {
    it('should return zero hash at depth 0', async () => {
      const pad = await merklePadHash(0);
      expect(toHex(pad)).toBe(toHex(ZERO_HASH));
    });

    it('should return H(0||0) at depth 1', async () => {
      const pad = await merklePadHash(1);
      const expected = await merkleHashPair(ZERO_HASH, ZERO_HASH);
      expect(toHex(pad)).toBe(toHex(expected));
    });

    it('should recursively compute pad at depth 2', async () => {
      const pad1 = await merklePadHash(1);
      const pad2 = await merklePadHash(2);
      const expected = await merkleHashPair(pad1, pad1);
      expect(toHex(pad2)).toBe(toHex(expected));
    });
  });

  describe('merkleRoot', () => {
    it('should return single leaf as root', async () => {
      const leaf = new Uint8Array(32).fill(42);
      const root = await merkleRoot([leaf]);
      expect(toHex(root)).toBe(toHex(leaf));
    });

    it('should compute root of two leaves', async () => {
      const a = new Uint8Array(32).fill(1);
      const b = new Uint8Array(32).fill(2);

      const root = await merkleRoot([a, b], 2);
      const expected = await merkleHashPair(a, b);

      expect(toHex(root)).toBe(toHex(expected));
    });

    it('should compute root with padding for 3 leaves', async () => {
      const a = new Uint8Array(32).fill(1);
      const b = new Uint8Array(32).fill(2);
      const c = new Uint8Array(32).fill(3);

      // Tree: ((a,b), (c,0))
      const root = await merkleRoot([a, b, c], 4);

      const ab = await merkleHashPair(a, b);
      const c0 = await merkleHashPair(c, ZERO_HASH);
      const expected = await merkleHashPair(ab, c0);

      expect(toHex(root)).toBe(toHex(expected));
    });

    it('should return zero hash for empty', async () => {
      const root = await merkleRoot([]);
      expect(toHex(root)).toBe(toHex(ZERO_HASH));
    });
  });

  describe('merkleBuildTree', () => {
    it('should build complete tree for 4 leaves', async () => {
      const leaves = [
        new Uint8Array(32).fill(1),
        new Uint8Array(32).fill(2),
        new Uint8Array(32).fill(3),
        new Uint8Array(32).fill(4),
      ];

      const tree = await merkleBuildTree(leaves);

      // Tree should have 7 nodes (2*4 - 1)
      expect(tree.length).toBe(7);

      // Leaves should be at indices 3-6
      expect(toHex(tree[3])).toBe(toHex(leaves[0]));
      expect(toHex(tree[4])).toBe(toHex(leaves[1]));
      expect(toHex(tree[5])).toBe(toHex(leaves[2]));
      expect(toHex(tree[6])).toBe(toHex(leaves[3]));

      // Parents should be computed
      const expected12 = await merkleHashPair(leaves[0], leaves[1]);
      const expected34 = await merkleHashPair(leaves[2], leaves[3]);
      expect(toHex(tree[1])).toBe(toHex(expected12));
      expect(toHex(tree[2])).toBe(toHex(expected34));

      // Root
      const expectedRoot = await merkleHashPair(expected12, expected34);
      expect(toHex(tree[0])).toBe(toHex(expectedRoot));
    });

    it('should pad with zeros for 3 leaves', async () => {
      const leaves = [
        new Uint8Array(32).fill(1),
        new Uint8Array(32).fill(2),
        new Uint8Array(32).fill(3),
      ];

      const tree = await merkleBuildTree(leaves);

      // Needs 4 leaves, so 7 nodes
      expect(tree.length).toBe(7);

      // 4th leaf should be zero
      expect(toHex(tree[6])).toBe(toHex(ZERO_HASH));
    });
  });

  describe('merkleGetProof and merkleVerifyProof', () => {
    it('should generate and verify proof for 4 leaves', async () => {
      const leaves = [
        new Uint8Array(32).fill(1),
        new Uint8Array(32).fill(2),
        new Uint8Array(32).fill(3),
        new Uint8Array(32).fill(4),
      ];

      const tree = await merkleBuildTree(leaves);
      const root = tree[0];

      // Verify proof for each leaf
      for (let i = 0; i < 4; i++) {
        const proof = merkleGetProof(tree, i, 4);
        const valid = await merkleVerifyProof(leaves[i], i, proof, root, 4);
        expect(valid).toBe(true);
      }
    });

    it('should reject invalid proof', async () => {
      const leaves = [
        new Uint8Array(32).fill(1),
        new Uint8Array(32).fill(2),
        new Uint8Array(32).fill(3),
        new Uint8Array(32).fill(4),
      ];

      const tree = await merkleBuildTree(leaves);
      const root = tree[0];

      const proof = merkleGetProof(tree, 0, 4);

      // Wrong leaf should fail
      const wrongLeaf = new Uint8Array(32).fill(99);
      const valid = await merkleVerifyProof(wrongLeaf, 0, proof, root, 4);
      expect(valid).toBe(false);
    });

    it('should reject wrong index', async () => {
      const leaves = [
        new Uint8Array(32).fill(1),
        new Uint8Array(32).fill(2),
      ];

      const tree = await merkleBuildTree(leaves);
      const root = tree[0];
      const proof = merkleGetProof(tree, 0, 2);

      // Using proof for leaf 0 with leaf 1 should fail
      const valid = await merkleVerifyProof(leaves[1], 0, proof, root, 2);
      expect(valid).toBe(false);
    });
  });
});

describe('Bep52TreeBuilder', () => {
  let store: MemoryStore;

  beforeEach(() => {
    store = new MemoryStore();
  });

  describe('constructor', () => {
    it('should accept default config', () => {
      const builder = new Bep52TreeBuilder();
      expect(builder).toBeInstanceOf(Bep52TreeBuilder);
    });

    it('should accept custom piece size', () => {
      const builder = new Bep52TreeBuilder({ pieceSize: 32 * 1024 });
      expect(builder).toBeInstanceOf(Bep52TreeBuilder);
    });

    it('should reject piece size < 16KB', () => {
      expect(() => new Bep52TreeBuilder({ pieceSize: 8 * 1024 })).toThrow();
    });

    it('should reject non-power-of-2 piece size', () => {
      expect(() => new Bep52TreeBuilder({ pieceSize: 20 * 1024 })).toThrow();
    });
  });

  describe('buildFromData', () => {
    it('should handle empty data', async () => {
      const builder = new Bep52TreeBuilder({ store });
      const result = await builder.buildFromData(new Uint8Array(0));

      expect(result.size).toBe(0);
      expect(result.blockCount).toBe(0);
      expect(result.leafHashes.length).toBe(0);
      expect(toHex(result.root)).toBe(toHex(ZERO_HASH));
    });

    it('should hash single small block', async () => {
      const builder = new Bep52TreeBuilder({ store });
      const data = new Uint8Array([1, 2, 3, 4, 5]);
      const result = await builder.buildFromData(data);

      expect(result.size).toBe(5);
      expect(result.blockCount).toBe(1);
      expect(result.leafHashes.length).toBe(1);

      // Root should equal the single leaf hash
      const expectedHash = await sha256(data);
      expect(toHex(result.root)).toBe(toHex(expectedHash));
      expect(toHex(result.leafHashes[0])).toBe(toHex(expectedHash));
    });

    it('should chunk large data into 16KB blocks', async () => {
      const builder = new Bep52TreeBuilder({ store });
      const data = new Uint8Array(BEP52_BLOCK_SIZE * 3 + 100);
      data.fill(42);

      const result = await builder.buildFromData(data);

      expect(result.size).toBe(data.length);
      expect(result.blockCount).toBe(4); // 3 full + 1 partial
      expect(result.leafHashes.length).toBe(4);
    });

    it('should store blocks in store', async () => {
      const builder = new Bep52TreeBuilder({ store });
      const data = new Uint8Array(100);
      data.fill(123);

      await builder.buildFromData(data);

      // Block should be stored
      const hash = await sha256(data);
      expect(await store.has(hash)).toBe(true);
      expect(await store.get(hash)).toEqual(data);
    });

    it('should work without store (hash-only mode)', async () => {
      const builder = new Bep52TreeBuilder(); // No store
      const data = new Uint8Array(100);
      data.fill(42);

      const result = await builder.buildFromData(data);

      expect(result.size).toBe(100);
      expect(result.blockCount).toBe(1);
      expect(result.leafHashes.length).toBe(1);
    });

    it('should produce deterministic roots', async () => {
      const builder = new Bep52TreeBuilder();
      const data = new Uint8Array(BEP52_BLOCK_SIZE * 2);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }

      const result1 = await builder.buildFromData(data);
      const result2 = await builder.buildFromData(data);

      expect(toHex(result1.root)).toBe(toHex(result2.root));
    });
  });

  describe('piece layers', () => {
    it('should not compute piece layers when pieceSize equals blockSize', async () => {
      const builder = new Bep52TreeBuilder({ pieceSize: BEP52_BLOCK_SIZE });
      const data = new Uint8Array(BEP52_BLOCK_SIZE * 4);

      const result = await builder.buildFromData(data);

      expect(result.pieceLayers).toBeUndefined();
    });

    it('should compute piece layers for larger piece size', async () => {
      const builder = new Bep52TreeBuilder({ pieceSize: BEP52_BLOCK_SIZE * 2 });
      const data = new Uint8Array(BEP52_BLOCK_SIZE * 4);
      data.fill(42);

      const result = await builder.buildFromData(data);

      // 4 blocks / 2 blocks per piece = 2 piece layers
      expect(result.pieceLayers).toBeDefined();
      expect(result.pieceLayers!.length).toBe(2);
    });

    it('should compute correct piece layer hashes', async () => {
      const builder = new Bep52TreeBuilder({ pieceSize: BEP52_BLOCK_SIZE * 2 });

      // Create 2 blocks of distinct data
      const data = new Uint8Array(BEP52_BLOCK_SIZE * 2);
      const block1 = data.subarray(0, BEP52_BLOCK_SIZE);
      const block2 = data.subarray(BEP52_BLOCK_SIZE);
      block1.fill(1);
      block2.fill(2);

      const result = await builder.buildFromData(data);

      // Piece layer should be H(H(block1) || H(block2))
      const h1 = await sha256(block1);
      const h2 = await sha256(block2);
      const expectedPieceHash = await merkleHashPair(h1, h2);

      expect(result.pieceLayers!.length).toBe(1);
      expect(toHex(result.pieceLayers![0])).toBe(toHex(expectedPieceHash));
    });
  });

  describe('buildFromHashes', () => {
    it('should build tree from pre-computed hashes', async () => {
      const builder = new Bep52TreeBuilder();

      const hashes = [
        new Uint8Array(32).fill(1),
        new Uint8Array(32).fill(2),
        new Uint8Array(32).fill(3),
        new Uint8Array(32).fill(4),
      ];

      const result = await builder.buildFromHashes(hashes, BEP52_BLOCK_SIZE * 4);

      expect(result.blockCount).toBe(4);
      expect(result.leafHashes).toEqual(hashes);

      // Verify root is computed correctly
      const h12 = await merkleHashPair(hashes[0], hashes[1]);
      const h34 = await merkleHashPair(hashes[2], hashes[3]);
      const expectedRoot = await merkleHashPair(h12, h34);

      expect(toHex(result.root)).toBe(toHex(expectedRoot));
    });
  });
});

describe('Bep52StreamBuilder', () => {
  let store: MemoryStore;

  beforeEach(() => {
    store = new MemoryStore();
  });

  describe('append and finalize', () => {
    it('should build tree incrementally', async () => {
      const stream = new Bep52StreamBuilder({ store });

      // Append small chunks
      await stream.append(new Uint8Array([1, 2, 3]));
      await stream.append(new Uint8Array([4, 5, 6]));

      const result = await stream.finalize();

      expect(result.size).toBe(6);
      expect(result.blockCount).toBe(1);
    });

    it('should handle data crossing block boundaries', async () => {
      const stream = new Bep52StreamBuilder({ store });

      // Append more than one block
      const chunk = new Uint8Array(BEP52_BLOCK_SIZE + 100);
      chunk.fill(42);
      await stream.append(chunk);

      const result = await stream.finalize();

      expect(result.size).toBe(BEP52_BLOCK_SIZE + 100);
      expect(result.blockCount).toBe(2);
    });

    it('should produce same result as batch builder', async () => {
      const batchBuilder = new Bep52TreeBuilder({ store });
      const streamBuilder = new Bep52StreamBuilder({ store });

      const data = new Uint8Array(BEP52_BLOCK_SIZE * 2 + 500);
      for (let i = 0; i < data.length; i++) {
        data[i] = i % 256;
      }

      // Build with batch
      const batchResult = await batchBuilder.buildFromData(data);

      // Build with stream in chunks
      const chunkSize = 1000;
      for (let i = 0; i < data.length; i += chunkSize) {
        await streamBuilder.append(data.subarray(i, Math.min(i + chunkSize, data.length)));
      }
      const streamResult = await streamBuilder.finalize();

      expect(toHex(streamResult.root)).toBe(toHex(batchResult.root));
      expect(streamResult.size).toBe(batchResult.size);
      expect(streamResult.blockCount).toBe(batchResult.blockCount);
    });

    it('should handle empty stream', async () => {
      const stream = new Bep52StreamBuilder({ store });
      const result = await stream.finalize();

      expect(result.size).toBe(0);
      expect(result.blockCount).toBe(0);
      expect(toHex(result.root)).toBe(toHex(ZERO_HASH));
    });
  });

  describe('stats', () => {
    it('should track progress', async () => {
      const stream = new Bep52StreamBuilder({ store });

      expect(stream.stats.blocks).toBe(0);
      expect(stream.stats.buffered).toBe(0);
      expect(stream.stats.totalSize).toBe(0);

      await stream.append(new Uint8Array(100));
      expect(stream.stats.blocks).toBe(0);
      expect(stream.stats.buffered).toBe(100);
      expect(stream.stats.totalSize).toBe(100);

      // Fill the block
      await stream.append(new Uint8Array(BEP52_BLOCK_SIZE));
      expect(stream.stats.blocks).toBe(1);
      expect(stream.stats.buffered).toBe(100); // Leftover
      expect(stream.stats.totalSize).toBe(BEP52_BLOCK_SIZE + 100);
    });
  });
});

describe('BEP52 compatibility with libtorrent', () => {
  // These tests verify behavior matches libtorrent's implementation

  it('should use zero-padding for incomplete trees', async () => {
    const builder = new Bep52TreeBuilder();

    // 3 blocks -> needs 4 leaf tree with 1 zero-padded
    const data = new Uint8Array(BEP52_BLOCK_SIZE * 3);
    for (let i = 0; i < data.length; i++) {
      data[i] = i % 256;
    }

    const result = await builder.buildFromData(data);

    // Manually compute expected root with zero padding
    const block1 = await sha256(data.subarray(0, BEP52_BLOCK_SIZE));
    const block2 = await sha256(data.subarray(BEP52_BLOCK_SIZE, BEP52_BLOCK_SIZE * 2));
    const block3 = await sha256(data.subarray(BEP52_BLOCK_SIZE * 2, BEP52_BLOCK_SIZE * 3));

    const h12 = await merkleHashPair(block1, block2);
    const h3pad = await merkleHashPair(block3, ZERO_HASH);
    const expectedRoot = await merkleHashPair(h12, h3pad);

    expect(toHex(result.root)).toBe(toHex(expectedRoot));
  });

  it('should produce correct structure for power-of-2 blocks', async () => {
    const builder = new Bep52TreeBuilder();

    // Exactly 8 blocks
    const data = new Uint8Array(BEP52_BLOCK_SIZE * 8);
    for (let i = 0; i < data.length; i++) {
      data[i] = i % 256;
    }

    const result = await builder.buildFromData(data);

    expect(result.blockCount).toBe(8);
    expect(result.leafHashes.length).toBe(8);

    // Build tree and verify root
    const tree = await merkleBuildTree(result.leafHashes);
    expect(toHex(tree[0])).toBe(toHex(result.root));
  });
});
