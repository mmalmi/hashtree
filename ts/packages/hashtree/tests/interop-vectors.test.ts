/**
 * Interoperability test vector generation
 * These tests generate test vectors that can be verified by the Rust implementation
 */

import { describe, it, expect, beforeEach } from 'vitest';
import { TreeBuilder } from '../src/builder.js';
import { MemoryStore } from '../src/store/memory.js';
import { toHex, fromHex } from '../src/types.js';
import { sha256 } from '../src/hash.js';
import { encodeTreeNode, decodeTreeNode } from '../src/codec.js';
import { LinkType, TreeNode } from '../src/types.js';
import { encryptChk, decryptChk } from '../src/crypto.js';
import * as fs from 'fs';
import * as path from 'path';

interface TestVector {
  name: string;
  input: {
    type: 'blob' | 'file' | 'tree_node' | 'directory';
    data?: string; // hex encoded for blobs/files
    node?: {
      links: Array<{
        hash: string;
        name?: string;
        size?: number;
        meta?: Record<string, unknown>;
      }>;
      totalSize?: number;
    };
    entries?: Array<{
      name: string;
      hash: string;
      size?: number;
    }>;
  };
  expected: {
    hash: string;
    msgpack?: string; // hex encoded MessagePack for tree nodes
    ciphertext?: string; // hex encoded ciphertext for CHK tests
    size?: number;
  };
}

describe('Interoperability Test Vectors', () => {
  let store: MemoryStore;
  let builder: TreeBuilder;
  const vectors: TestVector[] = [];

  beforeEach(() => {
    store = new MemoryStore();
    builder = new TreeBuilder({ store });
  });

  it('should generate SHA256 hash vectors', async () => {
    // Empty data
    const emptyHash = await sha256(new Uint8Array(0));
    vectors.push({
      name: 'sha256_empty',
      input: { type: 'blob', data: '' },
      expected: { hash: toHex(emptyHash) }
    });
    expect(toHex(emptyHash)).toBe('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');

    // "hello world"
    const helloHash = await sha256(new TextEncoder().encode('hello world'));
    vectors.push({
      name: 'sha256_hello_world',
      input: { type: 'blob', data: toHex(new TextEncoder().encode('hello world')) },
      expected: { hash: toHex(helloHash) }
    });
    expect(toHex(helloHash)).toBe('b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9');

    // Binary data [1, 2, 3, 4, 5]
    const binaryData = new Uint8Array([1, 2, 3, 4, 5]);
    const binaryHash = await sha256(binaryData);
    vectors.push({
      name: 'sha256_binary',
      input: { type: 'blob', data: toHex(binaryData) },
      expected: { hash: toHex(binaryHash) }
    });
  });

  it('should generate empty tree node vector', async () => {
    const node: TreeNode = {
      type: LinkType.Dir,
      links: [],
    };

    const encoded = encodeTreeNode(node);
    const hash = await sha256(encoded);

    vectors.push({
      name: 'tree_node_empty',
      input: {
        type: 'tree_node',
        node: { links: [] }
      },
      expected: {
        hash: toHex(hash),
        msgpack: toHex(encoded)
      }
    });

    // Verify roundtrip
    const decoded = decodeTreeNode(encoded);
    expect(decoded.links).toEqual([]);
  });

  it('should generate tree node with single link', async () => {
    const linkHash = new Uint8Array(32).fill(0xab);
    const node: TreeNode = {
      type: LinkType.Dir,
      links: [{ hash: linkHash, name: 'test.txt', size: 100, type: LinkType.Blob }],
    };

    const encoded = encodeTreeNode(node);
    const hash = await sha256(encoded);

    vectors.push({
      name: 'tree_node_single_link',
      input: {
        type: 'tree_node',
        node: {
          links: [{
            hash: toHex(linkHash),
            name: 'test.txt',
            size: 100
          }]
        }
      },
      expected: {
        hash: toHex(hash),
        msgpack: toHex(encoded)
      }
    });
  });

  it('should generate tree node with multiple links', async () => {
    const hash1 = new Uint8Array(32).fill(0x01);
    const hash2 = new Uint8Array(32).fill(0x02);
    const hash3 = new Uint8Array(32).fill(0x03);

    const node: TreeNode = {
      type: LinkType.Dir,
      links: [
        { hash: hash1, name: 'a.txt', size: 10, type: LinkType.Blob },
        { hash: hash2, name: 'b.txt', size: 20, type: LinkType.Blob },
        { hash: hash3, name: 'c.txt', size: 30, type: LinkType.Blob },
      ],
      totalSize: 60,
    };

    const encoded = encodeTreeNode(node);
    const hash = await sha256(encoded);

    vectors.push({
      name: 'tree_node_multiple_links',
      input: {
        type: 'tree_node',
        node: {
          links: [
            { hash: toHex(hash1), name: 'a.txt', size: 10 },
            { hash: toHex(hash2), name: 'b.txt', size: 20 },
            { hash: toHex(hash3), name: 'c.txt', size: 30 },
          ],
          totalSize: 60
        }
      },
      expected: {
        hash: toHex(hash),
        msgpack: toHex(encoded)
      }
    });
  });

  it('should generate tree node with link meta', async () => {
    const linkHash = new Uint8Array(32).fill(0xcd);
    const node: TreeNode = {
      type: LinkType.Dir,
      links: [{ hash: linkHash, size: 0, type: LinkType.Blob, meta: { author: 'test', version: 1 } }],
    };

    const encoded = encodeTreeNode(node);
    const hash = await sha256(encoded);

    vectors.push({
      name: 'tree_node_with_link_meta',
      input: {
        type: 'tree_node',
        node: {
          links: [{ hash: toHex(linkHash), size: 0, meta: { author: 'test', version: 1 } }]
        }
      },
      expected: {
        hash: toHex(hash),
        msgpack: toHex(encoded)
      }
    });
  });

  it('should generate tree node with unnamed links (chunked file)', async () => {
    const hash1 = new Uint8Array(32).fill(0xaa);
    const hash2 = new Uint8Array(32).fill(0xbb);

    const node: TreeNode = {
      type: LinkType.File,
      links: [
        { hash: hash1, size: 100, type: LinkType.Blob },
        { hash: hash2, size: 50, type: LinkType.Blob },
      ],
      totalSize: 150,
    };

    const encoded = encodeTreeNode(node);
    const hash = await sha256(encoded);

    vectors.push({
      name: 'tree_node_unnamed_links',
      input: {
        type: 'tree_node',
        node: {
          links: [
            { hash: toHex(hash1), size: 100 },
            { hash: toHex(hash2), size: 50 },
          ],
          totalSize: 150
        }
      },
      expected: {
        hash: toHex(hash),
        msgpack: toHex(encoded)
      }
    });
  });

  it('should generate small file vector', async () => {
    const data = new TextEncoder().encode('Hello, HashTree!');
    const hash = await builder.putBlob(data);

    vectors.push({
      name: 'small_file',
      input: {
        type: 'file',
        data: toHex(data)
      },
      expected: {
        hash: toHex(hash),
        size: data.length
      }
    });
  });

  it('should generate chunked file vector', async () => {
    // Use small chunk size to force chunking
    const smallBuilder = new TreeBuilder({ store, chunkSize: 10 });

    const data = new TextEncoder().encode('This is a longer message that will be chunked.');
    const { hash, size } = await smallBuilder.putFile(data);

    vectors.push({
      name: 'chunked_file',
      input: {
        type: 'file',
        data: toHex(data)
      },
      expected: {
        hash: toHex(hash),
        size
      }
    });
  });

  it('should generate CHK encryption vectors', async () => {
    // Test cases for CHK encryption interoperability
    const testCases = [
      { name: 'chk_empty', data: new Uint8Array(0) },
      { name: 'chk_hello', data: new TextEncoder().encode('hello') },
      { name: 'chk_binary', data: new Uint8Array([0x01, 0x02, 0x03, 0x04, 0x05]) },
      { name: 'chk_longer', data: new TextEncoder().encode('This is a longer message for testing CHK encryption interoperability.') },
    ];

    for (const tc of testCases) {
      const { ciphertext, key } = await encryptChk(tc.data);

      vectors.push({
        name: tc.name,
        input: {
          type: 'blob', // Using blob type in vectors interface
          data: toHex(tc.data)
        },
        expected: {
          hash: toHex(key), // Key derived from SHA256 of plaintext
          ciphertext: toHex(ciphertext) // AES-GCM encrypted ciphertext
        }
      });

      // Verify decryption works
      const decrypted = await decryptChk(ciphertext, key);
      expect(decrypted).toEqual(tc.data);
    }
  });

  // Write vectors to file after all tests
  afterAll(() => {
    const outputPath = path.join(__dirname, '../test-data/interop-vectors.json');
    fs.mkdirSync(path.dirname(outputPath), { recursive: true });
    fs.writeFileSync(outputPath, JSON.stringify(vectors, null, 2));
    console.log(`Written ${vectors.length} test vectors to ${outputPath}`);
  });
});
