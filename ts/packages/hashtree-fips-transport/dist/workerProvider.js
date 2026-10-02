import { fromHex } from '@hashtree/core';
import { TCP_BLOB_DEFAULT_HTL, TCP_BLOB_MAX_HTL, TCP_BLOB_SERVICE_PORT, TcpBlobTransport, } from './tcpBlobTransport.js';
export const HASHTREE_BLOB_CAPABILITY = 'hashtree.blob/1';
/**
 * Bridges a running FIPS node into HashtreeWorkerClient.setP2PProvider().
 * Provider selection uses supplied routes or a successful blob-service probe;
 * ordinary FIPS connections alone never qualify a peer as a file provider.
 */
export class FipsWorkerP2PProvider {
    options;
    transport;
    closed = false;
    probed = new Map();
    discovery = null;
    constructor(options) {
        this.options = options;
        this.transport = new TcpBlobTransport({
            endpoint: options.node,
            localStore: options.localStore,
            serveBlob: options.serveBlob,
            getUploadLimitBytesPerSecond: options.getUploadLimitBytesPerSecond,
            timeoutMs: options.requestTimeoutMs,
            allowIncomingPeer: options.allowIncomingPeer,
        });
    }
    fetch(hashHex, peerId, htl) {
        if (this.closed)
            return Promise.reject(new Error('FIPS worker P2P provider is closed'));
        if (htl !== undefined && (!Number.isInteger(htl) || htl < 0 || htl > TCP_BLOB_MAX_HTL)) {
            throw new Error('TCP/FIPS blob HTL is invalid');
        }
        const hash = parseHash(hashHex);
        return this.fetchHash(hash, peerId, htl);
    }
    async fetchHash(hash, peerId, requestedHtl) {
        const routes = await this.routes();
        if (peerId) {
            const known = routes.find((route) => route.peerId === peerId);
            return this.transport.get(hash, [peerId], effectiveHtl(known, requestedHtl));
        }
        return this.fetchRoutes(hash, routes, requestedHtl);
    }
    async listPeerIds() {
        if (this.closed)
            return [];
        return (await this.routes(false)).map((route) => route.peerId);
    }
    close() {
        if (this.closed)
            return;
        this.closed = true;
        void this.transport.close();
    }
    async routes(waitForDiscovery = true) {
        const source = this.options.providerRoutes;
        const explicit = source ? (typeof source === 'function' ? await source() : source) : [];
        // Listing already admitted routes must not wait for unrelated service probes.
        // Fetches still await discovery so their fallback set is not incomplete.
        if (waitForDiscovery || explicit.length === 0)
            await this.discoverProviders();
        // A service probe adds providers; it must not replace an explicit route policy.
        const configured = new Set(explicit.map(({ peerId }) => peerId.trim()));
        const discovered = [...this.probed].filter(([peerId, status]) => status.available && !configured.has(peerId))
            .map(([peerId]) => ({ peerId, htl: 0 }));
        return normalizeRoutes([...explicit, ...discovered]);
    }
    /** Bounded service negotiation; ordinary FIPS peers are never assumed to serve files. */
    async discoverProviders() {
        if (this.closed || !this.options.candidatePeerIds)
            return;
        if (this.discovery)
            return this.discovery;
        this.discovery = (async () => {
            // Re-read admission after every batch: another peer can connect while a seed
            // probe is in flight. Bound total work and concurrent streams per pass.
            for (let probes = 0; probes < 16 && !this.closed;) {
                const candidates = [...new Set(this.options.candidatePeerIds())].slice(0, 16);
                for (const peer of this.probed.keys())
                    if (!candidates.includes(peer))
                        this.probed.delete(peer);
                const pending = candidates.filter((peer) => (this.probed.get(peer)?.retryAt ?? 0) <= Date.now()).slice(0, 2);
                if (pending.length === 0)
                    break;
                probes += pending.length;
                await Promise.all(pending.map(async (peer) => {
                    const available = await this.transport.probe(peer);
                    if (!this.closed)
                        this.probed.set(peer, { available, retryAt: Date.now() + (available ? 60_000 : 10_000) });
                }));
            }
        })().finally(() => { this.discovery = null; });
        return this.discovery;
    }
    async fetchRoutes(hash, routes, requestedHtl) {
        const groups = new Map();
        for (const route of routes) {
            const htl = effectiveHtl(route, requestedHtl);
            const peers = groups.get(htl) ?? [];
            peers.push(route.peerId);
            groups.set(htl, peers);
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
function effectiveHtl(route, requestedHtl) {
    // An authenticated same-host route is terminal and must remain local-only.
    if (route?.htl === 0)
        return 0;
    return requestedHtl ?? route?.htl ?? TCP_BLOB_DEFAULT_HTL;
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