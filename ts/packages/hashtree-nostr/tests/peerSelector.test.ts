import { describe, expect, it } from 'vitest';
import {
  PeerSelector,
  buildHedgedWavePlan,
  normalizeDispatchConfig,
} from '../src/webrtc/peerSelector.js';
import type { RequestDispatchConfig } from '../src/webrtc/types.js';

describe('PeerSelector', () => {
  it('orders peers by weighted strategy using reliability and latency', () => {
    const selector = PeerSelector.withStrategy('weighted');
    selector.addPeer('peer-fast:1');
    selector.addPeer('peer-bad:1');

    for (let i = 0; i < 4; i++) {
      selector.recordRequest('peer-fast:1', 40);
      selector.recordSuccess('peer-fast:1', 25, 4096);
    }

    for (let i = 0; i < 4; i++) {
      selector.recordRequest('peer-bad:1', 40);
      selector.recordTimeout('peer-bad:1');
    }

    const ordered = selector.selectPeers();
    expect(ordered[0]).toBe('peer-fast:1');
  });

  it('supports utility-ucb ranking strategy', () => {
    const selector = PeerSelector.withStrategy('utilityUcb');
    selector.addPeer('peer-good:1');
    selector.addPeer('peer-poor:1');

    for (let i = 0; i < 6; i++) {
      selector.recordRequest('peer-good:1', 64);
      selector.recordSuccess('peer-good:1', 20, 2048);
    }
    for (let i = 0; i < 6; i++) {
      selector.recordRequest('peer-poor:1', 64);
      selector.recordFailure('peer-poor:1');
    }

    const ordered = selector.selectPeers();
    expect(ordered[0]).toBe('peer-good:1');
  });

  it('applies fairness gate when one peer dominates and pool is large enough', () => {
    const selector = PeerSelector.withStrategy('weighted');
    selector.setFairness(true);

    const peerIds = ['p0:1', 'p1:1', 'p2:1', 'p3:1', 'p4:1'];
    for (const id of peerIds) selector.addPeer(id);

    for (let i = 0; i < 60; i++) selector.recordRequest('p0:1', 32);

    const ordered = selector.selectPeers();
    expect(ordered[0]).not.toBe('p0:1');
  });

  it('backs off timeout-heavy peers', () => {
    const selector = PeerSelector.withStrategy('weighted');
    selector.addPeer('stable:1');
    selector.addPeer('timeout:1');

    selector.recordRequest('stable:1', 48);
    selector.recordSuccess('stable:1', 30, 1024);

    selector.recordRequest('timeout:1', 48);
    selector.recordTimeout('timeout:1');

    const ordered = selector.selectPeers();
    expect(ordered[ordered.length - 1]).toBe('timeout:1');
  });

  it('persists and restores metadata by stable principal identity', () => {
    const source = PeerSelector.withStrategy('weighted');
    source.addPeer('fav-pubkey:old-session');
    source.recordRequest('fav-pubkey:old-session', 32);
    source.recordSuccess('fav-pubkey:old-session', 15, 4096);

    const snapshot = source.exportPeerMetadataSnapshot();
    expect(snapshot.version).toBe(1);
    expect(snapshot.peers.length).toBe(1);

    const restored = PeerSelector.withStrategy('weighted');
    restored.importPeerMetadataSnapshot(snapshot);
    restored.addPeer('fav-pubkey:new-session');
    restored.addPeer('other-pubkey:session');

    const ordered = restored.selectPeers();
    expect(ordered[0]).toBe('fav-pubkey:new-session');
  });
});

describe('dispatch helpers', () => {
  it('normalizes and bounds dispatch config', () => {
    const input: RequestDispatchConfig = {
      initialFanout: 0,
      hedgeFanout: 0,
      maxFanout: 0,
      hedgeIntervalMs: 120,
    };
    const normalized = normalizeDispatchConfig(input, 3);
    expect(normalized.initialFanout).toBe(1);
    expect(normalized.hedgeFanout).toBe(1);
    expect(normalized.maxFanout).toBe(3);
  });

  it('builds staged hedged wave plan', () => {
    const waves = buildHedgedWavePlan(5, {
      initialFanout: 2,
      hedgeFanout: 1,
      maxFanout: 4,
      hedgeIntervalMs: 100,
    });
    expect(waves).toEqual([2, 1, 1]);
  });
});
