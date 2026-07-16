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
  it('reports only exact, unique provider identities', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.peerList();
    const list = requests[0];
    expect(list).toMatchObject({ type: 'p2pPeerList' });
    bridge.resolvePeerList(list.requestId, ['peer-b', '', 'peer-a', 'peer-a']);

    await expect(pending).resolves.toEqual(['peer-a', 'peer-b']);
  });

  it('leaves provider selection inside one composite route and preserves HTL', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pendingFetch = routes.read(createBlobRequest(fromHex('ab'.repeat(32))));
    const fetch = requests[0];
    expect(fetch).toMatchObject({
      type: 'p2pFetch',
      htl: 10,
    });
    expect(fetch).not.toHaveProperty('peerId');
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

  it('reports provider-list failure without inventing peer routes', async () => {
    const { bridge, requests, routes } = createRoutes();
    const pending = routes.peerList();
    bridge.resolvePeerList(requests[0].requestId, undefined, 'provider unavailable');
    await expect(pending).rejects.toThrow('provider unavailable');
  });
});
