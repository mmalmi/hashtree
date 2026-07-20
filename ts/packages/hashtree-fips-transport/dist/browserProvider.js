import { fromHex } from '@hashtree/core';
import { FipsNode, identityFromSecretKey, } from '@fips/core';
import { WebRtcTransport } from '@fips/transport-webrtc';
import { WebSocketTransport } from '@fips/transport-websocket';
import { DEFAULT_FIPS_DISCOVERY_APP, DEFAULT_FIPS_WEBSOCKET_SEED_URLS, } from './constants.js';
import { createFipsWorkerP2PProvider, } from './workerProvider.js';
const DEFAULT_STUN_SERVERS = [
    'stun:stun.l.google.com:19302',
    'stun:stun.cloudflare.com:3478',
];
export function supportsBrowserHashtreeFips() {
    return typeof WebSocket !== 'undefined' && typeof RTCPeerConnection !== 'undefined';
}
/**
 * Starts a browser FIPS node whose links and routes are discovered over Nostr,
 * then exposes Hashtree's worker-provider surface over authenticated FIPS data.
 */
export async function createBrowserHashtreeFipsProvider(options) {
    if (!supportsBrowserHashtreeFips()) {
        throw new Error('browser FIPS requires WebSocket and RTCPeerConnection');
    }
    const relays = normalizeRelayUrls(options.relays);
    if (relays.length === 0) {
        throw new Error('browser FIPS requires at least one Nostr relay');
    }
    const identity = options.identity ?? await identityFromSecretKey(readSecretKey(options.deviceSecretKey));
    const websocketSeedUrls = normalizeWebSocketSeedUrls(options.websocketSeedUrls ?? DEFAULT_FIPS_WEBSOCKET_SEED_URLS);
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
        logger: options.logger,
    });
    const node = new FipsNode({
        identity,
        transports: [webSocketTransport, webRtcTransport],
        forwarding: options.forwarding ?? true,
        routingMode: 'reply_learned',
        logger: options.logger,
    });
    const provider = createFipsWorkerP2PProvider({
        node,
        localStore: options.localStore,
        requestTimeoutMs: options.requestTimeoutMs,
        providerRoutes: options.providerRoutes,
    });
    let stopped = false;
    const stop = async () => {
        if (stopped)
            return;
        stopped = true;
        provider.close();
        await node.stop();
    };
    try {
        await node.start();
    }
    catch (error) {
        await stop().catch(() => undefined);
        throw error;
    }
    return {
        fetch: (hashHex, peerId, htl) => provider.fetch(hashHex, peerId, htl),
        listPeerIds: () => provider.listPeerIds(),
        node,
        webRtcTransport,
        webSocketTransport,
        stop,
    };
}
function readSecretKey(secret) {
    const bytes = typeof secret === 'string' ? fromHex(secret) : new Uint8Array(secret);
    if (bytes.byteLength !== 32) {
        throw new Error(`FIPS device secret must be 32 bytes, got ${bytes.byteLength}`);
    }
    return bytes;
}
function normalizeRelayUrls(relays) {
    const normalized = new Set();
    for (const relay of relays) {
        const value = relay.trim().replace(/\/+$/, '');
        if (value)
            normalized.add(value);
    }
    return [...normalized];
}
function normalizeWebSocketSeedUrls(seedUrls) {
    const normalized = new Set();
    for (const seedUrl of seedUrls) {
        const value = seedUrl.trim().replace(/\/+$/, '');
        if (value)
            normalized.add(value);
    }
    return [...normalized];
}
//# sourceMappingURL=browserProvider.js.map