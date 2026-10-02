import { FipsNostrPubsubClient, FipsNostrPubsubEventSource, type FipsNostrPubsubClientOptions, type RuntimeSource } from 'nostr-pubsub';
import { fromHex, toHex, type Hash, type Store } from '@hashtree/core';
import {
  FipsNode,
  identityFromSecretKey,
  type FipsIdentity,
  type Logger,
} from '@fips/core';
import { WebRtcTransport, type WebRtcTransportConfig } from '@fips/transport-webrtc';
import { WebSocketTransport } from '@fips/transport-websocket';
import {
  DEFAULT_FIPS_DISCOVERY_APP,
  DEFAULT_FIPS_WEBSOCKET_SEED_URLS,
} from './constants.js';
import {
  createFipsWorkerP2PProvider,
  type FipsBlobRouteSource,
  type HashtreeWorkerP2PProvider,
} from './workerProvider.js';

const DEFAULT_STUN_SERVERS = [
  'stun:stun.l.google.com:19302',
  'stun:stun.cloudflare.com:3478',
];

interface BrowserHashtreeFipsProviderBaseOptions {
  relays: readonly string[];
  localStore: Store;
  /** Serve authorized incoming requests without changing outgoing local-cache reads. */
  /** Inbound only. HTL 0 permits local content only; do not forward upstream. */
  serveBlob?: (hash: Hash, peerId: string, signal: AbortSignal, htl: number) => Promise<Uint8Array | null>;
  discoveryApp?: string;
  stunServers?: readonly string[];
  /** Explicit authenticated first-adjacency seeds used to bootstrap WebRTC negotiation. */
  websocketSeedUrls?: readonly string[];
  maxConnections?: number;
  connectTimeoutMs?: number;
  relayConnectTimeoutMs?: number;
  iceGatherTimeoutMs?: number;
  requestTimeoutMs?: number;
  getUploadLimitBytesPerSecond?: () => number | null;
  /** Authenticated capability routes or explicitly configured remote Hashtree peers. */
  providerRoutes?: FipsBlobRouteSource;
  /** Reject unsolicited WebRTC offers before creating a peer connection or FIPS link. */
  allowIncomingPeer?: WebRtcTransportConfig['allowIncomingPeer'];
  forwarding?: boolean;
  logger?: Logger;
}

export type BrowserHashtreeFipsProviderOptions = BrowserHashtreeFipsProviderBaseOptions & (
  | { identity: FipsIdentity; deviceSecretKey?: never }
  | { identity?: never; deviceSecretKey: Uint8Array | string }
);

export interface BrowserHashtreeFipsProvider extends HashtreeWorkerP2PProvider {
  readonly node: FipsNode;
  readonly localPeerId: string;
  listConnectedPeerIds(): string[];
  readonly webRtcTransport: WebRtcTransport;
  readonly webSocketTransport: WebSocketTransport;
  stop(): Promise<void>;
}

export function supportsBrowserHashtreeFips(): boolean {
  return typeof WebSocket !== 'undefined' && typeof RTCPeerConnection !== 'undefined';
}

/**
 * Starts a browser FIPS node whose links and routes are discovered over Nostr,
 * then exposes Hashtree's worker-provider surface over authenticated FIPS data.
 */
export async function createBrowserHashtreeFipsProvider(
  options: BrowserHashtreeFipsProviderOptions,
): Promise<BrowserHashtreeFipsProvider> {
  if (!supportsBrowserHashtreeFips()) {
    throw new Error('browser FIPS requires WebSocket and RTCPeerConnection');
  }
  const relays = normalizeRelayUrls(options.relays);
  if (relays.length === 0) {
    throw new Error('browser FIPS requires at least one Nostr relay');
  }
  const identity = options.identity ?? await identityFromSecretKey(readSecretKey(options.deviceSecretKey));
  const websocketSeedUrls = normalizeWebSocketSeedUrls(
    options.websocketSeedUrls ?? DEFAULT_FIPS_WEBSOCKET_SEED_URLS,
  );
  if (websocketSeedUrls.length === 0) {
    throw new Error('browser FIPS requires at least one WebSocket bootstrap seed');
  }
  const webSocketTransport = new WebSocketTransport({
    seedUrls: websocketSeedUrls,
    logger: options.logger,
  });
  const webRtcTransport = new WebRtcTransport({
    relays,
    stunServers: [...(options.stunServers ?? DEFAULT_STUN_SERVERS)],
    advertiseOnNostr: true,
    acceptConnections: true,
    autoConnect: true,
    discoveryApp: options.discoveryApp ?? DEFAULT_FIPS_DISCOVERY_APP,
    maxConnections: options.maxConnections ?? 8,
    connectTimeoutMs: options.connectTimeoutMs ?? 30_000,
    relayConnectTimeoutMs: options.relayConnectTimeoutMs ?? 8_000,
    iceGatherTimeoutMs: options.iceGatherTimeoutMs ?? 10_000,
    allowIncomingPeer: options.allowIncomingPeer,
    logger: options.logger,
  });
  const node = new FipsNode({
    identity,
    transports: [webSocketTransport, webRtcTransport],
    forwarding: options.forwarding ?? true,
    routingMode: 'reply_learned',
    logger: options.logger,
  });
  const adjacentPeers = new Set<string>();
  const provider = createFipsWorkerP2PProvider({
    node,
    localStore: options.localStore,
    serveBlob: options.serveBlob,
    getUploadLimitBytesPerSecond: options.getUploadLimitBytesPerSecond,
    requestTimeoutMs: options.requestTimeoutMs,
    providerRoutes: options.providerRoutes,
    allowIncomingPeer: options.allowIncomingPeer,
    candidatePeerIds: () => [...adjacentPeers],
  });
  const removePeerListener = node.on('peer', (data) => {
    const peer = data as { remotePubkey?: string; state?: string };
    if (!peer.remotePubkey) return;
    if (peer.state === 'connected') adjacentPeers.add(peer.remotePubkey);
    else if (peer.state === 'disconnected') adjacentPeers.delete(peer.remotePubkey);
    void provider.discoverProviders();
  });

  let stopped = false;
  const stop = async (): Promise<void> => {
    if (stopped) return;
    stopped = true;
    removePeerListener();
    adjacentPeers.clear();
    provider.close();
    await node.stop();
  };
  try {
    await node.start();
  } catch (error) {
    await stop().catch(() => undefined);
    throw error;
  }
  return {
    fetch: (hashHex, peerId, htl) => provider.fetch(hashHex, peerId, htl),
    listPeerIds: () => provider.listPeerIds(),
    node,
    localPeerId: toHex(identity.publicKey),
    listConnectedPeerIds: () => [...adjacentPeers],
    webRtcTransport,
    webSocketTransport,
    stop,
  };
}

function readSecretKey(secret: Uint8Array | string): Uint8Array {
  const bytes = typeof secret === 'string' ? fromHex(secret) : new Uint8Array(secret);
  if (bytes.byteLength !== 32) {
    throw new Error(`FIPS device secret must be 32 bytes, got ${bytes.byteLength}`);
  }
  return bytes;
}

function normalizeRelayUrls(relays: readonly string[]): string[] {
  const normalized = new Set<string>();
  for (const relay of relays) {
    const value = relay.trim().replace(/\/+$/, '');
    if (value) normalized.add(value);
  }
  return [...normalized];
}

function normalizeWebSocketSeedUrls(seedUrls: readonly string[]): string[] {
  const normalized = new Set<string>();
  for (const seedUrl of seedUrls) {
    const value = seedUrl.trim().replace(/\/+$/, '');
    if (value) normalized.add(value);
  }
  return [...normalized];
}

export type BrowserHashtreeNostrProviderOptions = BrowserHashtreeFipsProviderOptions & Pick<
  FipsNostrPubsubClientOptions, 'retainedEventReader' | 'allowedKinds' | 'limits'
> & {
  onNostrError?: FipsNostrPubsubClientOptions['onError'];
  /** Restrict event interests and retained history to application-admitted identities. */
  nostrPeers?: () => readonly string[];
};

export interface BrowserHashtreeNostrProvider extends BrowserHashtreeFipsProvider {
  readonly nostrSource: RuntimeSource;
  refreshNostrPeers(): void;
}

/** Files and events share one authenticated FIPS node, identity and set of links. */
export async function createBrowserHashtreeNostrProvider(
  options: BrowserHashtreeNostrProviderOptions,
): Promise<BrowserHashtreeNostrProvider> {
  const provider = await createBrowserHashtreeFipsProvider(options);
  let client: FipsNostrPubsubClient;
  try {
    client = new FipsNostrPubsubClient({
      node: provider.node,
      localPeerId: provider.localPeerId,
      peers: options.nostrPeers ?? (() => provider.listConnectedPeerIds()),
      retainedEventReader: options.retainedEventReader,
      allowedKinds: options.allowedKinds,
      limits: { maxFiltersPerSubscription: 32, ...options.limits },
      onError: options.onNostrError,
    }).start();
  } catch (error) {
    await provider.stop();
    throw error;
  }
  let stopped = false;
  return {
    ...provider,
    nostrSource: Object.assign(new FipsNostrPubsubEventSource(client), { id: 'fips' }),
    refreshNostrPeers: () => client.refreshPeers(),
    stop: async () => {
      if (stopped) return;
      stopped = true;
      try { await client.stop(); } finally { await provider.stop(); }
    },
  };
}
