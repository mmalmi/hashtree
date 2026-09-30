import { beforeEach, describe, expect, it, vi } from 'vitest';
import { nip19 } from 'nostr-tools';
import { HASHTREE_ROOT_KIND, HASHTREE_ROOT_KINDS } from '@hashtree/nostr';

const query = vi.fn();
const nostrSubscribe = vi.fn();
const nostrUnsubscribe = vi.fn();
const getCachedRoot = vi.fn();
const getTreeRootCacheStore = vi.fn();
const setCachedRoot = vi.fn();

vi.mock('../src/relay/nostrRuntime', () => ({
  query,
  subscribe: nostrSubscribe,
  unsubscribe: nostrUnsubscribe,
}));

vi.mock('../src/relay/treeRootCache', () => ({
  getCachedRoot,
  getTreeRootCacheStore,
  setCachedRoot,
}));

function hexToBytes(hex: string): Uint8Array {
  return Uint8Array.from(
    { length: hex.length / 2 },
    (_, index) => parseInt(hex.slice(index * 2, index * 2 + 2), 16),
  );
}

describe('tree root freshness', () => {
  beforeEach(() => {
    vi.resetModules();
    query.mockReset();
    nostrSubscribe.mockReset();
    nostrUnsubscribe.mockReset();
    getCachedRoot.mockReset();
    getTreeRootCacheStore.mockReset();
    setCachedRoot.mockReset();
  });

  it('queries exact tree roots through the shared event runtime', async () => {
    const pubkey = 'f'.repeat(64);
    const npub = nip19.npubEncode(pubkey);
    const treeName = 'hashtree';
    const hashHex = 'a'.repeat(64);
    query.mockResolvedValue({ complete: true, events: [{
      id: 'evt1',
      pubkey,
      kind: HASHTREE_ROOT_KIND,
      content: '',
      tags: [
        ['d', treeName],
        ['l', 'hashtree'],
        ['hash', hashHex],
      ],
      created_at: 123,
      sig: 'sig',
    }] });

    getCachedRoot.mockResolvedValue(null);
    getTreeRootCacheStore.mockReturnValue(null);
    setCachedRoot.mockResolvedValue({
      applied: true,
      record: {
        hash: hexToBytes(hashHex),
        key: undefined,
        visibility: 'public',
        updatedAt: 123,
      },
    });

    const { resolveTreeRootNow } = await import('../src/relay/treeRootSubscription');
    await resolveTreeRootNow(npub, treeName, 1000);

    expect(query).toHaveBeenCalledWith(
      [expect.objectContaining({
        kinds: [...HASHTREE_ROOT_KINDS],
        authors: [pubkey],
        '#d': [treeName],
      })],
      expect.objectContaining({
        deadline: expect.any(Number),
      }),
    );
  });

  it('keeps a live subscription while replaying cached roots', async () => {
    const pubkey = 'e'.repeat(64);

    const { subscribeToTreeRoots } = await import('../src/relay/treeRootSubscription');
    subscribeToTreeRoots(pubkey);

    expect(nostrSubscribe).toHaveBeenCalledWith(
      `tree-${pubkey.slice(0, 8)}`,
      [{
        kinds: [...HASHTREE_ROOT_KINDS],
        authors: [pubkey],
      }],
      expect.objectContaining({
        cache: 'cache-first',
      }),
    );
  });
});
