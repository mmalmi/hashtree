import { describe, it, expect } from 'vitest';
import { fromHex, toHex } from '@hashtree/core';
import { treeRootRegistry } from '../src/tree-root';

const HASH_A = fromHex('11'.repeat(32));
const HASH_B = fromHex('33'.repeat(32));
const KEY_A = fromHex('22'.repeat(32));
const KEY_B = fromHex('44'.repeat(32));

describe('tree root registry same-hash merges', () => {
  it('merges key and visibility metadata from older worker updates when hash is unchanged', () => {
    const npub = 'npub-test-worker-merge';
    const treeName = 'boards/test-worker-merge';

    treeRootRegistry.delete(npub, treeName);
    treeRootRegistry.setFromExternal(npub, treeName, HASH_A, 'prefetch', {
      updatedAt: 200,
      visibility: 'public',
    });

    const updated = treeRootRegistry.setFromWorker(npub, treeName, HASH_A, 100, {
      key: KEY_A,
      visibility: 'link-visible',
      encryptedKey: 'aa'.repeat(32),
      keyId: 'key-id-1',
      selfEncryptedLinkKey: 'bb'.repeat(32),
    });

    expect(updated).toBe(true);
    const record = treeRootRegistry.get(npub, treeName);
    expect(record).not.toBeNull();
    expect(record?.hash && toHex(record.hash)).toBe(toHex(HASH_A));
    expect(record?.key && toHex(record.key)).toBe(toHex(KEY_A));
    expect(record?.visibility).toBe('link-visible');
    expect(record?.encryptedKey).toBe('aa'.repeat(32));
    expect(record?.keyId).toBe('key-id-1');
    expect(record?.selfEncryptedLinkKey).toBe('bb'.repeat(32));
  });

  it('still rejects older updates when the hash changes', () => {
    const npub = 'npub-test-older-hash';
    const treeName = 'boards/test-older-hash';

    treeRootRegistry.delete(npub, treeName);
    treeRootRegistry.setFromExternal(npub, treeName, HASH_A, 'prefetch', {
      updatedAt: 200,
      visibility: 'public',
    });

    const updated = treeRootRegistry.setFromWorker(npub, treeName, HASH_B, 100, {
      key: KEY_A,
      visibility: 'link-visible',
    });

    expect(updated).toBe(false);
    const record = treeRootRegistry.get(npub, treeName);
    expect(record).not.toBeNull();
    expect(record?.hash && toHex(record.hash)).toBe(toHex(HASH_A));
    expect(record?.key).toBeUndefined();
    expect(record?.visibility).toBe('public');
  });

  it('preserves labels across local writes when autosave does not resend them', () => {
    const npub = 'npub-test-label-preserve';
    const treeName = 'git/test-label-preserve';

    treeRootRegistry.delete(npub, treeName);
    treeRootRegistry.setFromExternal(npub, treeName, HASH_A, 'prefetch', {
      updatedAt: 200,
      visibility: 'public',
      labels: ['hashtree', 'git'],
    });

    treeRootRegistry.setLocal(npub, treeName, HASH_B, {
      visibility: 'public',
    });

    const record = treeRootRegistry.get(npub, treeName);
    expect(record).not.toBeNull();
    expect(record?.hash && toHex(record.hash)).toBe(toHex(HASH_B));
    expect(record?.labels).toEqual(['hashtree', 'git']);
  });

  it('replaces a stale same-hash key when the resolver later provides the trusted key', () => {
    const npub = 'npub-test-correct-key';
    const treeName = 'boards/test-correct-key';

    treeRootRegistry.delete(npub, treeName);
    treeRootRegistry.setFromExternal(npub, treeName, HASH_A, 'prefetch', {
      updatedAt: 200,
      visibility: 'link-visible',
      key: KEY_A,
      encryptedKey: 'aa'.repeat(32),
    });

    const updated = treeRootRegistry.setFromResolver(npub, treeName, HASH_A, 200, {
      key: KEY_B,
      visibility: 'link-visible',
      encryptedKey: 'aa'.repeat(32),
    });

    expect(updated).toBe(true);
    const record = treeRootRegistry.get(npub, treeName);
    expect(record).not.toBeNull();
    expect(record?.key && toHex(record.key)).toBe(toHex(KEY_B));
  });

  it('preserves link-visible metadata on same-hash external refreshes that omit it', () => {
    const npub = 'npub-test-external-metadata-preserve';
    const treeName = 'boards/test-external-metadata-preserve';

    treeRootRegistry.delete(npub, treeName);
    treeRootRegistry.setFromResolver(npub, treeName, HASH_A, 100, {
      key: KEY_A,
      visibility: 'link-visible',
      encryptedKey: 'aa'.repeat(32),
      keyId: 'key-id-2',
      selfEncryptedLinkKey: 'bb'.repeat(32),
    });

    treeRootRegistry.setFromExternal(npub, treeName, HASH_A, 'prefetch', {
      key: KEY_A,
      visibility: 'link-visible',
      updatedAt: 200,
    });

    const record = treeRootRegistry.get(npub, treeName);
    expect(record).not.toBeNull();
    expect(record?.key && toHex(record.key)).toBe(toHex(KEY_A));
    expect(record?.visibility).toBe('link-visible');
    expect(record?.encryptedKey).toBe('aa'.repeat(32));
    expect(record?.keyId).toBe('key-id-2');
    expect(record?.selfEncryptedLinkKey).toBe('bb'.repeat(32));
  });

  it('orders rapid local writes when an older publish is already in flight', () => {
    const npub = 'npub-test-local-order';
    const treeName = 'boards/test-local-order';

    treeRootRegistry.delete(npub, treeName);
    treeRootRegistry.setLocal(npub, treeName, HASH_A, {
      key: KEY_A,
      visibility: 'private',
    });
    const staleInFlightRecord = treeRootRegistry.get(npub, treeName);
    expect(staleInFlightRecord).not.toBeNull();

    treeRootRegistry.setLocal(npub, treeName, HASH_B, {
      key: KEY_B,
      visibility: 'private',
    });
    const latestRecord = treeRootRegistry.get(npub, treeName);
    expect(latestRecord).not.toBeNull();
    expect(staleInFlightRecord!.updatedAt).toBeLessThan(latestRecord!.updatedAt);
    expect(staleInFlightRecord?.hash && toHex(staleInFlightRecord.hash)).toBe(toHex(HASH_A));
    expect(latestRecord?.hash && toHex(latestRecord.hash)).toBe(toHex(HASH_B));
  });
});
