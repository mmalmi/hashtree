const DEFAULT_PEER_LIST_CACHE_MS = 1_500;
/** One composite route whose provider owns authenticated peer selection. */
export class P2PPeerRoutes {
    bridge;
    cacheMs;
    id = 'p2p';
    peerIds = [];
    refreshedAt = 0;
    inflight = null;
    generation = 0;
    constructor(bridge, cacheMs = DEFAULT_PEER_LIST_CACHE_MS) {
        this.bridge = bridge;
        this.cacheMs = cacheMs;
    }
    isAvailable = () => this.bridge.isEnabled();
    setEnabled(enabled) {
        this.generation += 1;
        this.peerIds = [];
        this.refreshedAt = 0;
        this.inflight = null;
        this.bridge.setEnabled(enabled);
    }
    read(request, context) {
        return this.bridge.fetch(request, undefined, context?.signal);
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
            this.peerIds = [...new Set(peerIds.filter(Boolean))].sort();
            this.refreshedAt = Date.now();
            return [...this.peerIds];
        }
        finally {
            if (this.inflight === pending)
                this.inflight = null;
        }
    }
}
//# sourceMappingURL=p2pPeerRoutes.js.map