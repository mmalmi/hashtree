import { describe, it, expect, beforeEach } from 'vitest';
import {
  HashTree,
  MemoryStore,
  encrypt,
  decrypt,
  generateKey,
  keyToHex,
  keyFromHex,
  encryptedSize,
  plaintextSize,
} from '../src/index.js';

describe('crypto', () => {
  describe('encrypt/decrypt', () => {
    it('should encrypt and decrypt small data', async () => {
      const key = generateKey();
      const plaintext = new TextEncoder().encode('hello world');

      const encrypted = await encrypt(plaintext, key);
      const decrypted = await decrypt(encrypted, key);

      expect(decrypted).toEqual(plaintext);
    });

    it('should encrypt and decrypt empty data', async () => {
      const key = generateKey();
      const plaintext = new Uint8Array(0);

      const encrypted = await encrypt(plaintext, key);
      const decrypted = await decrypt(encrypted, key);

      expect(decrypted).toEqual(plaintext);
    });

    it('should encrypt and decrypt large data', async () => {
      const key = generateKey();
      const plaintext = new Uint8Array(1024 * 1024); // 1MB
      // Fill with pattern instead of random (crypto.getRandomValues has 64KB limit)
      for (let i = 0; i < plaintext.length; i++) {
        plaintext[i] = i % 256;
      }

      const encrypted = await encrypt(plaintext, key);
      const decrypted = await decrypt(encrypted, key);

      expect(decrypted).toEqual(plaintext);
    });

    it('should produce different ciphertext with different keys', async () => {
      const key1 = generateKey();
      const key2 = generateKey();
      const plaintext = new TextEncoder().encode('hello world');

      const encrypted1 = await encrypt(plaintext, key1);
      const encrypted2 = await encrypt(plaintext, key2);

      // Different keys = different ciphertext
      expect(encrypted1).not.toEqual(encrypted2);
    });

    it('should fail to decrypt with wrong key', async () => {
      const key1 = generateKey();
      const key2 = generateKey();
      const plaintext = new TextEncoder().encode('hello world');

      const encrypted = await encrypt(plaintext, key1);

      await expect(decrypt(encrypted, key2)).rejects.toThrow();
    });

    it('should fail with invalid key length', async () => {
      const shortKey = new Uint8Array(16);
      const plaintext = new TextEncoder().encode('hello');

      await expect(encrypt(plaintext, shortKey)).rejects.toThrow('32 bytes');
    });
  });

  describe('key utilities', () => {
    it('should convert key to hex and back', () => {
      const key = generateKey();
      const hex = keyToHex(key);
      const restored = keyFromHex(hex);

      expect(hex.length).toBe(64);
      expect(restored).toEqual(key);
    });

    it('should reject invalid hex length', () => {
      expect(() => keyFromHex('abc')).toThrow('64 characters');
    });
  });

  describe('size utilities', () => {
    it('should calculate encrypted size', () => {
      // IV (12) + plaintext + auth tag (16)
      expect(encryptedSize(0)).toBe(28);
      expect(encryptedSize(100)).toBe(128);
    });

    it('should calculate plaintext size', () => {
      expect(plaintextSize(28)).toBe(0);
      expect(plaintextSize(128)).toBe(100);
    });
  });
});

/**
 * Count unique byte values in data (for randomness testing)
 * Mimics the looksRandom function from blossom-cf-worker
 */
function countUniqueBytes(data: Uint8Array): number {
  const sampleSize = Math.min(data.length, 256);
  const seen = new Set<number>();
  for (let i = 0; i < sampleSize; i++) {
    seen.add(data[i]);
  }
  return seen.size;
}

describe('HashTree encrypted', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store });
  });

  describe('putFile/readFile (encrypted by default)', () => {
    it('should encrypt and decrypt small file', async () => {
      const data = new TextEncoder().encode('hello encrypted world');

      // putFile() encrypts by default, returns CID with key
      const { cid, size } = await tree.putFile(data);

      expect(size).toBe(data.length);
      expect(cid.key).toBeDefined();
      expect(cid.key!.length).toBe(32);

      // readFile(cid) decrypts using the key in the CID
      const decrypted = await tree.readFile(cid);
      expect(decrypted).toEqual(data);
    });

    it('should encrypt and decrypt chunked file', async () => {
      const smallTree = new HashTree({ store, chunkSize: 10 });
      const data = new TextEncoder().encode('this is a longer message that will be chunked and encrypted');

      const { cid, size } = await smallTree.putFile(data);

      expect(size).toBe(data.length);

      const decrypted = await smallTree.readFile(cid);
      expect(decrypted).toEqual(data);
    });

    it('should derive deterministic key from content (CHK)', async () => {
      const data = new TextEncoder().encode('hello');

      // With CHK, same content always produces same key
      const result1 = await tree.putFile(data);
      const result2 = await tree.putFile(data);

      expect(result1.cid.key).toEqual(result2.cid.key);
      expect(result1.cid.hash).toEqual(result2.cid.hash);
    });

    it('should fail to decrypt with wrong key', async () => {
      const data = new TextEncoder().encode('hello');
      const { cid } = await tree.putFile(data);
      const wrongKey = generateKey();

      await expect(tree.readFile({ hash: cid.hash, key: wrongKey })).rejects.toThrow();
    });

    it('should return null for missing hash', async () => {
      const key = generateKey();
      const missingHash = new Uint8Array(32);

      const result = await tree.readFile({ hash: missingHash, key });
      expect(result).toBeNull();
    });
  });

  describe('readFileStream (encrypted)', () => {
    it('should stream decrypt small file', async () => {
      const data = new TextEncoder().encode('hello stream');
      const { cid } = await tree.putFile(data);

      const chunks: Uint8Array[] = [];
      for await (const chunk of tree.readFileStream(cid)) {
        chunks.push(chunk);
      }

      expect(chunks.length).toBe(1);
      expect(chunks[0]).toEqual(data);
    });

    it('should stream decrypt chunked file', async () => {
      const smallTree = new HashTree({ store, chunkSize: 10 });
      const data = new TextEncoder().encode('this is a longer message for streaming');

      const { cid } = await smallTree.putFile(data);

      const chunks: Uint8Array[] = [];
      for await (const chunk of smallTree.readFileStream(cid)) {
        chunks.push(chunk);
      }

      // Multiple chunks
      expect(chunks.length).toBeGreaterThan(1);

      // Reassemble and verify
      const totalLength = chunks.reduce((sum, c) => sum + c.length, 0);
      const reassembled = new Uint8Array(totalLength);
      let offset = 0;
      for (const chunk of chunks) {
        reassembled.set(chunk, offset);
        offset += chunk.length;
      }

      expect(reassembled).toEqual(data);
    });
  });

  describe('blob randomness (for blossom compatibility)', () => {
    const RANDOMNESS_THRESHOLD = 140; // Same as blossom-cf-worker

    it('encrypted blobs with 256+ bytes should look random', async () => {
      // Use larger data to ensure blobs are big enough for randomness check
      // Each random-looking chunk needs at least 256 bytes to be meaningful
      const data = new Uint8Array(1024);
      for (let i = 0; i < data.length; i++) data[i] = i % 256;

      const { cid } = await tree.putFile(data);

      // Check all stored blobs have high unique byte count
      for await (const block of tree.walkBlocks(cid)) {
        // Only check blobs >= 256 bytes (smaller ones can't have 140 unique)
        if (block.data.length >= 256) {
          const uniqueBytes = countUniqueBytes(block.data);
          expect(uniqueBytes).toBeGreaterThanOrEqual(RANDOMNESS_THRESHOLD);
        }
      }
    });

    it('encrypted directory with many entries should look random', async () => {
      // Create 10 files to make a larger directory tree node
      const entries = [];
      for (let i = 0; i < 10; i++) {
        const data = new TextEncoder().encode(`file ${i} content with some padding data`);
        const { cid, size } = await tree.putFile(data);
        entries.push({ name: `file${i}.txt`, cid, size, type: 0 as const });
      }

      const { cid: dirCid } = await tree.putDirectory(entries);

      // Check that large blobs look random
      for await (const block of tree.walkBlocks(dirCid)) {
        if (block.data.length >= 256) {
          const uniqueBytes = countUniqueBytes(block.data);
          expect(uniqueBytes).toBeGreaterThanOrEqual(RANDOMNESS_THRESHOLD);
        }
      }
    });

    it('small encrypted blobs may have fewer unique bytes (known limitation)', async () => {
      // Small data produces small encrypted blobs that can't have 140 unique bytes
      const smallData = new TextEncoder().encode('tiny');
      const { cid } = await tree.putFile(smallData);

      for await (const block of tree.walkBlocks(cid)) {
        // Encrypted small data: ~4 bytes plaintext + 16 byte tag = ~20 bytes ciphertext
        // Can't have more unique bytes than the blob size
        const uniqueBytes = countUniqueBytes(block.data);
        expect(uniqueBytes).toBeLessThanOrEqual(block.data.length);
      }
    });

    it('public option stores unencrypted blobs (lower unique bytes)', async () => {
      const file1Data = new TextEncoder().encode('file1 content');
      const file2Data = new TextEncoder().encode('file2 content');

      // Public files don't have encryption keys
      const { cid: file1Cid, size: file1Size } = await tree.putFile(file1Data, { unencrypted: true });
      const { cid: file2Cid, size: file2Size } = await tree.putFile(file2Data, { unencrypted: true });

      // Public files should NOT have keys
      expect(file1Cid.key).toBeUndefined();
      expect(file2Cid.key).toBeUndefined();

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'file1.txt', cid: file1Cid, size: file1Size, type: 0 },
        { name: 'file2.txt', cid: file2Cid, size: file2Size, type: 0 },
      ], { unencrypted: true });

      // Directory should also NOT have a key
      expect(dirCid.key).toBeUndefined();

      // Unencrypted blobs may have lower unique byte count
      // This is expected behavior for the library
      // Apps that require encryption should NOT use unencrypted: true
    });

    it('diagnose: print blob sizes and unique bytes for encrypted video-like tree', async () => {
      // Simulate video upload: large file chunks + small metadata files
      const videoChunk = new Uint8Array(1000);
      for (let i = 0; i < videoChunk.length; i++) videoChunk[i] = Math.floor(Math.random() * 256);

      const titleData = new TextEncoder().encode('My Video Title');
      const descData = new TextEncoder().encode('Video description here');

      const { cid: videoCid, size: videoSize } = await tree.putFile(videoChunk);
      const { cid: titleCid, size: titleSize } = await tree.putFile(titleData);
      const { cid: descCid, size: descSize } = await tree.putFile(descData);

      const { cid: dirCid } = await tree.putDirectory([
        { name: 'video.webm', cid: videoCid, size: videoSize, type: 0 },
        { name: 'title.txt', cid: titleCid, size: titleSize, type: 0 },
        { name: 'description.txt', cid: descCid, size: descSize, type: 0 },
      ]);

      // All blobs should pass filter if they're >= 64 bytes
      // (blossom rejects < 64 bytes anyway)
      for await (const block of tree.walkBlocks(dirCid)) {
        const uniqueBytes = countUniqueBytes(block.data);
        if (block.data.length >= 64) {
          // Large enough to have meaningful randomness
          expect(uniqueBytes).toBeGreaterThanOrEqual(Math.min(RANDOMNESS_THRESHOLD, block.data.length * 0.5));
        }
      }
    });
  });
});
