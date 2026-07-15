import { fromHex } from '@hashtree/core';
import { TCP_BLOB_DEFAULT_HTL, TCP_BLOB_MAX_HTL, TCP_BLOB_SERVICE_PORT, TcpBlobTransport, } from './tcpBlobTransport.js';
export const HASHTREE_BLOB_CAPABILITY = 'hashtree.blob/1';
/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Peer selection remains dynamic: the provider reads authenticated FIPS links
 * for every request instead of maintaining a second discovery mesh.
 */
export class FipsWorkerP2PProvider {
    options;
    transport;
    closed = false;
    constructor(options) {
        this.options = options;
        this.transport = new TcpBlobTransport({
            endpoint: options.node,
            localStore: options.localStore,
            timeoutMs: options.requestTimeoutMs,
        });
    }
    fetch(hashHex, peerId) {
        if (this.closed)
            return Promise.reject(new Error('FIPS worker P2P provider is closed'));
        const hash = parseHash(hashHex);
        return this.fetchHash(hash, peerId);
    }
    async fetchHash(hash, peerId) {
        const routes = await this.routes();
        if (peerId) {
            const known = routes.find((route) => route.peerId === peerId);
            return this.transport.get(hash, [peerId], known?.htl ?? TCP_BLOB_DEFAULT_HTL);
        }
        return this.fetchRoutes(hash, routes);
    }
    async listPeerIds() {
        if (this.closed)
            return [];
        return (await this.routes()).map((route) => route.peerId);
    }
    close() {
        if (this.closed)
            return;
        this.closed = true;
        void this.transport.close();
    }
    async routes() {
        const source = this.options.providerRoutes;
        if (!source)
            return [];
        return normalizeRoutes(typeof source === 'function' ? await source() : source);
    }
    async fetchRoutes(hash, routes) {
        const groups = new Map();
        for (const route of routes) {
            const peers = groups.get(route.htl) ?? [];
            peers.push(route.peerId);
            groups.set(route.htl, peers);
        }
        if (groups.size === 0)
            return this.transport.get(hash, []);
        const attempts = [...groups].map(async ([htl, peers]) => {
            try {
                return { kind: 'result', data: await this.transport.get(hash, peers, htl) };
            }
            catch (error) {
                return { kind: 'failed', error };
            }
        });
        const pending = new Map(attempts.map((attempt, index) => [
            index,
            attempt.then((result) => [index, result]),
        ]));
        const failures = [];
        let misses = 0;
        while (pending.size > 0) {
            const [index, result] = await Promise.race(pending.values());
            pending.delete(index);
            if (result.kind === 'result' && result.data)
                return result.data;
            if (result.kind === 'result')
                misses += 1;
            else
                failures.push(result.error);
        }
        if (failures.length === 0 && misses === groups.size)
            return null;
        throw new AggregateError(failures, 'TCP/FIPS blob availability is uncertain');
    }
}
export function createFipsWorkerP2PProvider(options) {
    return new FipsWorkerP2PProvider(options);
}
/** Convert an authenticated FSP local-instance roster into local-only blob routes. */
export function blobRoutesFromCapabilityRoster(advertisements) {
    return normalizeRoutes(advertisements.flatMap((advertisement) => {
        const capability = advertisement.capabilities
            .filter(({ name, fspPort }) => (name === HASHTREE_BLOB_CAPABILITY && fspPort === TCP_BLOB_SERVICE_PORT))
            .sort((left, right) => (right.priority ?? 0) - (left.priority ?? 0))[0];
        return capability
            ? [{ peerId: advertisement.peerId, htl: 0, priority: capability.priority ?? 0 }]
            : [];
    }));
}
function normalizeRoutes(routes) {
    const normalized = routes.map((route) => {
        const peerId = route.peerId.trim();
        const priority = route.priority ?? 0;
        if (!peerId)
            throw new Error('TCP/FIPS blob provider identity is empty');
        if (!Number.isInteger(route.htl) || route.htl < 0 || route.htl > TCP_BLOB_MAX_HTL) {
            throw new Error('TCP/FIPS blob HTL is invalid');
        }
        if (!Number.isInteger(priority) || priority < -0x8000 || priority > 0x7fff) {
            throw new Error('TCP/FIPS blob provider priority is invalid');
        }
        return { peerId, htl: route.htl, priority };
    }).sort((left, right) => (right.priority - left.priority
        || left.peerId.localeCompare(right.peerId)
        || left.htl - right.htl));
    const peers = new Set();
    return normalized.filter(({ peerId }) => {
        if (peers.has(peerId))
            return false;
        peers.add(peerId);
        return true;
    });
}
function parseHash(hashHex) {
    const normalized = hashHex.trim();
    if (!/^[0-9a-f]{64}$/i.test(normalized)) {
        throw new Error('Hashtree block hash must be 32-byte hex');
    }
    return fromHex(normalized);
}
//# sourceMappingURL=workerProvider.js.map