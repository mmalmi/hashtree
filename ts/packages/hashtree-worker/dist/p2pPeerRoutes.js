const DEFAULT_PEER_LIST_CACHE_MS = 1_500;
/** Turns authenticated provider identities into explicit, independently verified blob routes. */
export class P2PPeerRoutes {
    bridge;
    cacheMs;
    peerIds = [];
    listError = null;
    refreshedAt = 0;
    inflight = null;
    generation = 0;
    constructor(bridge, cacheMs = DEFAULT_PEER_LIST_CACHE_MS) {
        this.bridge = bridge;
        this.cacheMs = cacheMs;
    }
    setEnabled(enabled) {
        this.generation += 1;
        this.peerIds = [];
        this.listError = null;
        this.refreshedAt = 0;
        this.inflight = null;
        this.bridge.setEnabled(enabled);
    }
    async sources() {
        if (!this.bridge.isEnabled())
            return [];
        await this.refresh();
        if (this.listError) {
            const error = this.listError;
            return [{
                    id: 'p2p:provider-list',
                    groupId: 'p2p',
                    read: async () => { throw error; },
                }];
        }
        return this.peerIds.map((peerId) => ({
            id: `peer:${peerId}`,
            groupId: 'p2p',
            read: (request, signal) => (this.bridge.fetch(request, peerId, signal)),
        }));
    }
    async peerList() {
        if (!this.bridge.isEnabled())
            return [];
        await this.refresh();
        if (this.listError)
            throw this.listError;
        return [...this.peerIds];
    }
    async refresh() {
        if (Date.now() - this.refreshedAt < this.cacheMs)
            return;
        const generation = this.generation;
        const pending = this.inflight ?? this.bridge.listPeers();
        this.inflight = pending;
        try {
            const peerIds = await pending;
            if (generation !== this.generation || !this.bridge.isEnabled())
                return;
            this.peerIds = [...new Set(peerIds.filter((peerId) => peerId.length > 0))].sort();
            this.listError = null;
            this.refreshedAt = Date.now();
        }
        catch (error) {
            if (generation !== this.generation)
                return;
            this.peerIds = [];
            this.listError = error instanceof Error ? error : new Error(String(error));
            this.refreshedAt = Date.now();
        }
        finally {
            if (this.inflight === pending)
                this.inflight = null;
        }
    }
}
//# sourceMappingURL=p2pPeerRoutes.js.map