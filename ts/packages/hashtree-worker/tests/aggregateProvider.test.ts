import { afterEach, describe, expect, it, vi } from 'vitest';
import { createBlobRequest, sha256 } from '@hashtree/core';
import { BlobRouter } from '@hashtree/mesh';
import { P2PBridge, type P2PBridgeRequest } from '../src/p2pBridge.js';
import { P2PPeerRoutes } from '../src/p2pPeerRoutes.js';

const bridges: P2PBridge[] = [];

function aggregate() {
  const requests: P2PBridgeRequest[] = [];
  const bridge = new P2PBridge({
    respond: (request) => requests.push(request),
    peerListTimeoutMs: 5_000,
  });
  bridges.push(bridge);
  const route = new P2PPeerRoutes(bridge);
  route.setEnabled(true, false);
  const router = new BlobRouter([route], { requestTimeoutMs: 20_000 });
  return { bridge, route, router, requests };
}

afterEach(() => {
  for (const bridge of bridges.splice(0)) bridge.clear();
  vi.useRealTimers();
});

describe('aggregate P2P providers', () => {
  it('awaits one aggregate fetch beyond the listing budget without inventing a peer', async () => {
    vi.useFakeTimers();
    const { bridge, route, router, requests } = aggregate();
    const data = new Uint8Array([1, 2, 3]);
    const request = createBlobRequest(await sha256(data));
    const result = router.read(request);
    void result.catch(() => {});
    await vi.advanceTimersByTimeAsync(6_000);

    expect(requests).toEqual([{
      type: 'p2pFetch', requestId: expect.any(String), hashHex: expect.any(String), htl: 10,
    }]);
    await expect(route.peerList()).resolves.toEqual([]);
    expect(route.isAvailable()).toBe(true);
    bridge.resolveFetch(requests[0].requestId, data);
    await expect(result).resolves.toEqual({ type: 'data', data });
  });

  it('cancels the actual bridge request and ignores its late completion', async () => {
    const { bridge, route, requests } = aggregate();
    const data = new Uint8Array([4, 5, 6]);
    const request = createBlobRequest(await sha256(data));
    const controller = new AbortController();
    const canceled = route.read(request, { signal: controller.signal });
    const rejection = expect(canceled).rejects.toThrow('cancelled');
    expect(requests[0].type).toBe('p2pFetch');
    controller.abort();
    await rejection;
    bridge.resolveFetch(requests[0].requestId, new Uint8Array([99]));

    const next = route.read(request);
    void next.catch(() => {});
    expect(requests[1].type).toBe('p2pFetch');
    expect(requests[1].requestId).not.toBe(requests[0].requestId);
    bridge.resolveFetch(requests[1].requestId, data);
    await expect(next).resolves.toEqual({ type: 'data', data });
  });

  it('preserves incomplete discovery as an error and a provider miss as no result', async () => {
    const { bridge, route, requests } = aggregate();
    const request = createBlobRequest(await sha256(new Uint8Array([7])));
    const incomplete = route.read(request);
    const failure = expect(incomplete).rejects.toThrow('discovery is incomplete');
    expect(requests[0].type).toBe('p2pFetch');
    bridge.resolveFetch(requests[0].requestId, undefined, 'discovery is incomplete');
    await failure;

    const miss = route.read(request);
    void miss.catch(() => {});
    bridge.resolveFetch(requests[1].requestId);
    await expect(miss).resolves.toEqual({ type: 'no-result' });
  });

  it('still rejects a corrupt aggregate response through the central hash verifier', async () => {
    vi.useFakeTimers();
    const { bridge, router, requests } = aggregate();
    const result = router.read(createBlobRequest(await sha256(new Uint8Array([8]))));
    const failure = expect(result).rejects.toThrow();
    await vi.advanceTimersByTimeAsync(0);
    expect(requests[0].type).toBe('p2pFetch');
    bridge.resolveFetch(requests[0].requestId, new Uint8Array([9]));
    await failure;
  });

  it('does not adopt a stale peer list after switching to aggregate mode', async () => {
    const { bridge, route, requests } = aggregate();
    route.setEnabled(true);
    const oldList = route.peerList();
    route.setEnabled(true, false);
    bridge.resolvePeerList(requests[0].requestId, ['stale-peer']);
    await expect(oldList).resolves.toEqual([]);
    await expect(route.peerList()).resolves.toEqual([]);
    expect(requests).toHaveLength(1);
  });
});
