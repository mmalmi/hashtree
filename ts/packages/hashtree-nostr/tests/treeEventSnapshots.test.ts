import { describe, expect, it } from 'vitest';
import { HashTree, MemoryStore, cid, fromHex, toHex } from '@hashtree/core';
import {
  storeTreeEventSnapshot,
  fetchLatestTreeEventSnapshot,
  readTreeEventSnapshot,
  resolveSnapshotRootCid,
  snapshotMatchesRootCid,
  watchLatestTreeEventSnapshot,
  type TreeEventSnapshotInfo,
} from '../src/treeEventSnapshots.js';
import { HASHTREE_ROOT_KIND } from '../src/snapshot.js';
import type { StoredNostrEvent } from '../src/events.js';
import type { Nip19Like } from '../src/resolver/nostr.js';

const nip19: Nip19Like = {
  decode(value: string) {
    if (!value.startsWith('npub1') || value.length !== 69) {
      throw new Error('invalid npub');
    }
    return { type: 'npub', data: value.slice(5) };
  },
  npubEncode(pubkey: string) {
    return `npub1${pubkey}`;
  },
};

function makeEvent(overrides: Partial<StoredNostrEvent> = {}): StoredNostrEvent {
  return {
    id: '1'.repeat(64),
    pubkey: '2'.repeat(64),
    created_at: 1_700_000_000,
    kind: HASHTREE_ROOT_KIND,
    tags: [
      ['d', 'videos/demo'],
      ['l', 'hashtree'],
      ['hash', '3'.repeat(64)],
      ['key', '4'.repeat(64)],
    ],
    content: '',
    sig: '5'.repeat(128),
    ...overrides,
  };
}

function makeSnapshot(overrides: Partial<TreeEventSnapshotInfo> = {}): TreeEventSnapshotInfo {
  const rootCid = cid(fromHex('3'.repeat(64)), fromHex('4'.repeat(64)));
  return {
    event: makeEvent(),
    treeName: 'videos/demo',
    rootCid,
    visibility: 'public',
    labels: ['hashtree'],
    snapshotCid: cid(fromHex('6'.repeat(64))),
    snapshotNhash: 'nhash1snapshot',
    npub: nip19.npubEncode('2'.repeat(64)),
    ...overrides,
  };
}

async function waitForLength(values: unknown[], expectedLength: number, timeoutMs = 250): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    if (values.length >= expectedLength) {
      return;
    }
    await new Promise((resolve) => {
      setTimeout(resolve, 1);
    });
  }
  throw new Error(`Timed out waiting for ${expectedLength} values`);
}

describe('tree event snapshots', () => {
  it('stores and reloads signed tree event snapshots', async () => {
    const store = new MemoryStore();
    const tree = new HashTree({ store });
    const event = makeEvent();

    const snapshot = await storeTreeEventSnapshot(tree, nip19, event);
    const restored = snapshot ? await readTreeEventSnapshot(tree, nip19, snapshot.snapshotCid) : null;

    expect(snapshot).not.toBeNull();
    expect(snapshot?.snapshotNhash).toMatch(/^nhash1/);
    expect(snapshot?.npub).toBe(nip19.npubEncode(event.pubkey));
    expect(restored).toEqual(snapshot);
  });

  it('fetches the latest snapshot by created_at and event id', async () => {
    const store = new MemoryStore();
    const tree = new HashTree({ store });

    const snapshot = await fetchLatestTreeEventSnapshot(
      {
        snapshotTarget: tree,
        nip19,
        fetchEvents: async () => [
          makeEvent({
            id: '1'.repeat(64),
            created_at: 10,
            tags: [
              ['d', 'videos/demo'],
              ['l', 'hashtree'],
              ['hash', 'a'.repeat(64)],
              ['key', '4'.repeat(64)],
            ],
          }),
          makeEvent({
            id: 'f'.repeat(64),
            created_at: 10,
            tags: [
              ['d', 'videos/demo'],
              ['l', 'hashtree'],
              ['hash', 'b'.repeat(64)],
              ['key', '4'.repeat(64)],
            ],
          }),
          makeEvent({
            id: 'e'.repeat(64),
            created_at: 11,
            tags: [
              ['d', 'videos/demo'],
              ['l', 'hashtree'],
              ['hash', 'c'.repeat(64)],
              ['key', '4'.repeat(64)],
            ],
          }),
        ],
      },
      nip19.npubEncode('2'.repeat(64)),
      'videos/demo',
    );

    expect(snapshot?.event.created_at).toBe(11);
    expect(toHex(snapshot!.rootCid.hash)).toBe('c'.repeat(64));
  });

  it('watches the latest snapshot until closed', async () => {
    const store = new MemoryStore();
    const emitted: string[] = [];
    let onEvent: ((event: StoredNostrEvent) => void) | null = null;
    let unsubscribed = false;
    const initial = makeEvent({
      id: '1'.repeat(64),
      created_at: 10,
      tags: [
        ['d', 'videos/demo'],
        ['l', 'hashtree'],
        ['hash', 'a'.repeat(64)],
        ['key', '4'.repeat(64)],
      ],
    });
    const newer = makeEvent({
      id: '2'.repeat(64),
      created_at: 11,
      tags: [
        ['d', 'videos/demo'],
        ['l', 'hashtree'],
        ['hash', 'b'.repeat(64)],
        ['key', '4'.repeat(64)],
      ],
    });
    const newest = makeEvent({
      id: '3'.repeat(64),
      created_at: 12,
      tags: [
        ['d', 'videos/demo'],
        ['l', 'hashtree'],
        ['hash', 'c'.repeat(64)],
        ['key', '4'.repeat(64)],
      ],
    });

    const stop = watchLatestTreeEventSnapshot(
      {
        snapshotTarget: store,
        nip19,
        fetchEvents: async () => [initial],
        subscribeEvents: (_filter, handler) => {
          onEvent = handler;
          return () => {
            unsubscribed = true;
          };
        },
      },
      nip19.npubEncode('2'.repeat(64)),
      'videos/demo',
      (snapshot) => {
        emitted.push(snapshot.event.id);
      },
    );

    await waitForLength(emitted, 1);
    expect(emitted).toEqual([initial.id]);

    onEvent?.(initial);
    await new Promise((resolve) => {
      setTimeout(resolve, 5);
    });
    expect(emitted).toEqual([initial.id]);

    onEvent?.(newer);
    await waitForLength(emitted, 2);
    expect(emitted).toEqual([initial.id, newer.id]);

    stop();
    expect(unsubscribed).toBe(true);

    onEvent?.(newest);
    await new Promise((resolve) => {
      setTimeout(resolve, 5);
    });
    expect(emitted).toEqual([initial.id, newer.id]);
  });

  it('resolves link-visible snapshots with the supplied link key', async () => {
    const linkKey = fromHex('7'.repeat(64));
    const contentKey = fromHex('8'.repeat(64));
    const encryptedKey = contentKey.map((byte, index) => byte ^ linkKey[index]);
    const snapshot = makeSnapshot({
      visibility: 'link-visible',
      rootCid: cid(fromHex('3'.repeat(64))),
      encryptedKey: toHex(encryptedKey),
    });

    const resolved = await resolveSnapshotRootCid(snapshot, toHex(linkKey));

    expect(resolved).toEqual(cid(fromHex('3'.repeat(64)), contentKey));
  });

  it('matches snapshots by root hash even when the public key is omitted', () => {
    const snapshot = makeSnapshot();

    expect(snapshotMatchesRootCid(snapshot, cid(fromHex('3'.repeat(64))))).toBe(true);
    expect(snapshotMatchesRootCid(snapshot, cid(fromHex('a'.repeat(64))))).toBe(false);
  });
});
