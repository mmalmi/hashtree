/**
 * Tests for chunk metadata store
 */
import { describe, it, expect, beforeEach } from 'vitest';
import {
  registerChunks,
  touchChunks,
  getTreeSyncState,
  updateTreeSyncState,
  removeTreeSyncState,
  getStorageByUser,
  getOtherUsersWithTrees,
  getTotalTrackedBytes,
  getChunksToEvict,
  removeChunks,
  getOrphanedChunks,
  clearAllMetadata,
  chunkMetadataDb,
  type TreeSyncState,
} from '../src/stores/chunkMetadata';

describe('ChunkMetadata Store', () => {
  beforeEach(async () => {
    await clearAllMetadata();
  });

  describe('registerChunks', () => {
    it('should register new chunks with owner', async () => {
      const chunks = [
        { hashHex: 'aaa111', size: 100 },
        { hashHex: 'bbb222', size: 200 },
      ];

      await registerChunks('npub1abc/tree1', chunks);

      const stored = await chunkMetadataDb.chunks.toArray();
      expect(stored).toHaveLength(2);
      expect(stored[0].owners).toContain('npub1abc/tree1');
      expect(stored[1].owners).toContain('npub1abc/tree1');
    });

    it('should add tree to existing chunk owners', async () => {
      // First registration
      await registerChunks('npub1abc/tree1', [{ hashHex: 'aaa111', size: 100 }]);

      // Second registration of same chunk by different tree
      await registerChunks('npub1abc/tree2', [{ hashHex: 'aaa111', size: 100 }]);

      const chunk = await chunkMetadataDb.chunks.get('aaa111');
      expect(chunk?.owners).toHaveLength(2);
      expect(chunk?.owners).toContain('npub1abc/tree1');
      expect(chunk?.owners).toContain('npub1abc/tree2');
    });

    it('should update lastAccessed on re-registration', async () => {
      await registerChunks('npub1abc/tree1', [{ hashHex: 'aaa111', size: 100 }]);
      const first = await chunkMetadataDb.chunks.get('aaa111');

      // Wait a bit
      await new Promise(r => setTimeout(r, 10));

      await registerChunks('npub1abc/tree1', [{ hashHex: 'aaa111', size: 100 }]);
      const second = await chunkMetadataDb.chunks.get('aaa111');

      expect(second!.lastAccessed).toBeGreaterThanOrEqual(first!.lastAccessed);
    });
  });

  describe('touchChunks', () => {
    it('should update lastAccessed timestamp', async () => {
      await registerChunks('npub1abc/tree1', [{ hashHex: 'aaa111', size: 100 }]);
      const before = await chunkMetadataDb.chunks.get('aaa111');

      await new Promise(r => setTimeout(r, 10));
      await touchChunks(['aaa111']);

      const after = await chunkMetadataDb.chunks.get('aaa111');
      expect(after!.lastAccessed).toBeGreaterThan(before!.lastAccessed);
    });
  });

  describe('TreeSyncState', () => {
    it('should create and retrieve tree sync state', async () => {
      const state: TreeSyncState = {
        key: 'npub1abc/tree1',
        ownerNpub: 'npub1abc',
        rootHash: 'hash123',
        totalBytes: 1000,
        lastSynced: Date.now(),
        isOwn: false,
      };

      await updateTreeSyncState(state);
      const retrieved = await getTreeSyncState('npub1abc/tree1');

      expect(retrieved).toEqual(state);
    });

    it('should update existing tree sync state', async () => {
      const state: TreeSyncState = {
        key: 'npub1abc/tree1',
        ownerNpub: 'npub1abc',
        rootHash: 'hash123',
        totalBytes: 1000,
        lastSynced: Date.now(),
        isOwn: false,
      };

      await updateTreeSyncState(state);
      await updateTreeSyncState({ ...state, rootHash: 'hash456', totalBytes: 2000 });

      const retrieved = await getTreeSyncState('npub1abc/tree1');
      expect(retrieved?.rootHash).toBe('hash456');
      expect(retrieved?.totalBytes).toBe(2000);
    });

    it('should remove tree sync state', async () => {
      await updateTreeSyncState({
        key: 'npub1abc/tree1',
        ownerNpub: 'npub1abc',
        rootHash: 'hash123',
        totalBytes: 1000,
        lastSynced: Date.now(),
        isOwn: false,
      });

      await removeTreeSyncState('npub1abc/tree1');
      const retrieved = await getTreeSyncState('npub1abc/tree1');

      expect(retrieved).toBeUndefined();
    });
  });

  describe('getStorageByUser', () => {
    it('should calculate total bytes for a user', async () => {
      await updateTreeSyncState({
        key: 'npub1abc/tree1',
        ownerNpub: 'npub1abc',
        rootHash: 'hash1',
        totalBytes: 1000,
        lastSynced: Date.now(),
        isOwn: false,
      });

      await updateTreeSyncState({
        key: 'npub1abc/tree2',
        ownerNpub: 'npub1abc',
        rootHash: 'hash2',
        totalBytes: 2000,
        lastSynced: Date.now(),
        isOwn: false,
      });

      const total = await getStorageByUser('npub1abc');
      expect(total).toBe(3000);
    });

    it('should return 0 for unknown user', async () => {
      const total = await getStorageByUser('npub1xyz');
      expect(total).toBe(0);
    });
  });

  describe('getOtherUsersWithTrees', () => {
    it('should return other users excluding own accounts', async () => {
      await updateTreeSyncState({
        key: 'npub1abc/tree1',
        ownerNpub: 'npub1abc',
        rootHash: 'hash1',
        totalBytes: 1000,
        lastSynced: Date.now(),
        isOwn: true,
      });

      await updateTreeSyncState({
        key: 'npub1def/tree1',
        ownerNpub: 'npub1def',
        rootHash: 'hash2',
        totalBytes: 2000,
        lastSynced: Date.now(),
        isOwn: false,
      });

      await updateTreeSyncState({
        key: 'npub1ghi/tree1',
        ownerNpub: 'npub1ghi',
        rootHash: 'hash3',
        totalBytes: 3000,
        lastSynced: Date.now(),
        isOwn: false,
      });

      const others = await getOtherUsersWithTrees(['npub1abc']);
      expect(others).toHaveLength(2);
      expect(others).toContain('npub1def');
      expect(others).toContain('npub1ghi');
      expect(others).not.toContain('npub1abc');
    });
  });

  describe('getChunksToEvict', () => {
    it('should return chunks in LRU order', async () => {
      // Register chunks with different access times
      await registerChunks('npub1abc/tree1', [
        { hashHex: 'chunk1', size: 100 },
      ]);

      await new Promise(r => setTimeout(r, 10));

      await registerChunks('npub1abc/tree1', [
        { hashHex: 'chunk2', size: 100 },
      ]);

      await new Promise(r => setTimeout(r, 10));

      await registerChunks('npub1abc/tree1', [
        { hashHex: 'chunk3', size: 100 },
      ]);

      // Update tree sync state
      await updateTreeSyncState({
        key: 'npub1abc/tree1',
        ownerNpub: 'npub1abc',
        rootHash: 'hash1',
        totalBytes: 300,
        lastSynced: Date.now(),
        isOwn: false,
      });

      // Evict 150 bytes - should get chunk1 (oldest) then chunk2
      const toEvict = await getChunksToEvict('npub1abc', 150);
      expect(toEvict.length).toBeGreaterThanOrEqual(2);
      expect(toEvict[0]).toBe('chunk1');
    });

    it('should not evict chunks owned by other users', async () => {
      // Chunk owned by both user A and user B
      await registerChunks('npub1abc/tree1', [{ hashHex: 'shared', size: 100 }]);
      await registerChunks('npub1def/tree1', [{ hashHex: 'shared', size: 100 }]);

      // Chunk owned only by user A
      await registerChunks('npub1abc/tree2', [{ hashHex: 'onlyA', size: 100 }]);

      await updateTreeSyncState({
        key: 'npub1abc/tree1',
        ownerNpub: 'npub1abc',
        rootHash: 'hash1',
        totalBytes: 100,
        lastSynced: Date.now(),
        isOwn: false,
      });

      await updateTreeSyncState({
        key: 'npub1abc/tree2',
        ownerNpub: 'npub1abc',
        rootHash: 'hash2',
        totalBytes: 100,
        lastSynced: Date.now(),
        isOwn: false,
      });

      // Evict from user A - should not include 'shared' chunk
      const toEvict = await getChunksToEvict('npub1abc', 200);
      expect(toEvict).not.toContain('shared');
      expect(toEvict).toContain('onlyA');
    });
  });

  describe('getOrphanedChunks', () => {
    it('should find chunks with no owners', async () => {
      await registerChunks('npub1abc/tree1', [{ hashHex: 'chunk1', size: 100 }]);

      // Manually remove owner
      await chunkMetadataDb.chunks.update('chunk1', { owners: [] });

      const orphans = await getOrphanedChunks();
      expect(orphans).toContain('chunk1');
    });
  });

  describe('getTotalTrackedBytes', () => {
    it('should sum all chunk sizes', async () => {
      await registerChunks('npub1abc/tree1', [
        { hashHex: 'chunk1', size: 100 },
        { hashHex: 'chunk2', size: 200 },
        { hashHex: 'chunk3', size: 300 },
      ]);

      const total = await getTotalTrackedBytes();
      expect(total).toBe(600);
    });
  });
});
