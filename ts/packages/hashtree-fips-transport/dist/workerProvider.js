import { fromHex } from '@hashtree/core';
import { TcpBlobTransport } from './tcpBlobTransport.js';
/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Peer selection remains dynamic: the provider reads authenticated FIPS links
 * for every request instead of maintaining a second discovery mesh.
 */
export class FipsWorkerP2PProvider {
    transport;
    node;
    peers = new Set();
    unsubscribePeer;
    closed = false;
    constructor(options) {
        this.node = options.node;
        this.unsubscribePeer = this.node.on('peer', (value) => {
            const event = value;
            if (event.state === 'connected')
                this.peers.add(event.remotePubkey);
            else
                this.peers.delete(event.remotePubkey);
        });
        this.transport = new TcpBlobTransport({
            endpoint: options.node,
            localStore: options.localStore,
            timeoutMs: options.requestTimeoutMs,
        });
    }
    fetch(hashHex, peerId) {
        if (this.closed)
            return Promise.resolve(null);
        const hash = parseHash(hashHex);
        return this.transport.get(hash, peerId ? [peerId] : [...this.peers]);
    }
    async listPeerIds() {
        if (this.closed)
            return [];
        return [...this.peers];
    }
    close() {
        if (this.closed)
            return;
        this.closed = true;
        this.unsubscribePeer();
        void this.transport.close();
    }
}
export function createFipsWorkerP2PProvider(options) {
    return new FipsWorkerP2PProvider(options);
}
function parseHash(hashHex) {
    const normalized = hashHex.trim();
    if (!/^[0-9a-f]{64}$/i.test(normalized)) {
        throw new Error('Hashtree block hash must be 32-byte hex');
    }
    return fromHex(normalized);
}
//# sourceMappingURL=workerProvider.js.map