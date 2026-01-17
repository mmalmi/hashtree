/**
 * Additional tests for streaming functionality
 * Inspired by scionic-merkle-tree-ts streaming tests
 */

import { describe, it, expect, beforeEach } from 'vitest';
import { HashTree } from '../src/hashtree.js';
import { MemoryStore } from '../src/store/memory.js';
import { toHex, cid, type CID } from '../src/types.js';

describe('StreamWriter - Streaming scenarios', () => {
  let store: MemoryStore;
  let tree: HashTree;

  beforeEach(() => {
    store = new MemoryStore();
    tree = new HashTree({ store });
  });

  describe('incremental root updates', () => {
    it('should provide updated root CID after each chunk', async () => {
      const stream = new HashTree({ store, chunkSize: 100 }).createStream();

      await stream.append(new Uint8Array([1, 2, 3]));
      const root1 = await stream.currentRoot();

      await stream.append(new Uint8Array([4, 5, 6]));
      const root2 = await stream.currentRoot();

      await stream.append(new Uint8Array([7, 8, 9]));
      const root3 = await stream.currentRoot();

      // Each addition should produce different root (CID now includes key)
      expect(toHex(root1!.hash)).not.toBe(toHex(root2!.hash));
      expect(toHex(root2!.hash)).not.toBe(toHex(root3!.hash));

      // All intermediate roots should be readable with their keys
      const data1 = await tree.readFile(root1!);
      expect(data1).toEqual(new Uint8Array([1, 2, 3]));

      const data2 = await tree.readFile(root2!);
      expect(data2).toEqual(new Uint8Array([1, 2, 3, 4, 5, 6]));

      const data3 = await tree.readFile(root3!);
      expect(data3).toEqual(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9]));
    });

    it('should allow reading partial stream at any point', async () => {
      const stream = new HashTree({ store, chunkSize: 100 }).createStream();
      const checkpoints: CID[] = [];

      for (let i = 0; i < 5; i++) {
        const chunk = new Uint8Array(20).fill(i);
        await stream.append(chunk);
        const root = await stream.currentRoot();
        checkpoints.push(root!);
      }

      // Each checkpoint should be independently readable with its key
      for (let i = 0; i < checkpoints.length; i++) {
        const data = await tree.readFile(checkpoints[i]);
        expect(data!.length).toBe((i + 1) * 20);
        expect(data![i * 20]).toBe(i);
      }
    });
  });

  describe('livestream simulation', () => {
    it('should simulate video stream chunking', async () => {
      // Simulate 1-second video chunks (~100KB each)
      const chunkSize = 64 * 1024; // 64KB internal chunks
      const streamTree = new HashTree({ store, chunkSize });
      const stream = streamTree.createStream();

      const videoChunks = [];
      const publishedRoots: CID[] = [];

      // Simulate 5 seconds of video
      for (let second = 0; second < 5; second++) {
        // Each "second" of video is ~100KB
        const videoData = new Uint8Array(100 * 1024);
        for (let i = 0; i < videoData.length; i++) {
          videoData[i] = (second * 100 + i) % 256;
        }
        videoChunks.push(videoData);

        await stream.append(videoData);
        const root = await stream.currentRoot();
        publishedRoots.push(root!);
      }

      // Final root - finalize returns { hash, size, key }
      const { hash, size, key } = await stream.finalize();
      expect(size).toBe(5 * 100 * 1024);

      // Viewer joining at second 3 should be able to read data with key
      const partialData = await tree.readFile(publishedRoots[2]);
      expect(partialData!.length).toBe(3 * 100 * 1024);

      // Full stream should contain all data (use CID with key)
      const fullData = await tree.readFile(cid(hash, key));
      expect(fullData!.length).toBe(5 * 100 * 1024);
    });

    it('should handle rapid sequential chunk additions', async () => {
      const stream = new HashTree({ store, chunkSize: 1024 }).createStream();

      // Simulate rapid data arrival (sequential - appends must be serialized)
      for (let i = 0; i < 50; i++) {
        const chunk = new Uint8Array(100).fill(i);
        await stream.append(chunk);
      }

      const { hash, size, key } = await stream.finalize();
      expect(size).toBe(5000);

      const data = await tree.readFile(cid(hash, key));
      expect(data!.length).toBe(5000);
    });
  });

  describe('concurrent readers', () => {
    it('should support multiple readers at different positions', async () => {
      const stream = new HashTree({ store, chunkSize: 100 }).createStream();

      // Build stream
      for (let i = 0; i < 10; i++) {
        await stream.append(new Uint8Array(50).fill(i));
      }
      const { hash, key } = await stream.finalize();
      const fileCid = cid(hash, key);

      // Multiple readers can read independently
      const reader1 = new HashTree({ store });
      const reader2 = new HashTree({ store });

      const [data1, data2] = await Promise.all([
        reader1.readFile(fileCid),
        reader2.readFile(fileCid),
      ]);

      expect(data1).toEqual(data2);
      expect(data1!.length).toBe(500);
    });
  });

  describe('edge cases', () => {
    it('should handle single byte appends', async () => {
      const stream = new HashTree({ store, chunkSize: 10 }).createStream();

      for (let i = 0; i < 25; i++) {
        await stream.append(new Uint8Array([i]));
      }

      const { hash, size, key } = await stream.finalize();
      expect(size).toBe(25);

      const data = await tree.readFile(cid(hash, key));
      expect(data!.length).toBe(25);
      for (let i = 0; i < 25; i++) {
        expect(data![i]).toBe(i);
      }
    });

    it('should handle chunk-aligned appends', async () => {
      const chunkSize = 100;
      const stream = new HashTree({ store, chunkSize }).createStream();

      // Append exactly chunk-sized data 5 times
      for (let i = 0; i < 5; i++) {
        await stream.append(new Uint8Array(chunkSize).fill(i));
      }

      expect(stream.stats.chunks).toBe(5);
      expect(stream.stats.buffered).toBe(0);
      expect(stream.stats.totalSize).toBe(500);

      const { hash, key } = await stream.finalize();
      const data = await tree.readFile(cid(hash, key));
      expect(data!.length).toBe(500);
    });

    it('should handle very large single append', async () => {
      const chunkSize = 100;
      const stream = new HashTree({ store, chunkSize }).createStream();

      // Single large append (10 chunks worth)
      const bigData = new Uint8Array(chunkSize * 10);
      for (let i = 0; i < bigData.length; i++) {
        bigData[i] = i % 256;
      }

      await stream.append(bigData);

      expect(stream.stats.chunks).toBe(10);
      expect(stream.stats.totalSize).toBe(1000);

      const { hash, key } = await stream.finalize();
      const data = await tree.readFile(cid(hash, key));
      expect(data).toEqual(bigData);
    });

    it('should handle mixed small and large appends', async () => {
      const chunkSize = 100;
      const stream = new HashTree({ store, chunkSize }).createStream();

      await stream.append(new Uint8Array([1, 2, 3])); // 3 bytes
      await stream.append(new Uint8Array(250).fill(4)); // 250 bytes (crosses chunks)
      await stream.append(new Uint8Array([5])); // 1 byte
      await stream.append(new Uint8Array(46).fill(6)); // 46 bytes

      const { hash, size, key } = await stream.finalize();
      expect(size).toBe(300);

      const data = await tree.readFile(cid(hash, key));
      expect(data![0]).toBe(1);
      expect(data![3]).toBe(4);
      expect(data![253]).toBe(5);
      expect(data![254]).toBe(6);
    });
  });

  describe('live mode simulation (rolling window)', () => {
    it('should allow rebuilding from subset of chunks', async () => {
      const chunkSize = 100;
      const maxChunks = 3; // Keep only last 3 "seconds"

      // Simulate chunks arriving
      const allChunks: Uint8Array[] = [];
      for (let i = 0; i < 10; i++) {
        const chunk = new Uint8Array(chunkSize);
        chunk.fill(i);
        allChunks.push(chunk);
      }

      // Build "live" stream with only last N chunks
      const liveChunks = allChunks.slice(-maxChunks);
      const stream = new HashTree({ store, chunkSize }).createStream();

      for (const chunk of liveChunks) {
        await stream.append(chunk);
      }

      const { hash, size, key } = await stream.finalize();
      expect(size).toBe(maxChunks * chunkSize);

      const data = await tree.readFile(cid(hash, key));
      // Should contain chunks 7, 8, 9
      expect(data![0]).toBe(7);
      expect(data![100]).toBe(8);
      expect(data![200]).toBe(9);
    });
  });

  describe('deduplication', () => {
    it('should deduplicate identical chunks', async () => {
      const chunkSize = 100;
      const stream = new HashTree({ store, chunkSize }).createStream();

      const repeatedData = new Uint8Array(chunkSize).fill(42);

      // Append same data 5 times
      for (let i = 0; i < 5; i++) {
        await stream.append(repeatedData);
      }

      const { size } = await stream.finalize();
      expect(size).toBe(500);

      // Store should have fewer items due to dedup
      // (1 chunk blob + potentially some tree nodes)
      const storeSize = store.size;
      // With 5 identical chunks, we only store 1 unique chunk
      // plus tree structure (much less than 5 separate chunks)
      expect(storeSize).toBeLessThan(5);
    });
  });

  describe('live viewer - incremental reading', () => {
    it('should read only new data when CID updates', async () => {
      // This tests the core live viewing pattern:
      // 1. Viewer joins stream, reads initial data
      // 2. Stream adds more data, publishes new CID
      // 3. Viewer reads from new CID starting at previous end offset
      // 4. Viewer only receives the NEW bytes, not the whole file

      const chunkSize = 100;
      const streamTree = new HashTree({ store, chunkSize });
      const stream = streamTree.createStream();

      // Initial data
      const chunk1 = new Uint8Array(50).fill(1);
      await stream.append(chunk1);
      const cid1 = await stream.currentRoot();

      // Viewer joins and reads all available data
      const initialData = await tree.readFile(cid1!);
      expect(initialData).toEqual(chunk1);
      const viewerOffset = initialData!.length; // 50 bytes

      // More data arrives
      const chunk2 = new Uint8Array(50).fill(2);
      await stream.append(chunk2);
      const cid2 = await stream.currentRoot();

      // Viewer reads ONLY new data using readFileRange starting from offset
      const newData = await tree.readFileRange(cid2!, viewerOffset);
      expect(newData!.length).toBe(50);
      expect(newData![0]).toBe(2); // This is chunk2 data

      // Verify the full file contains both chunks
      const fullData = await tree.readFile(cid2!);
      expect(fullData!.length).toBe(100);
      expect(fullData!.slice(0, 50)).toEqual(chunk1);
      expect(fullData!.slice(50)).toEqual(chunk2);
    });

    it('should support continuous incremental reads as stream grows', async () => {
      // Simulates a viewer watching a live stream for extended period
      const chunkSize = 100;
      const streamTree = new HashTree({ store, chunkSize });
      const stream = streamTree.createStream();

      let viewerOffset = 0;
      const receivedChunks: Uint8Array[] = [];

      // Simulate 10 "seconds" of streaming
      for (let second = 0; second < 10; second++) {
        // Streamer adds data
        const newChunk = new Uint8Array(30).fill(second);
        await stream.append(newChunk);
        const currentCid = await stream.currentRoot();

        // Viewer fetches new data from their last position
        const newData = await tree.readFileRange(currentCid!, viewerOffset);

        if (newData && newData.length > 0) {
          receivedChunks.push(newData);
          viewerOffset += newData.length;
        }
      }

      // Verify viewer received all data incrementally
      const totalReceived = receivedChunks.reduce((sum, c) => sum + c.length, 0);
      expect(totalReceived).toBe(300); // 10 chunks * 30 bytes

      // Verify data integrity - each 30-byte segment should be filled with its index
      let offset = 0;
      for (let i = 0; i < 10; i++) {
        // Find the data at this offset across received chunks
        let remaining = 30;
        let chunkOffset = 0;
        for (const chunk of receivedChunks) {
          if (chunkOffset + chunk.length <= offset) {
            chunkOffset += chunk.length;
            continue;
          }
          const startInChunk = Math.max(0, offset - chunkOffset);
          const available = chunk.length - startInChunk;
          const toCheck = Math.min(available, remaining);
          for (let j = 0; j < toCheck; j++) {
            expect(chunk[startInChunk + j]).toBe(i);
          }
          remaining -= toCheck;
          offset += toCheck;
          chunkOffset += chunk.length;
          if (remaining === 0) break;
        }
      }
    });

    it('should handle viewer joining mid-stream', async () => {
      // Stream has been running, viewer joins late
      const chunkSize = 100;
      const streamTree = new HashTree({ store, chunkSize });
      const stream = streamTree.createStream();

      // Stream already has 5 seconds of content
      for (let i = 0; i < 5; i++) {
        await stream.append(new Uint8Array(50).fill(i));
      }
      const midStreamCid = await stream.currentRoot();
      const midStreamSize = 250; // 5 * 50

      // Viewer joins now - in live mode, seek to near end (e.g., last 100 bytes)
      const seekPosition = Math.max(0, midStreamSize - 100);
      const catchUpData = await tree.readFileRange(midStreamCid!, seekPosition);
      expect(catchUpData!.length).toBe(100); // Last 100 bytes

      // Now viewer tracks from this position
      let viewerOffset = midStreamSize;

      // More data arrives
      await stream.append(new Uint8Array(50).fill(99));
      const newCid = await stream.currentRoot();

      // Viewer gets only the new 50 bytes
      const newData = await tree.readFileRange(newCid!, viewerOffset);
      expect(newData!.length).toBe(50);
      expect(newData![0]).toBe(99);
    });
  });
});
