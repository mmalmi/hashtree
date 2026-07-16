import { describe, expect, it, vi } from 'vitest';
import { createBlobRequest, fromHex, sha256 } from '@hashtree/core';
import { P2PBridge, type P2PBridgeRequest } from '../src/p2pBridge.js';
import { P2PPeerRoutes } from '../src/p2pPeerRoutes.js';

function createRoutes() {
  const requests: P2PBridgeRequest[] = [];
  const bridge = new P2PBridge({
    respond: (request) => requests.push(request),
    peerListTimeoutMs: 1_000,
  });
  const routes = new P2PPeerRoutes(bridge);
  routes.setEnabled(true);
  return { bridge, requests, routes };
}

describe('P2PPeerRoutes', () => {
  it('reports only exact, unique provider identities', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.peerList();
    const list = requests[0];
    expect(list).toMatchObject({ type: 'p2pPeerList' });
    bridge.resolvePeerList(list.requestId, ['peer-b', '', 'peer-a', 'peer-a']);

    await expect(pending).resolves.toEqual(['peer-a', 'peer-b']);
  });

  it('routes through the exact configured provider identity and preserves HTL', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pendingFetch = routes.read(createBlobRequest(fromHex('ab'.repeat(32))));
    const list = requests[0];
    expect(list).toMatchObject({ type: 'p2pPeerList' });
    bridge.resolvePeerList(list.requestId, ['fips-peer']);

    await vi.waitFor(() => expect(requests).toHaveLength(2));
    const fetch = requests[1];
    expect(fetch).toMatchObject({
      type: 'p2pFetch',
      htl: 10,
      peerId: 'fips-peer',
    });
    bridge.resolveFetch(fetch.requestId);
    await expect(pendingFetch).resolves.toEqual({ type: 'no-result' });
  });

  it('keeps an enabled composite route even when its provider list is empty', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.peerList();
    bridge.resolvePeerList(requests[0].requestId, []);
    await expect(pending).resolves.toEqual([]);
    expect(routes.isAvailable()).toBe(true);
  });

  it('isolates corrupt peers and returns the first centrally verified reply', async () => {
    const { bridge, requests, routes } = createRoutes();
    const data = new Uint8Array([1, 2, 3, 4]);
    const pendingFetch = routes.read(createBlobRequest(await sha256(data)));
    bridge.resolvePeerList(requests[0].requestId, ['peer-a', 'peer-b']);

    await vi.waitFor(() => expect(requests).toHaveLength(2));
    expect(requests[1]).toMatchObject({ type: 'p2pFetch', peerId: 'peer-a', htl: 10 });
    bridge.resolveFetch(requests[1].requestId, new Uint8Array([9]));

    await vi.waitFor(() => expect(requests).toHaveLength(3));
    expect(requests[2]).toMatchObject({ type: 'p2pFetch', peerId: 'peer-b', htl: 10 });
    bridge.resolveFetch(requests[2].requestId, data);

    await expect(pendingFetch).resolves.toEqual({ type: 'data', data });
  });

  it('reports provider-list failure without inventing peer routes', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.peerList();
    bridge.resolvePeerList(requests[0].requestId, undefined, 'provider unavailable');
    await expect(pending).rejects.toThrow('provider unavailable');
  });
});
