import { MemoryStore } from '@hashtree/core';
import type { FipsIdentity } from '@fips/core';
import { afterEach, describe, expect, it, vi } from 'vitest';

const fake = vi.hoisted(() => ({
  webRtcOptions: undefined as Record<string, unknown> | undefined,
  providerOptions: undefined as Record<string, unknown> | undefined,
  webSocketOptions: [] as Record<string, unknown>[],
}));

vi.mock('@fips/core', () => ({
  FipsNode: class {
    on(): () => void { return () => {}; }
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
  WebSocketTransport: class {
    constructor(options: Record<string, unknown>) { fake.webSocketOptions.push(options); }
  },
}));

vi.mock('../src/workerProvider.js', () => ({
  createFipsWorkerP2PProvider: (options: Record<string, unknown>) => {
    fake.providerOptions = options;
    return {
    close: vi.fn(),
    discoverProviders: vi.fn(),
    fetch: vi.fn(),
    listPeerIds: vi.fn(() => []),
    };
  },
}));

import { createBrowserHashtreeFipsProvider } from '../src/browserProvider.js';
import { DEFAULT_FIPS_WEBSOCKET_SEED_URLS } from '../src/constants.js';

describe('browser Hashtree FIPS provider', () => {
  afterEach(() => {
    fake.webRtcOptions = undefined;
    fake.providerOptions = undefined;
    fake.webSocketOptions = [];
    vi.unstubAllGlobals();
  });

  it('passes inbound peer admission to WebRTC before accepting an offer', async () => {
    vi.stubGlobal('WebSocket', class {});
    vi.stubGlobal('RTCPeerConnection', class {});
    const allowIncomingPeer = vi.fn((peerId: string) => peerId === 'allowed-peer');
    const serveBlob = vi.fn(async () => null);
    const getUploadLimitBytesPerSecond = () => 1024;

    const provider = await createBrowserHashtreeFipsProvider({
      relays: ['wss://relay.example'],
      localStore: new MemoryStore(),
      identity: identity(),
      allowIncomingPeer,
      serveBlob,
      getUploadLimitBytesPerSecond,
    });

    expect(fake.webRtcOptions?.allowIncomingPeer).toBe(allowIncomingPeer);
    expect(fake.providerOptions).toMatchObject({ allowIncomingPeer, serveBlob, getUploadLimitBytesPerSecond });
    expect(fake.webSocketOptions[0]?.seedUrls).toEqual(DEFAULT_FIPS_WEBSOCKET_SEED_URLS);
    expect(provider.webSocketTransport).toBeDefined();
    await provider.stop();
  });

  it('keeps WebRTC and the chosen relay when explicit seeds and STUN are empty', async () => {
    vi.stubGlobal('WebSocket', class {});
    vi.stubGlobal('RTCPeerConnection', class {});
    const provider = await createBrowserHashtreeFipsProvider({
      relays: [' ws://127.0.0.1:18123/ '], localStore: new MemoryStore(), identity: identity(),
      websocketSeedUrls: [], stunServers: [],
    });
    expect(fake.webSocketOptions).toEqual([]);
    expect(provider.webSocketTransport).toBeUndefined();
    expect(provider.webRtcTransport).toBeDefined();
    expect(fake.webRtcOptions).toMatchObject({ relays: ['ws://127.0.0.1:18123'], stunServers: [] });
    await provider.stop();
  });

  it('keeps explicit empty relays when an application supplies discovery', async () => {
    vi.stubGlobal('WebSocket', class {});
    vi.stubGlobal('RTCPeerConnection', class {});
    const discoveryProvider = { publish: vi.fn(), subscribe: vi.fn() };
    const provider = await createBrowserHashtreeFipsProvider({
      relays: [], discoveryProvider, localStore: new MemoryStore(), identity: identity(),
      websocketSeedUrls: [], stunServers: [],
    });
    expect(fake.webRtcOptions).toMatchObject({ relays: [], discoveryProvider, stunServers: [] });
    expect(fake.webSocketOptions).toEqual([]);
    await provider.stop();
  });

  it('preserves the no-relay/no-discovery error before starting any transport', async () => {
    vi.stubGlobal('WebSocket', class {});
    vi.stubGlobal('RTCPeerConnection', class {});
    await expect(createBrowserHashtreeFipsProvider({
      relays: [], localStore: new MemoryStore(), identity: identity(), websocketSeedUrls: [],
    })).rejects.toThrow('at least one Nostr relay');
    expect(fake.webSocketOptions).toEqual([]);
    expect(fake.webRtcOptions).toBeUndefined();
  });

  it('preserves the blank-only seed error and rejects malformed relay configuration before transports', async () => {
    vi.stubGlobal('WebSocket', class {});
    vi.stubGlobal('RTCPeerConnection', class {});
    const options = { relays: ['wss://relay.example'], localStore: new MemoryStore(), identity: identity() };
    await expect(createBrowserHashtreeFipsProvider({ ...options, websocketSeedUrls: ['  '] }))
      .rejects.toThrow('at least one WebSocket bootstrap seed');
    for (const relays of [['not a url'], ['https://relay.example']]) {
      await expect(createBrowserHashtreeFipsProvider({ ...options, relays, websocketSeedUrls: [] }))
        .rejects.toThrow();
    }
    expect(fake.webSocketOptions).toEqual([]);
    expect(fake.webRtcOptions).toBeUndefined();
  });
});

function identity(): FipsIdentity {
  return {
    secretKey: new Uint8Array(32).fill(1),
    publicKey: new Uint8Array(33).fill(2),
    xOnlyPubkey: new Uint8Array(32).fill(3),
  };
}
