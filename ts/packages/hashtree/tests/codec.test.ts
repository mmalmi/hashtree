import { describe, it, expect } from 'vitest';
import { encodeTreeNode, decodeTreeNode, encodeAndHash, tryDecodeTreeNode } from '../src/codec.js';
import { LinkType, TreeNode, toHex } from '../src/types.js';
import { sha256 } from '../src/hash.js';

describe('codec', () => {
  describe('encodeTreeNode / decodeTreeNode', () => {
    it('should encode and decode empty tree', () => {
      const node: TreeNode = {
        type: LinkType.File,
        links: [],
      };

      const encoded = encodeTreeNode(node);
      const decoded = decodeTreeNode(encoded);

      expect(decoded.type).toBe(LinkType.File);
      expect(decoded.links).toEqual([]);
    });

    it('should encode and decode tree with links', () => {
      const hash1 = new Uint8Array(32).fill(1);
      const hash2 = new Uint8Array(32).fill(2);

      const node: TreeNode = {
        type: LinkType.Dir,
        links: [
          { hash: hash1, name: 'file1.txt', size: 100, type: LinkType.Blob },
          { hash: hash2, name: 'dir', size: 500, type: LinkType.Dir },
        ],
      };

      const encoded = encodeTreeNode(node);
      const decoded = decodeTreeNode(encoded);

      expect(decoded.links.length).toBe(2);
      expect(decoded.links[0].name).toBe('file1.txt');
      expect(decoded.links[0].size).toBe(100);
      expect(toHex(decoded.links[0].hash)).toBe(toHex(hash1));
      expect(decoded.links[1].name).toBe('dir');
    });

    it('should preserve link meta', () => {
      const hash = new Uint8Array(32).fill(1);
      const node: TreeNode = {
        type: LinkType.Dir,
        links: [{ hash, name: 'file.txt', size: 100, type: LinkType.Blob, meta: { version: 1, author: 'test' } }],
      };

      const encoded = encodeTreeNode(node);
      const decoded = decodeTreeNode(encoded);

      expect(decoded.links[0].meta).toEqual({ version: 1, author: 'test' });
    });

    it('should handle links without optional fields', () => {
      const hash = new Uint8Array(32).fill(42);

      const node: TreeNode = {
        type: LinkType.File,
        links: [{ hash, size: 0, type: LinkType.Blob }],
      };

      const encoded = encodeTreeNode(node);
      const decoded = decodeTreeNode(encoded);

      expect(decoded.links[0].name).toBeUndefined();
      expect(decoded.links[0].size).toBe(0);
      expect(decoded.links[0].meta).toBeUndefined();
      expect(toHex(decoded.links[0].hash)).toBe(toHex(hash));
    });
  });

  describe('encodeAndHash', () => {
    it('should compute hash of encoded data', async () => {
      const node: TreeNode = {
        type: LinkType.File,
        links: [],
      };

      const { data, hash } = await encodeAndHash(node);
      const expectedHash = await sha256(data);

      expect(toHex(hash)).toBe(toHex(expectedHash));
    });

    it('should produce consistent hashes', async () => {
      const node: TreeNode = {
        type: LinkType.Dir,
        links: [{ hash: new Uint8Array(32).fill(1), name: 'test', size: 100, type: LinkType.Blob }],
      };

      const result1 = await encodeAndHash(node);
      const result2 = await encodeAndHash(node);

      expect(toHex(result1.hash)).toBe(toHex(result2.hash));
    });
  });

  describe('tryDecodeTreeNode', () => {
    it('should decode tree nodes', () => {
      const node: TreeNode = {
        type: LinkType.File,
        links: [],
      };

      const encoded = encodeTreeNode(node);
      expect(tryDecodeTreeNode(encoded)).not.toBeNull();
      expect(tryDecodeTreeNode(encoded)?.type).toBe(LinkType.File);
    });

    it('should return null for raw blobs', () => {
      const blob = new Uint8Array([1, 2, 3, 4, 5]);
      expect(tryDecodeTreeNode(blob)).toBeNull();
    });

    it('should return null for invalid MessagePack', () => {
      const invalid = new Uint8Array([255, 255, 255]);
      expect(tryDecodeTreeNode(invalid)).toBeNull();
    });

    it('should return null for non-tree MessagePack objects', () => {
      // This would be valid data but not a tree node
      const notTree = new TextEncoder().encode('hello');
      expect(tryDecodeTreeNode(notTree)).toBeNull();
    });
  });

  describe('determinism', () => {
    it('should produce identical bytes for identical nodes', () => {
      const hash = new Uint8Array(32).fill(42);

      const node: TreeNode = {
        type: LinkType.Dir,
        links: [{ hash, name: 'file.txt', size: 100, type: LinkType.Blob }],
      };

      const encoded1 = encodeTreeNode(node);
      const encoded2 = encodeTreeNode(node);
      const encoded3 = encodeTreeNode(node);

      expect(toHex(encoded1)).toBe(toHex(encoded2));
      expect(toHex(encoded2)).toBe(toHex(encoded3));
    });

    it('should produce identical bytes regardless of link meta key insertion order', async () => {
      const hash = new Uint8Array(32).fill(1);

      // Create meta with keys in different orders
      // Note: JavaScript object key order is preserved since ES2015,
      // but we still sort them explicitly for cross-platform determinism
      const meta1 = { zebra: 'last', alpha: 'first', middle: 'mid' };
      const meta2 = { alpha: 'first', middle: 'mid', zebra: 'last' };

      const node1: TreeNode = {
        type: LinkType.Dir,
        links: [{ hash, name: 'file', size: 100, type: LinkType.Blob, meta: meta1 }],
      };

      const node2: TreeNode = {
        type: LinkType.Dir,
        links: [{ hash, name: 'file', size: 100, type: LinkType.Blob, meta: meta2 }],
      };

      const encoded1 = encodeTreeNode(node1);
      const encoded2 = encodeTreeNode(node2);

      // Both should produce identical bytes (keys sorted alphabetically)
      expect(toHex(encoded1)).toBe(toHex(encoded2));

      // And identical hashes
      const hash1 = await sha256(encoded1);
      const hash2 = await sha256(encoded2);
      expect(toHex(hash1)).toBe(toHex(hash2));
    });
  });
});
