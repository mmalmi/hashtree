import { MemoryStore, fromHex, toHex } from '@hashtree/core';
import { beforeEach, describe, expect, it } from 'vitest';
import {
  clearMemoryCache,
  getCachedRootInfo,
  initTreeRootCache,
  setCachedRoot,
} from '../src/relay/treeRootCache';

const HASH_A = fromHex('11'.repeat(32));
const HASH_B = fromHex('33'.repeat(32));
const KEY_A = fromHex('22'.repeat(32));

describe('treeRootCache', () => {
  beforeEach(() => {
    clearMemoryCache();
    initTreeRootCache(new MemoryStore());
  });

  it('preserves same-hash visibility metadata when a later cache sync omits it', async () => {
    const npub = 'npub-worker-cache-metadata';
    const treeName = 'boards/worker-cache-metadata';

    await setCachedRoot(npub, treeName, { hash: HASH_A, key: KEY_A }, 'link-visible', {
      updatedAt: 100,
      encryptedKey: 'aa'.repeat(32),
      keyId: 'key-id-3',
      selfEncryptedLinkKey: 'bb'.repeat(32),
    });

    await setCachedRoot(npub, treeName, { hash: HASH_A }, 'link-visible', {
      updatedAt: 200,
    });

    const cached = await getCachedRootInfo(npub, treeName);
    expect(cached).toBeTruthy();
    expect(cached?.key && toHex(cached.key)).toBe(toHex(KEY_A));
    expect(cached?.encryptedKey).toBe('aa'.repeat(32));
    expect(cached?.keyId).toBe('key-id-3');
    expect(cached?.selfEncryptedLinkKey).toBe('bb'.repeat(32));
  });

  it('preserves same-hash snapshot metadata when a later cache sync omits it', async () => {
    const npub = 'npub-worker-cache-snapshot';
    const treeName = 'sites/worker-cache-snapshot';

    await setCachedRoot(npub, treeName, { hash: HASH_A }, 'public', {
      updatedAt: 100,
      snapshotNhash: 'nhash1snapshotcache',
    });

    await setCachedRoot(npub, treeName, { hash: HASH_A }, 'public', {
      updatedAt: 200,
    });

    const cached = await getCachedRootInfo(npub, treeName);
    expect(cached?.snapshotNhash).toBe('nhash1snapshotcache');
  });

  it('clears stale key metadata when a newer same-hash event makes the tree public', async () => {
    const npub = 'npub-worker-cache-public-reset';
    const treeName = 'profiles/public-reset';

    await setCachedRoot(npub, treeName, { hash: HASH_A, key: KEY_A }, 'link-visible', {
      updatedAt: 100,
      encryptedKey: 'aa'.repeat(32),
      keyId: 'key-id-9',
      selfEncryptedLinkKey: 'bb'.repeat(32),
    });

    await setCachedRoot(npub, treeName, { hash: HASH_A }, 'public', {
      updatedAt: 200,
    });

    const cached = await getCachedRootInfo(npub, treeName);
    expect(cached).toBeTruthy();
    expect(cached?.key).toBeUndefined();
    expect(cached?.encryptedKey).toBeUndefined();
    expect(cached?.keyId).toBeUndefined();
    expect(cached?.selfEncryptedLinkKey).toBeUndefined();
    expect(cached?.visibility).toBe('public');
  });

  it('lets an authoritative local cache sync replace a same-second relay event', async () => {
    const npub = 'npub-worker-cache-local-write';
    const treeName = 'main';

    await setCachedRoot(npub, treeName, { hash: HASH_A }, 'public', {
      updatedAt: 100,
      eventId: 'ff'.repeat(32),
    });

    const rejected = await setCachedRoot(npub, treeName, { hash: HASH_B }, 'public', {
      updatedAt: 100,
    });
    expect(rejected.applied).toBe(false);

    const applied = await setCachedRoot(npub, treeName, { hash: HASH_B }, 'public', {
      updatedAt: 100,
      force: true,
    });
    expect(applied.applied).toBe(true);
    expect(applied.record.hash).toEqual(HASH_B);
  });
});
