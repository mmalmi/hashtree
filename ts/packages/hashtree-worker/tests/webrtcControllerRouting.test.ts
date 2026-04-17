import { afterEach, describe, expect, it, vi } from 'vitest';
import { MemoryStore, sha256 } from '@hashtree/core';
import { WebRTCController } from '../src/p2p/webrtcController.js';
import { createResponse, encodeResponse } from '@hashtree/nostr';

interface ControllerPeer {
  peerId: string;
  dataChannelReady: boolean;
  state: 'connecting' | 'connected' | 'disconnected';
  hashGet: boolean;
}

interface ControllerPrivateApi {
  createPeer: (
    peerId: string,
    pubkey: string,
    pool: 'follows' | 'other',
    direction: 'inbound' | 'outbound'
  ) => ControllerPeer;
  onPeerStateChange: (peerId: string, state: RTCPeerConnectionState) => void;
  onDataChannelMessage: (peerId: string, data: Uint8Array) => Promise<void>;
}

afterEach(() => {
  vi.useRealTimers();
});

function createRoutingController(options: {
  follows?: Set<string>;
  requestDispatch?: {
    initialFanout: number;
    hedgeFanout: number;
    maxFanout: number;
    hedgeIntervalMs: number;
  };
}) {
  const sentData: Array<{ peerId: string; data: Uint8Array }> = [];
  const commands: Array<{ type: string; peerId?: string }> = [];
  const hellos: Array<string> = [];
  const controller = new WebRTCController({
    pubkey: 'self-pubkey',
    localStore: new MemoryStore(),
    sendCommand: (cmd) => {
      commands.push({ type: cmd.type, peerId: 'peerId' in cmd ? cmd.peerId : undefined });
      if (cmd.type === 'rtc:sendData') {
        sentData.push({ peerId: cmd.peerId, data: cmd.data });
      }
    },
    sendSignaling: async (msg) => {
      if (msg.type === 'hello') {
        hellos.push(msg.peerId);
      }
    },
    getFollows: () => options.follows ?? new Set<string>(),
    requestTimeout: 120,
    requestDispatch: options.requestDispatch,
  });
  return { controller, internal: controller as unknown as ControllerPrivateApi, sentData, commands, hellos };
}

function connectPeer(
  internal: ControllerPrivateApi,
  peerId: string,
  pubkey: string,
  pool: 'follows' | 'other' = 'other'
): ControllerPeer {
  const peer = internal.createPeer(peerId, pubkey, pool, 'outbound');
  peer.state = 'connected';
  peer.dataChannelReady = true;
  return peer;
}

describe('WebRTCController routing', () => {
  it('sends staged hedged waves instead of flooding all peers immediately', async () => {
    vi.useFakeTimers();
    const { controller, internal, sentData } = createRoutingController({
      requestDispatch: {
        initialFanout: 2,
        hedgeFanout: 1,
        maxFanout: 4,
        hedgeIntervalMs: 50,
      },
    });

    connectPeer(internal, 'peer-a', 'pub-a');
    connectPeer(internal, 'peer-b', 'pub-b');
    connectPeer(internal, 'peer-c', 'pub-c');
    connectPeer(internal, 'peer-d', 'pub-d');

    const hash = await sha256(new TextEncoder().encode('route-me'));
    const pending = controller.get(hash);

    await vi.advanceTimersByTimeAsync(0);
    expect(sentData).toHaveLength(2);

    await vi.advanceTimersByTimeAsync(49);
    expect(sentData).toHaveLength(2);

    await vi.advanceTimersByTimeAsync(1);
    expect(sentData).toHaveLength(3);

    await vi.advanceTimersByTimeAsync(50);
    expect(sentData).toHaveLength(4);

    await vi.advanceTimersByTimeAsync(500);
    await expect(pending).resolves.toBeNull();
  });

  it('prioritizes follows pool peers first', async () => {
    vi.useFakeTimers();
    const follows = new Set<string>(['followed-pub']);
    const { controller, internal, sentData } = createRoutingController({
      follows,
      requestDispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 100,
      },
    });

    connectPeer(internal, 'peer-other', 'other-pub', 'other');
    const followed = connectPeer(internal, 'peer-followed', 'followed-pub', 'follows');

    const hash = await sha256(new TextEncoder().encode('prefer-follows'));
    const pending = controller.get(hash);

    await vi.advanceTimersByTimeAsync(0);
    expect(sentData[0]?.peerId).toBe(followed.peerId);

    await vi.advanceTimersByTimeAsync(500);
    await expect(pending).resolves.toBeNull();
  });

  it('skips peers that advertise hash_get as disabled', async () => {
    vi.useFakeTimers();
    const { controller, internal, sentData } = createRoutingController({
      requestDispatch: {
        initialFanout: 2,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 100,
      },
    });

    const assistPeer = connectPeer(internal, 'peer-assist', 'pub-assist');
    assistPeer.hashGet = false;
    connectPeer(internal, 'peer-capable', 'pub-capable');

    const hash = await sha256(new TextEncoder().encode('skip-assist'));
    const pending = controller.get(hash);

    await vi.advanceTimersByTimeAsync(0);
    expect(sentData).toHaveLength(1);
    expect(sentData[0]?.peerId).toBe('peer-capable');

    await vi.advanceTimersByTimeAsync(500);
    await expect(pending).resolves.toBeNull();
  });

  it('persists and reloads peer metadata snapshots', async () => {
    const localStore = new MemoryStore();

    const first = new WebRTCController({
      pubkey: 'self-pubkey',
      localStore,
      sendCommand: () => {},
      sendSignaling: async () => {},
    });
    const selector1 = (first as any).peerSelector;
    selector1.addPeer('fav-pub');
    selector1.recordRequest('fav-pub', 32);
    selector1.recordSuccess('fav-pub', 12, 1024);

    const hash = await first.persistPeerMetadata();
    expect(hash).not.toBeNull();

    const second = new WebRTCController({
      pubkey: 'self-pubkey',
      localStore,
      sendCommand: () => {},
      sendSignaling: async () => {},
    });

    const loaded = await second.loadPeerMetadata();
    expect(loaded).toBe(true);

    const selector2 = (second as any).peerSelector;
    selector2.addPeer('fav-pub');
    selector2.addPeer('other-pub');
    const ordered = selector2.selectPeers();
    expect(ordered[0]).toBe('fav-pub');
  });

  it('replaces stale disconnected peers when a fresh hello arrives', async () => {
    const { controller, internal, commands } = createRoutingController({});

    const peer = internal.createPeer('z-pub', 'z-pub', 'other', 'outbound');
    peer.state = 'disconnected';
    peer.dataChannelReady = false;

    await controller.handleSignalingMessage({
      type: 'hello',
      peerId: 'z-pub',
      hashGet: true,
    }, 'z-pub');

    expect(commands.some((command) => command.type === 'rtc:closePeer' && command.peerId === 'z-pub')).toBe(true);
    expect(commands.filter((command) => command.type === 'rtc:createPeer' && command.peerId === 'z-pub')).toHaveLength(2);
    expect(controller.getPeerStats()).toHaveLength(1);
  });

  it('drops disconnected peers and reannounces after a short grace period', async () => {
    vi.useFakeTimers();
    const { controller, internal, hellos } = createRoutingController({});

    const peer = internal.createPeer('z-pub', 'z-pub', 'other', 'outbound');
    peer.state = 'connected';
    peer.dataChannelReady = true;

    internal.onPeerStateChange('z-pub', 'disconnected');
    expect(controller.getPeerStats()).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(2_499);
    expect(controller.getPeerStats()).toHaveLength(1);
    expect(hellos).toHaveLength(0);

    await vi.advanceTimersByTimeAsync(1);
    expect(controller.getPeerStats()).toHaveLength(0);
    expect(hellos).toHaveLength(1);
  });

  it('promotes previously successful peers on subsequent lookups', async () => {
    vi.useFakeTimers();
    const { controller, internal, sentData } = createRoutingController({
      requestDispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 100,
      },
    });

    connectPeer(internal, 'peer-a', 'pub-a');
    connectPeer(internal, 'peer-b', 'pub-b');

    const payload = new TextEncoder().encode('winner');
    const hash1 = await sha256(payload);

    const firstGet = controller.get(hash1);
    await vi.advanceTimersByTimeAsync(0);
    const firstPeer = sentData[0]?.peerId;
    expect(firstPeer).toBeDefined();

    const response = new Uint8Array(encodeResponse(createResponse(hash1, payload)));
    await internal.onDataChannelMessage(firstPeer!, response);
    await expect(firstGet).resolves.toEqual(payload);

    sentData.length = 0;
    const hash2 = await sha256(new TextEncoder().encode('second-request'));
    const secondGet = controller.get(hash2);
    await vi.advanceTimersByTimeAsync(0);

    expect(sentData[0]?.peerId).toBe(firstPeer);

    await vi.advanceTimersByTimeAsync(500);
    await expect(secondGet).resolves.toBeNull();
  });

  it('spreads concurrent hash lookups across peers when the best peer is already busy', async () => {
    vi.useFakeTimers();
    const { controller, internal, sentData } = createRoutingController({
      requestDispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 100,
      },
    });

    connectPeer(internal, 'peer-a', 'pub-a');
    connectPeer(internal, 'peer-b', 'pub-b');

    const hashA = await sha256(new TextEncoder().encode('block-a'));
    const hashB = await sha256(new TextEncoder().encode('block-b'));

    const firstGet = controller.get(hashA);
    await vi.advanceTimersByTimeAsync(0);
    expect(sentData[0]?.peerId).toBe('peer-a');

    const secondGet = controller.get(hashB);
    await vi.advanceTimersByTimeAsync(0);
    expect(sentData[1]?.peerId).toBe('peer-b');

    await vi.advanceTimersByTimeAsync(500);
    await expect(firstGet).resolves.toBeNull();
    await expect(secondGet).resolves.toBeNull();
  });

  it('keeps the final hedged peer alive beyond the initial request window', async () => {
    vi.useFakeTimers();
    const { controller, internal, sentData } = createRoutingController({
      requestDispatch: {
        initialFanout: 1,
        hedgeFanout: 1,
        maxFanout: 2,
        hedgeIntervalMs: 50,
      },
    });

    connectPeer(internal, 'peer-a', 'pub-a');
    connectPeer(internal, 'peer-b', 'pub-b');

    const payload = new TextEncoder().encode('late-hedged-hit');
    const hash = await sha256(payload);

    const pending = controller.get(hash);
    await vi.advanceTimersByTimeAsync(0);
    expect(sentData[0]?.peerId).toBe('peer-a');

    await vi.advanceTimersByTimeAsync(50);
    expect(sentData[1]?.peerId).toBe('peer-b');

    await vi.advanceTimersByTimeAsync(99);
    const response = new Uint8Array(encodeResponse(createResponse(hash, payload)));
    await internal.onDataChannelMessage('peer-b', response);

    await expect(pending).resolves.toEqual(payload);
  });
});
