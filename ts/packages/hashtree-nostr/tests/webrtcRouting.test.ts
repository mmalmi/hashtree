import { describe, expect, it, vi, afterEach } from 'vitest';
import { MemoryStore, sha256 } from '@hashtree/core';
import { WebRTCStore } from '../src/webrtc/store.js';

function createStore(overrides: Partial<ConstructorParameters<typeof WebRTCStore>[0]> = {}): WebRTCStore {
  const signer = async (evt: {
    kind: number;
    created_at: number;
    tags: string[][];
    content: string;
  }) => ({
    id: '1'.repeat(64),
    pubkey: '2'.repeat(64),
    sig: '3'.repeat(128),
    kind: evt.kind,
    created_at: evt.created_at,
    tags: evt.tags,
    content: evt.content,
  });

  return new WebRTCStore({
    signer,
    pubkey: '2'.repeat(64),
    encrypt: async () => '',
    decrypt: async () => '',
    giftWrap: async (inner) => signer({
      kind: 25050,
      created_at: Math.floor(Date.now() / 1000),
      tags: [],
      content: JSON.stringify(inner),
    }),
    giftUnwrap: async () => null,
    relays: [],
    localStore: new MemoryStore(),
    ...overrides,
  });
}

afterEach(() => {
  vi.useRealTimers();
});

describe('WebRTCStore routing', () => {
  it('defaults retrieval routing to weighted reply-likelihood ordering', () => {
    const store = createStore();
    expect((store as any).routing.selectionStrategy).toBe('weighted');
  });

  it('uses staged hedged fanout rather than querying all peers at once', async () => {
    vi.useFakeTimers();
    const store = createStore({
      requestDispatch: {
        initialFanout: 2,
        hedgeFanout: 1,
        maxFanout: 3,
        hedgeIntervalMs: 50,
      },
    });

    const goodData = new TextEncoder().encode('hedged-data');
    const hash = await sha256(goodData);
    const calls: string[] = [];

    const never = () => new Promise<Uint8Array | null>(() => {});
    const peerA = { peerId: 'peer-a', isConnected: true, request: async () => { calls.push('a'); return never(); } };
    const peerB = { peerId: 'peer-b', isConnected: true, request: async () => { calls.push('b'); return never(); } };
    const peerC = { peerId: 'peer-c', isConnected: true, request: async () => { calls.push('c'); return goodData; } };

    (store as any).peers = new Map([
      ['peer-a', { pool: 'other', peer: peerA, hashGet: true }],
      ['peer-b', { pool: 'other', peer: peerB, hashGet: true }],
      ['peer-c', { pool: 'other', peer: peerC, hashGet: true }],
    ]);

    const pending = (store as any).fetchFromPeers(hash);
    await vi.advanceTimersByTimeAsync(0);
    expect(calls).toHaveLength(2);

    await vi.advanceTimersByTimeAsync(49);
    expect(calls).toHaveLength(2);

    await vi.advanceTimersByTimeAsync(1);
    expect(calls).toHaveLength(3);

    await expect(pending).resolves.toEqual(goodData);
  });

  it('ignores invalid payloads and continues to next peers', async () => {
    const store = createStore({
      requestDispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 0,
      },
    });

    const goodData = new TextEncoder().encode('valid-data');
    const badData = new TextEncoder().encode('invalid-data');
    const hash = await sha256(goodData);

    const peerBad = { peerId: 'peer-bad', isConnected: true, request: async () => badData };
    const peerGood = { peerId: 'peer-good', isConnected: true, request: async () => goodData };

    (store as any).peers = new Map([
      ['peer-bad', { pool: 'other', peer: peerBad, hashGet: true }],
      ['peer-good', { pool: 'other', peer: peerGood, hashGet: true }],
    ]);

    const result = await (store as any).fetchFromPeers(hash);
    expect(result).toEqual(goodData);
  });

  it('persists and reloads peer metadata snapshots', async () => {
    const localStore = new MemoryStore();
    const first = createStore({ localStore });
    const selector1 = (first as any).peerSelector;
    selector1.addPeer('fav-pub');
    selector1.recordRequest('fav-pub', 32);
    selector1.recordSuccess('fav-pub', 20, 1024);

    const snapshotHash = await first.persistPeerMetadata();
    expect(snapshotHash).not.toBeNull();

    const second = createStore({ localStore });
    const loaded = await second.loadPeerMetadata();
    expect(loaded).toBe(true);

    const selector2 = (second as any).peerSelector;
    selector2.addPeer('fav-pub');
    selector2.addPeer('other-pub');
    const ordered = selector2.selectPeers();
    expect(ordered[0]).toBe('fav-pub');
  });
});
