import { BLOB_NO_RESULT, } from '@hashtree/core';
import { BlobRouter } from '@hashtree/mesh';
const DEFAULT_PEER_LIST_CACHE_MS = 1_500;
const MAX_P2P_PEERS = 32;
const P2P_ROUTE_TIMEOUT_MS = 20_000;
/** One composite route over the exact identities advertised by its configured provider. */
export class P2PPeerRoutes {
    bridge;
    cacheMs;
    id = 'p2p';
    peerIds = [];
    peerRoutes = new Map();
    router;
    refreshedAt = 0;
    inflight = null;
    generation = 0;
    constructor(bridge, cacheMs = DEFAULT_PEER_LIST_CACHE_MS) {
        this.bridge = bridge;
        this.cacheMs = cacheMs;
        this.router = new BlobRouter([], {
            id: 'p2p-peers',
            requestTimeoutMs: P2P_ROUTE_TIMEOUT_MS,
            maxRoutes: MAX_P2P_PEERS,
            maxRouteAttempts: MAX_P2P_PEERS,
        });
    }
    isAvailable = () => this.bridge.isEnabled();
    setEnabled(enabled) {
        this.generation += 1;
        this.peerIds = [];
        this.peerRoutes.clear();
        this.router.setRoutes([]);
        this.refreshedAt = 0;
        this.inflight = null;
        this.bridge.setEnabled(enabled);
    }
    async read(request, context) {
        if (context?.signal?.aborted)
            throw new Error('P2P blob request was cancelled');
        const peerIds = await this.peerList();
        if (context?.signal?.aborted)
            throw new Error('P2P blob request was cancelled');
        if (peerIds.length === 0)
            return BLOB_NO_RESULT;
        return this.router.read(request, context);
    }
    async peerList() {
        if (!this.bridge.isEnabled())
            return [];
        if (Date.now() - this.refreshedAt < this.cacheMs)
            return [...this.peerIds];
        const generation = this.generation;
        const pending = this.inflight ?? this.bridge.listPeers();
        this.inflight = pending;
        try {
            const peerIds = await pending;
            if (generation !== this.generation || !this.bridge.isEnabled())
                return [];
            this.peerIds = [...new Set(peerIds.filter(Boolean))].sort().slice(0, MAX_P2P_PEERS);
            this.syncPeerRoutes();
            this.refreshedAt = Date.now();
            return [...this.peerIds];
        }
        finally {
            if (this.inflight === pending)
                this.inflight = null;
        }
    }
    syncPeerRoutes() {
        const next = new Map();
        for (const peerId of this.peerIds) {
            const route = this.peerRoutes.get(peerId) ?? {
                id: peerId,
                groupId: this.id,
                isAvailable: () => this.bridge.isEnabled() && this.peerIds.includes(peerId),
                read: (request, context) => (this.bridge.fetch(request, peerId, context?.signal)),
            };
            next.set(peerId, route);
        }
        this.peerRoutes = next;
        this.router.setRoutes([...next.values()]);
    }
}
//# sourceMappingURL=p2pPeerRoutes.js.map