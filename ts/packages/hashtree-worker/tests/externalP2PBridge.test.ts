import { afterEach, describe, expect, test, vi } from 'vitest';
import { createBlobRequest, fromHex } from '@hashtree/core';
import { P2PBridge } from '../src/p2pBridge.js';

afterEach(() => {
  vi.useRealTimers();
});

describe('P2PBridge', () => {
  test('routes fetches and deduplicates peer lists returned by the provider', async () => {
    const requests: Array<Record<string, unknown>> = [];
    const bridge = new P2PBridge({
      respond: (request) => requests.push(request),
      fetchTimeoutMs: 1000,
      peerListTimeoutMs: 1000,
    });
    bridge.setEnabled(true);

    const fetch = bridge.fetch(createBlobRequest(fromHex('ab'.repeat(32))), 'peer-a');
    const fetchRequest = requests[0];
    expect(fetchRequest).toMatchObject({ htl: 10 });
    bridge.resolveFetch(fetchRequest.requestId as string, new Uint8Array([1, 2]));
    await expect(fetch).resolves.toEqual({ type: 'data', data: new Uint8Array([1, 2]) });

    const peers = bridge.listPeers();
    const peerRequest = requests[1];
    bridge.resolvePeerList(peerRequest.requestId as string, ['peer-b', 'peer-a', 'peer-b']);
    await expect(peers).resolves.toEqual(['peer-b', 'peer-a']);
  });

  test('settles pending work when disabled and bounds unanswered requests', async () => {
    vi.useFakeTimers();
    const bridge = new P2PBridge({
      respond: () => undefined,
      fetchTimeoutMs: 50,
      peerListTimeoutMs: 50,
    });
    bridge.setEnabled(true);

    const timedOut = bridge.fetch(createBlobRequest(fromHex('cd'.repeat(32))));
    const rejection = expect(timedOut).rejects.toThrow(/timed out/i);
    await vi.advanceTimersByTimeAsync(50);
    await rejection;

    const peers = bridge.listPeers();
    bridge.setEnabled(false);
    await expect(peers).resolves.toEqual([]);
  });

  test('preserves explicit misses separately from provider errors', async () => {
    const requests: Array<Record<string, unknown>> = [];
    const bridge = new P2PBridge({
      respond: (request) => requests.push(request),
      fetchTimeoutMs: 1000,
      peerListTimeoutMs: 1000,
    });
    bridge.setEnabled(true);

    const miss = bridge.fetch(createBlobRequest(fromHex('ab'.repeat(32))));
    bridge.resolveFetch(requests[0].requestId as string);
    await expect(miss).resolves.toEqual({ type: 'no-result' });

    const failed = bridge.fetch(createBlobRequest(fromHex('cd'.repeat(32))));
    bridge.resolveFetch(requests[1].requestId as string, undefined, 'provider unreachable');
    await expect(failed).rejects.toThrow('provider unreachable');
  });
});
