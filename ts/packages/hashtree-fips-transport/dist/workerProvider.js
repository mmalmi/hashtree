import { fromHex } from '@hashtree/core';
import { HashtreeFipsTransport, createFipsNodeEndpoint, } from './index.js';
/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Peer selection remains dynamic: the provider reads authenticated FIPS links
 * for every request instead of maintaining a second discovery mesh.
 */
export class FipsWorkerP2PProvider {
    endpoint;
    transport;
    closed = false;
    constructor(options) {
        const { node, ...transportOptions } = options;
        this.endpoint = createFipsNodeEndpoint(node);
        this.transport = new HashtreeFipsTransport({
            ...transportOptions,
            endpoint: this.endpoint,
            peers: () => this.endpoint.listPeerIds?.() ?? [],
        });
    }
    fetch(hashHex, peerId) {
        if (this.closed)
            return Promise.resolve(null);
        const hash = parseHash(hashHex);
        return this.transport.get(hash, peerId ? [peerId] : undefined);
    }
    async listPeerIds() {
        if (this.closed)
            return [];
        return [...await Promise.resolve(this.endpoint.listPeerIds?.() ?? [])];
    }
    close() {
        if (this.closed)
            return;
        this.closed = true;
        this.transport.close();
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