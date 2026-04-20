import { afterEach, describe, expect, it, vi } from 'vitest';
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
  static instances: FakeRTCPeerConnection[] = [];

  connectionState: RTCPeerConnectionState = 'connected';
  onicecandidate: ((this: RTCPeerConnection, ev: RTCPeerConnectionIceEvent) => any) | null = null;
  ondatachannel: ((this: RTCPeerConnection, ev: RTCDataChannelEvent) => any) | null = null;
  onconnectionstatechange: ((this: RTCPeerConnection, ev: Event) => any) | null = null;
  readonly dataChannel = new FakeDataChannel();

  constructor() {
    FakeRTCPeerConnection.instances.push(this);
  }

  createDataChannel(): RTCDataChannel {
    return this.dataChannel as unknown as RTCDataChannel;
  }

  async createOffer(): Promise<RTCSessionDescriptionInit> {
    return { type: 'offer', sdp: 'fake-offer-sdp' };
  }

  async setLocalDescription(): Promise<void> {}

  close(): void {}
}

afterEach(() => {
  FakeRTCPeerConnection.instances = [];
  vi.unstubAllGlobals();
});

describe('WebRTCProxy', () => {
  it('prioritizes request frames ahead of queued response traffic', () => {
    vi.stubGlobal('RTCPeerConnection', FakeRTCPeerConnection as unknown as typeof RTCPeerConnection);

    const proxy = new WebRTCProxy(() => undefined);
    proxy.handleCommand({ type: 'rtc:createPeer', peerId: 'peer-1', pubkey: 'pubkey-1' });
    proxy.handleCommand({ type: 'rtc:createOffer', peerId: 'peer-1' });

    const connection = FakeRTCPeerConnection.instances[0];
    expect(connection).toBeDefined();

    const channel = connection.dataChannel;
    channel.bufferedAmount = 300_000;

    proxy.handleCommand({ type: 'rtc:sendData', peerId: 'peer-1', data: new Uint8Array([0x01, 0xaa]) });
    proxy.handleCommand({ type: 'rtc:sendData', peerId: 'peer-1', data: new Uint8Array([0x00, 0xbb]) });

    expect(channel.sent).toHaveLength(0);

    channel.bufferedAmount = 0;
    channel.onbufferedamountlow?.call(channel as unknown as RTCDataChannel, new Event('bufferedamountlow'));

    expect(channel.sent.map((frame) => frame[0])).toEqual([0x00, 0x01]);
  });

  it('gives the first upload slot to peers with better seeding ratio', () => {
    vi.stubGlobal('RTCPeerConnection', FakeRTCPeerConnection as unknown as typeof RTCPeerConnection);

    const proxy = new WebRTCProxy(() => undefined, { maxUploadBytesPerSecond: 250 });
    proxy.handleCommand({ type: 'rtc:createPeer', peerId: 'leecher', pubkey: 'leecher-pubkey' });
    proxy.handleCommand({ type: 'rtc:createOffer', peerId: 'leecher' });
    proxy.handleCommand({ type: 'rtc:createPeer', peerId: 'seeder', pubkey: 'seeder-pubkey' });
    proxy.handleCommand({ type: 'rtc:createOffer', peerId: 'seeder' });

    const leecher = FakeRTCPeerConnection.instances[0]?.dataChannel;
    const seeder = FakeRTCPeerConnection.instances[1]?.dataChannel;
    expect(leecher).toBeDefined();
    expect(seeder).toBeDefined();

    leecher!.bufferedAmount = 300_000;
    seeder!.bufferedAmount = 300_000;

    // Prime the reciprocity score so `seeder` has already uploaded useful bytes to us.
    seeder!.emitMessage(new Uint8Array(220).fill(0x21));

    proxy.handleCommand({
      type: 'rtc:sendData',
      peerId: 'leecher',
      data: new Uint8Array(180).fill(0x01),
    });
    proxy.handleCommand({
      type: 'rtc:sendData',
      peerId: 'seeder',
      data: new Uint8Array(180).fill(0x01),
    });

    expect(leecher!.sent).toHaveLength(0);
    expect(seeder!.sent).toHaveLength(0);

    leecher!.bufferedAmount = 0;
    seeder!.bufferedAmount = 0;

    // Even if the leecher buffer drains first, the seeded peer should get the slot.
    leecher!.onbufferedamountlow?.call(
      leecher as unknown as RTCDataChannel,
      new Event('bufferedamountlow'),
    );

    expect(seeder!.sent).toHaveLength(1);
    expect(leecher!.sent).toHaveLength(0);
  });
});
