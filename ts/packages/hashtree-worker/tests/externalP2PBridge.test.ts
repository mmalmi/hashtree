import { afterEach, describe, expect, test, vi } from 'vitest';
import { ExternalP2PBridge } from '../src/relay/externalP2P.js';

afterEach(() => {
  vi.useRealTimers();
});

describe('ExternalP2PBridge', () => {
  test('routes fetches and deduplicates peer lists returned by the provider', async () => {
    const requests: Array<Record<string, unknown>> = [];
    const bridge = new ExternalP2PBridge({
      respond: (request) => requests.push(request),
      fetchTimeoutMs: 1000,
      peerListTimeoutMs: 1000,
    });
    bridge.setEnabled(true);

    const fetch = bridge.fetch('ab'.repeat(32), 'peer-a');
    const fetchRequest = requests[0];
    bridge.resolveFetch(fetchRequest.requestId as string, new Uint8Array([1, 2]));
    await expect(fetch).resolves.toEqual(new Uint8Array([1, 2]));

    const peers = bridge.listPeers();
    const peerRequest = requests[1];
    bridge.resolvePeerList(peerRequest.requestId as string, ['peer-b', 'peer-a', 'peer-b']);
    await expect(peers).resolves.toEqual(['peer-b', 'peer-a']);
  });

  test('settles pending work when disabled and bounds unanswered requests', async () => {
    vi.useFakeTimers();
    const bridge = new ExternalP2PBridge({
      respond: () => undefined,
      fetchTimeoutMs: 50,
      peerListTimeoutMs: 50,
    });
    bridge.setEnabled(true);

    const timedOut = bridge.fetch('cd'.repeat(32));
    await vi.advanceTimersByTimeAsync(50);
    await expect(timedOut).resolves.toBeNull();

    const peers = bridge.listPeers();
    bridge.setEnabled(false);
    await expect(peers).resolves.toEqual([]);
  });
});
