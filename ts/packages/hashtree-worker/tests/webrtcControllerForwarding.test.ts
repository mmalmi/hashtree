import { describe, expect, it, vi } from 'vitest';
import { sha256, type Store } from '@hashtree/core';
import {
  FRAGMENT_SIZE,
  MSG_TYPE_RESPONSE,
  createRequest,
  createFragmentResponse,
  createResponse,
  encodeRequest,
  encodeResponse,
  parseMessage,
} from '@hashtree/nostr';
import { WebRTCController } from '../src/p2p/webrtcController.js';

interface ControllerPeer {
  peerId: string;
  dataChannelReady: boolean;
  state: 'connecting' | 'connected' | 'disconnected';
}

interface ControllerPrivateApi {
  createPeer: (
    peerId: string,
    pubkey: string,
    pool: 'follows' | 'other',
    direction: 'inbound' | 'outbound'
  ) => ControllerPeer;
  onDataChannelMessage: (peerId: string, data: Uint8Array) => Promise<void>;
}

function createForwardingController(localStore: Store): {
  controller: WebRTCController;
  internal: ControllerPrivateApi;
  sentData: Array<{ peerId: string; data: Uint8Array }>;
}
function createForwardingController(
  localStore: Store,
  options: {
    upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>;
    requestTimeout?: number;
    requestDispatch?: {
      initialFanout: number;
      hedgeFanout: number;
      maxFanout: number;
      hedgeIntervalMs: number;
    };
  } = {},
): {
  controller: WebRTCController;
  internal: ControllerPrivateApi;
  sentData: Array<{ peerId: string; data: Uint8Array }>;
} {
  const sentData: Array<{ peerId: string; data: Uint8Array }> = [];
  const controller = new WebRTCController({
    pubkey: 'self-pubkey',
    localStore,
    sendCommand: (cmd) => {
      if (cmd.type === 'rtc:sendData') {
        sentData.push({ peerId: cmd.peerId, data: cmd.data });
      }
    },
    sendSignaling: async () => {},
    requestTimeout: options.requestTimeout ?? 100,
    upstreamFetch: options.upstreamFetch,
    requestDispatch: options.requestDispatch,
  });

  const internal = controller as unknown as ControllerPrivateApi;
  return { controller, internal, sentData };
}

function connectPeer(internal: ControllerPrivateApi, peerId: string, pubkey: string): ControllerPeer {
  const peer = internal.createPeer(peerId, pubkey, 'other', 'outbound');
  peer.state = 'connected';
  peer.dataChannelReady = true;
  return peer;
}

function countResponseMessages(
  sentData: Array<{ peerId: string; data: Uint8Array }>,
  peerId: string,
): number {
  return sentData.filter((entry) => entry.peerId === peerId)
    .map((entry) => parseMessage(entry.data))
    .filter((message) => message?.type === MSG_TYPE_RESPONSE)
    .length;
}

describe('WebRTCController forwarding behavior', () => {
  it('suppresses duplicate forwarded queries while a hash lookup is in flight', async () => {
    const localStore: Store = {
      put: async () => true,
      get: async () => null,
      has: async () => false,
      delete: async () => false,
    };
    const { controller, internal, sentData } = createForwardingController(localStore);
    const requester = connectPeer(internal, 'peer-requester', 'requester-pubkey');
    const upstream = connectPeer(internal, 'peer-upstream', 'upstream-pubkey');

    const hash = new Uint8Array(32).fill(5);
    const requestBytes = new Uint8Array(encodeRequest(createRequest(hash, 3)));

    await internal.onDataChannelMessage(requester.peerId, requestBytes);
    await internal.onDataChannelMessage(requester.peerId, requestBytes);

    const forwardedToUpstream = sentData.filter((entry) => entry.peerId === upstream.peerId);
    expect(forwardedToUpstream).toHaveLength(1);

    const requesterStats = controller.getPeerStats().find((stats) => stats.peerId === requester.peerId);
    expect(requesterStats?.forwardedRequests).toBe(1);
    expect(requesterStats?.forwardedSuppressed).toBe(1);
  });

  it('counts forwarded-resolved queries when an upstream response is returned', async () => {
    const localStore: Store = {
      put: async () => true,
      get: async () => null,
      has: async () => false,
      delete: async () => false,
    };
    const { controller, internal, sentData } = createForwardingController(localStore);
    const requester = connectPeer(internal, 'peer-requester', 'requester-pubkey');
    const upstream = connectPeer(internal, 'peer-upstream', 'upstream-pubkey');

    const payload = new Uint8Array([10, 20, 30, 40]);
    const hash = await sha256(payload);
    const requestBytes = new Uint8Array(encodeRequest(createRequest(hash, 4)));
    await internal.onDataChannelMessage(requester.peerId, requestBytes);

    const responseBytes = new Uint8Array(encodeResponse(createResponse(hash, payload)));
    await internal.onDataChannelMessage(upstream.peerId, responseBytes);

    const responsesToRequester = sentData.filter((entry) => entry.peerId === requester.peerId);
    expect(responsesToRequester.length).toBeGreaterThan(0);

    const requesterStats = controller.getPeerStats().find((stats) => stats.peerId === requester.peerId);
    expect(requesterStats?.forwardedResolved).toBe(1);
  });

  it('does not re-forward a hash when the same query loops back from another peer', async () => {
    const localStore: Store = {
      put: async () => true,
      get: async () => null,
      has: async () => false,
      delete: async () => false,
    };
    const { controller, internal, sentData } = createForwardingController(localStore);
    const requester = connectPeer(internal, 'peer-requester', 'requester-pubkey');
    const neighborA = connectPeer(internal, 'peer-neighbor-a', 'neighbor-a-pubkey');
    const neighborB = connectPeer(internal, 'peer-neighbor-b', 'neighbor-b-pubkey');

    const hash = new Uint8Array(32).fill(9);
    const requestBytes = new Uint8Array(encodeRequest(createRequest(hash, 4)));

    await internal.onDataChannelMessage(requester.peerId, requestBytes);
    const forwardsAfterFirstRequest = sentData.length;

    // Simulate the same hash request bouncing back from another peer in the mesh.
    await internal.onDataChannelMessage(neighborA.peerId, requestBytes);

    // No new forwards should be sent while the hash is already in-flight.
    expect(sentData.length).toBe(forwardsAfterFirstRequest);

    const neighborAStats = controller.getPeerStats().find((stats) => stats.peerId === neighborA.peerId);
    expect(neighborAStats?.forwardedSuppressed).toBe(1);

    // Sanity check that we originally forwarded to at least one other neighbor.
    const forwardedPeerIds = new Set(sentData.map((entry) => entry.peerId));
    expect(forwardedPeerIds.has(neighborB.peerId)).toBe(true);
  });

  it('resolves multiple requesters without re-forwarding the same hash query', async () => {
    const localStore: Store = {
      put: async () => true,
      get: async () => null,
      has: async () => false,
      delete: async () => false,
    };
    const { controller, internal, sentData } = createForwardingController(localStore);
    const requesterA = connectPeer(internal, 'peer-requester-a', 'requester-a-pubkey');
    const requesterB = connectPeer(internal, 'peer-requester-b', 'requester-b-pubkey');
    const upstream = connectPeer(internal, 'peer-upstream', 'upstream-pubkey');

    const payload = new Uint8Array([1, 4, 9, 16, 25]);
    const hash = await sha256(payload);
    const requestBytes = new Uint8Array(encodeRequest(createRequest(hash, 4)));

    await internal.onDataChannelMessage(requesterA.peerId, requestBytes);
    await internal.onDataChannelMessage(requesterB.peerId, requestBytes);

    const forwardsToUpstream = sentData.filter((entry) => entry.peerId === upstream.peerId).length;
    expect(forwardsToUpstream).toBe(1);

    const responseBytes = new Uint8Array(encodeResponse(createResponse(hash, payload)));
    await internal.onDataChannelMessage(upstream.peerId, responseBytes);

    const responsesToA = sentData.filter((entry) => entry.peerId === requesterA.peerId).length;
    const responsesToB = sentData.filter((entry) => entry.peerId === requesterB.peerId).length;
    expect(responsesToA).toBeGreaterThan(0);
    expect(responsesToB).toBeGreaterThan(0);

    const statsA = controller.getPeerStats().find((stats) => stats.peerId === requesterA.peerId);
    const statsB = controller.getPeerStats().find((stats) => stats.peerId === requesterB.peerId);
    expect(statsA?.forwardedRequests).toBe(1);
    expect(statsA?.forwardedResolved).toBe(1);
    expect(statsB?.forwardedSuppressed).toBe(1);
    expect(statsB?.forwardedResolved).toBe(1);
  });

  it('serves a peer miss from the upstream fetch path and caches it locally', async () => {
    const localStore: Store = {
      put: vi.fn().mockResolvedValue(true),
      get: vi.fn().mockResolvedValue(null),
      has: async () => false,
      delete: async () => false,
    };
    const payload = new Uint8Array([42, 43, 44, 45]);
    const hash = await sha256(payload);
    const upstreamFetch = vi.fn().mockResolvedValue(payload);
    const { internal, sentData } = createForwardingController(localStore, { upstreamFetch });
    const requester = connectPeer(internal, 'peer-requester', 'requester-pubkey');

    const requestBytes = new Uint8Array(encodeRequest(createRequest(hash, 3)));
    await internal.onDataChannelMessage(requester.peerId, requestBytes);
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(upstreamFetch).toHaveBeenCalledTimes(1);
    expect(localStore.put).toHaveBeenCalledWith(hash, payload);
    expect(countResponseMessages(sentData, requester.peerId)).toBe(1);
  });

  it('deduplicates upstream fetches across multiple requesters for the same hash', async () => {
    const localStore: Store = {
      put: vi.fn().mockResolvedValue(true),
      get: vi.fn().mockResolvedValue(null),
      has: async () => false,
      delete: async () => false,
    };
    const payload = new Uint8Array([7, 8, 9, 10]);
    const hash = await sha256(payload);
    let resolveUpstream: ((value: Uint8Array | null) => void) | null = null;
    const upstreamFetch = vi.fn().mockImplementation(
      () => new Promise<Uint8Array | null>((resolve) => {
        resolveUpstream = resolve;
      }),
    );

    const { internal, sentData } = createForwardingController(localStore, { upstreamFetch });
    const requesterA = connectPeer(internal, 'peer-requester-a', 'requester-a-pubkey');
    const requesterB = connectPeer(internal, 'peer-requester-b', 'requester-b-pubkey');
    const requestBytes = new Uint8Array(encodeRequest(createRequest(hash, 4)));

    await internal.onDataChannelMessage(requesterA.peerId, requestBytes);
    await internal.onDataChannelMessage(requesterB.peerId, requestBytes);

    expect(upstreamFetch).toHaveBeenCalledTimes(1);

    resolveUpstream?.(payload);
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(countResponseMessages(sentData, requesterA.peerId)).toBe(1);
    expect(countResponseMessages(sentData, requesterB.peerId)).toBe(1);
  });

  it('keeps large fragmented responses alive for the full request timeout window', async () => {
    vi.useFakeTimers();
    const localStore: Store = {
      put: vi.fn().mockResolvedValue(true),
      get: vi.fn().mockResolvedValue(null),
      has: async () => false,
      delete: async () => false,
    };
    const { controller, internal } = createForwardingController(localStore, {
      requestTimeout: 15_000,
    });
    const upstream = connectPeer(internal, 'peer-upstream', 'upstream-pubkey');

    const payload = new Uint8Array(FRAGMENT_SIZE * 3 + 1024);
    payload.fill(7);
    const hash = await sha256(payload);

    const pending = controller.get(hash);
    await vi.advanceTimersByTimeAsync(0);

    const totalFragments = Math.ceil(payload.length / FRAGMENT_SIZE);
    for (let index = 0; index < totalFragments; index += 1) {
      const start = index * FRAGMENT_SIZE;
      const end = Math.min(start + FRAGMENT_SIZE, payload.length);
      const fragment = payload.slice(start, end);
      const responseBytes = new Uint8Array(encodeResponse(createFragmentResponse(hash, fragment, index, totalFragments)));
      await internal.onDataChannelMessage(upstream.peerId, responseBytes);
      if (index < totalFragments - 1) {
        await vi.advanceTimersByTimeAsync(6_000);
      }
    }

    await expect(pending).resolves.toEqual(payload);
  });

  it('uses the same staged peer scheduler for forwarded requests instead of flooding every peer', async () => {
    vi.useFakeTimers();
    const localStore: Store = {
      put: async () => true,
      get: async () => null,
      has: async () => false,
      delete: async () => false,
    };
    const { internal, sentData } = createForwardingController(localStore, {
      requestDispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 100,
      },
    });
    const requesterA = connectPeer(internal, 'peer-requester-a', 'requester-a-pubkey');
    const requesterB = connectPeer(internal, 'peer-requester-b', 'requester-b-pubkey');
    connectPeer(internal, 'peer-upstream-a', 'upstream-a-pubkey');
    connectPeer(internal, 'peer-upstream-b', 'upstream-b-pubkey');

    const hashA = await sha256(new TextEncoder().encode('forward-a'));
    const hashB = await sha256(new TextEncoder().encode('forward-b'));

    await internal.onDataChannelMessage(
      requesterA.peerId,
      new Uint8Array(encodeRequest(createRequest(hashA, 4))),
    );
    await vi.advanceTimersByTimeAsync(0);

    expect(sentData).toHaveLength(1);
    const firstForwardPeer = sentData[0]?.peerId;
    expect(firstForwardPeer).toBeDefined();

    await internal.onDataChannelMessage(
      requesterB.peerId,
      new Uint8Array(encodeRequest(createRequest(hashB, 4))),
    );
    await vi.advanceTimersByTimeAsync(0);

    expect(sentData).toHaveLength(2);
    expect(sentData[1]?.peerId).not.toBe(firstForwardPeer);
  });
});
