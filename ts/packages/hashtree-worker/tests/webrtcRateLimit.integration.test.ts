import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { sha256, toHex, type Store } from '@hashtree/core';
import {
  MSG_TYPE_REQUEST,
  MSG_TYPE_RESPONSE,
  createRequest,
  encodeRequest,
  parseMessage,
} from '@hashtree/nostr';
import { WebRTCController } from '../src/p2p/webrtcController.js';
import { WebRTCProxy } from '../src/p2p/webrtcProxy.js';

class FakeDataChannel {
  readyState: RTCDataChannelState = 'open';
  binaryType: BinaryType = 'arraybuffer';
  bufferedAmount = 0;
  bufferedAmountLowThreshold = 0;
  onopen: ((this: RTCDataChannel, ev: Event) => any) | null = null;
  onclose: ((this: RTCDataChannel, ev: Event) => any) | null = null;
  onerror: ((this: RTCDataChannel, ev: Event) => any) | null = null;
  onmessage: ((this: RTCDataChannel, ev: MessageEvent) => any) | null = null;
  onbufferedamountlow: ((this: RTCDataChannel, ev: Event) => any) | null = null;
  readonly sent: Uint8Array[] = [];

  send(data: ArrayBufferLike): void {
    this.sent.push(new Uint8Array(data.slice(0)));
  }

  emitMessage(data: Uint8Array): void {
    const payload = data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength);
    this.onmessage?.call(this as unknown as RTCDataChannel, {
      data: payload,
    } as MessageEvent);
  }

  close(): void {}
}

class FakeRTCPeerConnection {
  connectionState: RTCPeerConnectionState = 'connected';
  onicecandidate: ((this: RTCPeerConnection, ev: RTCPeerConnectionIceEvent) => any) | null = null;
  ondatachannel: ((this: RTCPeerConnection, ev: RTCDataChannelEvent) => any) | null = null;
  onconnectionstatechange: ((this: RTCPeerConnection, ev: Event) => any) | null = null;
  readonly dataChannel = new FakeDataChannel();

  createDataChannel(): RTCDataChannel {
    return this.dataChannel as unknown as RTCDataChannel;
  }

  async createOffer(): Promise<RTCSessionDescriptionInit> {
    return { type: 'offer', sdp: 'fake-offer-sdp' };
  }

  async setLocalDescription(): Promise<void> {}

  close(): void {}
}

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
}

interface ProxyPeer {
  dataChannel: FakeDataChannel | null;
}

interface ProxyPrivateApi {
  peers: Map<string, ProxyPeer>;
}

type FrameType = typeof MSG_TYPE_REQUEST | typeof MSG_TYPE_RESPONSE;

function createStore(entries: Array<{ hash: Uint8Array; data: Uint8Array }>): Store {
  const dataByHash = new Map(entries.map((entry) => [toHex(entry.hash), entry.data]));

  return {
    put: async (hash, data) => {
      dataByHash.set(toHex(hash), data);
      return true;
    },
    get: async (hash) => dataByHash.get(toHex(hash)) ?? null,
    has: async (hash) => dataByHash.has(toHex(hash)),
    delete: async (hash) => dataByHash.delete(toHex(hash)),
  };
}

function getFrames(channel: FakeDataChannel, type: FrameType): Uint8Array[] {
  return channel.sent.filter((frame) => parseMessage(frame)?.type === type);
}

async function flushAsyncWork(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

function createIntegratedHarness(options: {
  localStore: Store;
  maxUploadBytesPerSecond?: number | null;
  forwardRateLimit?: {
    maxForwardsPerPeerWindow?: number;
    windowMs?: number;
  };
  requestTimeout?: number;
}): {
  controller: WebRTCController;
  addPeer: (peerId: string) => FakeDataChannel;
} {
  vi.stubGlobal('RTCPeerConnection', FakeRTCPeerConnection as unknown as typeof RTCPeerConnection);

  let controller!: WebRTCController;
  const proxy = new WebRTCProxy(
    (event) => controller.handleProxyEvent(event),
    { maxUploadBytesPerSecond: options.maxUploadBytesPerSecond },
  );

  controller = new WebRTCController({
    pubkey: 'self-pubkey',
    localStore: options.localStore,
    sendCommand: (cmd) => {
      proxy.handleCommand(cmd);
    },
    sendSignaling: async () => {},
    requestTimeout: options.requestTimeout ?? 100,
    forwardRateLimit: options.forwardRateLimit,
  });

  const controllerPrivate = controller as unknown as ControllerPrivateApi;
  const proxyPrivate = proxy as unknown as ProxyPrivateApi;

  return {
    controller,
    addPeer: (peerId: string) => {
      const peer = controllerPrivate.createPeer(peerId, `${peerId}-pubkey`, 'other', 'outbound');
      peer.state = 'connected';

      const channel = proxyPrivate.peers.get(peerId)?.dataChannel;
      if (!channel) {
        throw new Error(`Proxy channel missing for ${peerId}`);
      }

      return channel;
    },
  };
}

describe('WebRTC rate limiting integration', () => {
  beforeEach(() => {
    vi.useFakeTimers({
      toFake: ['Date', 'setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'performance'],
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('limits forwarded mesh requests per requester through the controller and proxy path', async () => {
    const harness = createIntegratedHarness({
      localStore: createStore([]),
      forwardRateLimit: {
        maxForwardsPerPeerWindow: 1,
        windowMs: 1000,
      },
      requestTimeout: 25,
    });

    const requester = harness.addPeer('peer-requester');
    const upstream = harness.addPeer('peer-upstream');

    const hashA = await sha256(new TextEncoder().encode('forward-a'));
    const hashB = await sha256(new TextEncoder().encode('forward-b'));
    const hashC = await sha256(new TextEncoder().encode('forward-c'));

    requester.emitMessage(new Uint8Array(encodeRequest(createRequest(hashA, 3))));
    await flushAsyncWork();
    expect(getFrames(upstream, MSG_TYPE_REQUEST)).toHaveLength(1);

    requester.emitMessage(new Uint8Array(encodeRequest(createRequest(hashB, 3))));
    await flushAsyncWork();
    expect(getFrames(upstream, MSG_TYPE_REQUEST)).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(1001);
    requester.emitMessage(new Uint8Array(encodeRequest(createRequest(hashC, 3))));
    await flushAsyncWork();

    expect(getFrames(upstream, MSG_TYPE_REQUEST)).toHaveLength(2);

    const requesterStats = harness.controller
      .getPeerStats()
      .find((stats) => stats.peerId === 'peer-requester');
    expect(requesterStats?.forwardedRequests).toBe(2);
  });

  it('throttles queued responses through the proxy upload limiter and resumes after the delay', async () => {
    const payloadA = new Uint8Array(256).fill(0x11);
    const payloadB = new Uint8Array(256).fill(0x22);
    const hashA = await sha256(payloadA);
    const hashB = await sha256(payloadB);
    const harness = createIntegratedHarness({
      localStore: createStore([
        { hash: hashA, data: payloadA },
        { hash: hashB, data: payloadB },
      ]),
      maxUploadBytesPerSecond: 350,
      requestTimeout: 25,
    });

    const requester = harness.addPeer('peer-requester');

    requester.emitMessage(new Uint8Array(encodeRequest(createRequest(hashA, 3))));
    requester.emitMessage(new Uint8Array(encodeRequest(createRequest(hashB, 3))));
    await flushAsyncWork();

    expect(getFrames(requester, MSG_TYPE_RESPONSE)).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(300);
    await flushAsyncWork();
    expect(getFrames(requester, MSG_TYPE_RESPONSE)).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(1000);
    await flushAsyncWork();
    expect(getFrames(requester, MSG_TYPE_RESPONSE)).toHaveLength(2);
  });
});
