import { MemoryStore } from '@hashtree/core';
import type { FipsIdentity } from '@fips/core';
import { afterEach, describe, expect, it, vi } from 'vitest';

const fake = vi.hoisted(() => ({
  webRtcOptions: undefined as Record<string, unknown> | undefined,
}));

vi.mock('@fips/core', () => ({
  FipsNode: class {
    async start(): Promise<void> {}
    async stop(): Promise<void> {}
  },
  identityFromSecretKey: vi.fn(),
}));

vi.mock('@fips/transport-webrtc', () => ({
  WebRtcTransport: class {
    constructor(options: Record<string, unknown>) {
      fake.webRtcOptions = options;
    }
  },
}));

vi.mock('@fips/transport-websocket', () => ({
  WebSocketTransport: class {},
}));

vi.mock('../src/workerProvider.js', () => ({
  createFipsWorkerP2PProvider: () => ({
    close: vi.fn(),
    fetch: vi.fn(),
    listPeerIds: vi.fn(() => []),
  }),
}));

import { createBrowserHashtreeFipsProvider } from '../src/browserProvider.js';

describe('browser Hashtree FIPS provider', () => {
  afterEach(() => {
    fake.webRtcOptions = undefined;
    vi.unstubAllGlobals();
  });

  it('passes inbound peer admission to WebRTC before accepting an offer', async () => {
    vi.stubGlobal('WebSocket', class {});
    vi.stubGlobal('RTCPeerConnection', class {});
    const allowIncomingPeer = vi.fn((peerId: string) => peerId === 'allowed-peer');

    const provider = await createBrowserHashtreeFipsProvider({
      relays: ['wss://relay.example'],
      localStore: new MemoryStore(),
      identity: identity(),
      allowIncomingPeer,
    });

    expect(fake.webRtcOptions?.allowIncomingPeer).toBe(allowIncomingPeer);
    await provider.stop();
  });
});

function identity(): FipsIdentity {
  return {
    secretKey: new Uint8Array(32).fill(1),
    publicKey: new Uint8Array(33).fill(2),
    xOnlyPubkey: new Uint8Array(32).fill(3),
  };
}
