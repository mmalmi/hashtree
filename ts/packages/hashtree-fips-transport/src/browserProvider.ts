import { fromHex, type Store } from '@hashtree/core';
import {
  FipsNode,
  identityFromSecretKey,
  type FipsIdentity,
  type Logger,
} from '@fips/core';
import { WebRtcTransport } from '@fips/transport-webrtc';
import type { FipsNodeLike } from './index.js';
import { DEFAULT_FIPS_DISCOVERY_APP } from './constants.js';
import {
  createFipsWorkerP2PProvider,
  type HashtreeWorkerP2PProvider,
} from './workerProvider.js';

const DEFAULT_STUN_SERVERS = [
  'stun:stun.l.google.com:19302',
  'stun:stun.cloudflare.com:3478',
];

interface BrowserHashtreeFipsProviderBaseOptions {
  relays: readonly string[];
  localStore: Store;
  discoveryApp?: string;
  stunServers?: readonly string[];
  maxConnections?: number;
  connectTimeoutMs?: number;
  relayConnectTimeoutMs?: number;
  iceGatherTimeoutMs?: number;
  requestTimeoutMs?: number;
  forwarding?: boolean;
  logger?: Logger;
}

export type BrowserHashtreeFipsProviderOptions = BrowserHashtreeFipsProviderBaseOptions & (
  | { identity: FipsIdentity; deviceSecretKey?: never }
  | { identity?: never; deviceSecretKey: Uint8Array | string }
);

export interface BrowserHashtreeFipsProvider extends HashtreeWorkerP2PProvider {
  readonly node: FipsNode;
  readonly webRtcTransport: WebRtcTransport;
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
    logger: options.logger,
  });
  const node = new FipsNode({
    identity,
    transports: [webRtcTransport],
    forwarding: options.forwarding ?? true,
    routingMode: 'reply_learned',
    logger: options.logger,
  });
  const provider = createFipsWorkerP2PProvider({
    node: node as unknown as FipsNodeLike & FipsNode,
    localStore: options.localStore,
    requestTimeoutMs: options.requestTimeoutMs,
  });

  let stopped = false;
  const stop = async (): Promise<void> => {
    if (stopped) return;
    stopped = true;
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
    fetch: (hashHex, peerId) => provider.fetch(hashHex, peerId),
    listPeerIds: () => provider.listPeerIds(),
    node,
    webRtcTransport,
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
