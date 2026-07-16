import { describe, expect, it } from 'vitest';
import { createBlobRequest, fromHex } from '@hashtree/core';
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
  it('creates only exact, unique provider routes', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.sources();
    const list = requests[0];
    expect(list).toMatchObject({ type: 'p2pPeerList' });
    bridge.resolvePeerList(list.requestId, ['peer-b', '', 'peer-a', 'peer-a']);

    await expect(pending).resolves.toEqual([
      expect.objectContaining({ id: 'peer:peer-a', groupId: 'p2p' }),
      expect.objectContaining({ id: 'peer:peer-b', groupId: 'p2p' }),
    ]);
  });

  it('passes the selected identity and native default HTL unchanged', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pendingSources = routes.sources();
    const list = requests[0];
    bridge.resolvePeerList(list.requestId, ['configured-peer']);
    const [route] = await pendingSources;

    const pendingFetch = route.read(createBlobRequest(fromHex('ab'.repeat(32))));
    const fetch = requests[1];
    expect(fetch).toMatchObject({
      type: 'p2pFetch',
      peerId: 'configured-peer',
      htl: 10,
    });
    bridge.resolveFetch(fetch.requestId);
    await expect(pendingFetch).resolves.toEqual({ type: 'no-result' });
  });

  it('creates no route when the provider exposes no identity', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.sources();
    bridge.resolvePeerList(requests[0].requestId, []);
    await expect(pending).resolves.toEqual([]);
    expect(requests).toHaveLength(1);
  });

  it('preserves provider-list failure as a route-local error', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.sources();
    bridge.resolvePeerList(requests[0].requestId, undefined, 'provider unavailable');
    const [route] = await pending;

    expect(route).toMatchObject({ id: 'p2p:provider-list', groupId: 'p2p' });
    await expect(route.read(createBlobRequest(fromHex('cd'.repeat(32)))))
      .rejects.toThrow('provider unavailable');
  });
});
