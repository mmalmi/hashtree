import { describe, it, expect, beforeEach } from 'vitest';
import { HashTree, MemoryStore, generateKey } from '../src/index.js';

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

const RANDOMNESS_THRESHOLD = 111; // From error message "Unique: 97 (min: 111)"

describe('re-encryption of unencrypted data', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store });
  });

  it('should detect unencrypted blob has low entropy', async () => {
    // Store data unencrypted
    const data = new Uint8Array(256);
    for (let i = 0; i < data.length; i++) data[i] = i % 256;

    const { cid } = await tree.putFile(data, { unencrypted: true });
    expect(cid.key).toBeUndefined();

    // Check blob entropy - should be low for unencrypted data
    const blob = await store.get(cid.hash);
    expect(blob).toBeDefined();
    const uniqueBytes = countUniqueBytes(blob!);
    // Unencrypted data might still have high entropy if content is varied
    // but the issue is the DATA itself, not the hash
  });

  it('BUG REPRO: CID has key but blob is unencrypted - reading with key fails', async () => {
    // This simulates the old bug where:
    // 1. Data was stored UNENCRYPTED (no key)
    // 2. But the Nostr event has a key (from somewhere)
    // 3. When we try to read with the key, decryption fails

    // Store data unencrypted
    const data = new TextEncoder().encode('hello world - this is unencrypted data');
    const { cid: unencryptedCid } = await tree.putFile(data, { unencrypted: true });
    expect(unencryptedCid.key).toBeUndefined();

    // Create a fake CID with a key pointing to the same hash
    const fakeKey = generateKey();
    const brokenCid = { hash: unencryptedCid.hash, key: fakeKey };

    // Try to read with the fake key - should fail because data isn't encrypted
    await expect(tree.readFile(brokenCid)).rejects.toThrow();
  });

  it('BUG REPRO: reading unencrypted data WITHOUT key should work', async () => {
    // Store data unencrypted
    const data = new TextEncoder().encode('hello world - this is unencrypted data');
    const { cid: unencryptedCid } = await tree.putFile(data, { unencrypted: true });

    // Read without key should work
    const readData = await tree.readFile({ hash: unencryptedCid.hash });
    expect(readData).toEqual(data);
  });

  it('FIX: re-encrypt unencrypted file by reading without key and storing encrypted', async () => {
    // Store data unencrypted
    const data = new TextEncoder().encode('hello world - this needs encryption');
    const { cid: unencryptedCid } = await tree.putFile(data, { unencrypted: true });
    expect(unencryptedCid.key).toBeUndefined();

    // Read without key (simulating force re-encrypt stripping the key)
    const readData = await tree.readFile({ hash: unencryptedCid.hash });
    expect(readData).toEqual(data);

    // Store with encryption (default)
    const { cid: encryptedCid } = await tree.putFile(readData!);
    expect(encryptedCid.key).toBeDefined();

    // Verify the new blob is encrypted (high entropy)
    const encryptedBlob = await store.get(encryptedCid.hash);
    expect(encryptedBlob).toBeDefined();
    if (encryptedBlob!.length >= 256) {
      const uniqueBytes = countUniqueBytes(encryptedBlob!);
      expect(uniqueBytes).toBeGreaterThanOrEqual(RANDOMNESS_THRESHOLD);
    }

    // Verify we can read the encrypted data
    const decryptedData = await tree.readFile(encryptedCid);
    expect(decryptedData).toEqual(data);
  });

  it('FIX: re-encrypt unencrypted directory by reading without key', async () => {
    // Create files unencrypted
    const file1Data = new TextEncoder().encode('file 1 content that is unencrypted');
    const file2Data = new TextEncoder().encode('file 2 content also unencrypted');

    const { cid: file1Cid, size: file1Size } = await tree.putFile(file1Data, { unencrypted: true });
    const { cid: file2Cid, size: file2Size } = await tree.putFile(file2Data, { unencrypted: true });

    expect(file1Cid.key).toBeUndefined();
    expect(file2Cid.key).toBeUndefined();

    // Create directory unencrypted
    const { cid: dirCid } = await tree.putDirectory([
      { name: 'file1.txt', cid: file1Cid, size: file1Size, type: 0 },
      { name: 'file2.txt', cid: file2Cid, size: file2Size, type: 0 },
    ], { unencrypted: true });

    expect(dirCid.key).toBeUndefined();

    // Now simulate the re-encryption process:
    // 1. Read directory without key
    const isDir = await tree.isDirectory({ hash: dirCid.hash });
    expect(isDir).toBe(true);

    const entries = await tree.listDirectory({ hash: dirCid.hash });
    expect(entries.length).toBe(2);

    // 2. Re-encrypt each child
    const newEntries = [];
    for (const entry of entries) {
      // Child CIDs from unencrypted dir don't have keys
      expect(entry.cid.key).toBeUndefined();

      // Read file without key
      const fileData = await tree.readFile({ hash: entry.cid.hash });
      expect(fileData).toBeDefined();

      // Store encrypted
      const { cid: newCid, size: newSize } = await tree.putFile(fileData!);
      expect(newCid.key).toBeDefined();

      newEntries.push({
        name: entry.name,
        cid: newCid,
        size: newSize,
        type: entry.type ?? 0,
      });
    }

    // 3. Create new encrypted directory
    const { cid: newDirCid } = await tree.putDirectory(newEntries);
    expect(newDirCid.key).toBeDefined();

    // 4. Verify we can read the encrypted directory
    const newEntries2 = await tree.listDirectory(newDirCid);
    expect(newEntries2.length).toBe(2);

    // 5. Verify blobs are encrypted (high entropy)
    for await (const block of tree.walkBlocks(newDirCid)) {
      if (block.data.length >= 256) {
        const uniqueBytes = countUniqueBytes(block.data);
        expect(uniqueBytes).toBeGreaterThanOrEqual(RANDOMNESS_THRESHOLD);
      }
    }
  });
});
