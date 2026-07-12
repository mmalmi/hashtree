export class ExternalP2PBridge {
    respond;
    fetchTimeoutMs;
    peerListTimeoutMs;
    pendingFetches = new Map();
    pendingPeerLists = new Map();
    requestCounter = 0;
    enabled = false;
    constructor(options) {
        this.respond = options.respond;
        this.fetchTimeoutMs = options.fetchTimeoutMs;
        this.peerListTimeoutMs = options.peerListTimeoutMs;
    }
    setEnabled(enabled) {
        this.enabled = enabled;
        if (!enabled)
            this.clear();
    }
    isEnabled() {
        return this.enabled;
    }
    fetch(hashHex, peerId) {
        if (!this.enabled)
            return Promise.resolve(null);
        const requestId = this.nextRequestId('p2p');
        return new Promise((resolve) => {
            const timeout = setTimeout(() => {
                this.pendingFetches.delete(requestId);
                resolve(null);
            }, this.fetchTimeoutMs);
            this.pendingFetches.set(requestId, { resolve, timeout });
            this.respond({ type: 'p2pFetch', requestId, hashHex, peerId });
        });
    }
    listPeers() {
        if (!this.enabled)
            return Promise.resolve([]);
        const requestId = this.nextRequestId('p2p_peers');
        return new Promise((resolve) => {
            const timeout = setTimeout(() => {
                this.pendingPeerLists.delete(requestId);
                resolve([]);
            }, this.peerListTimeoutMs);
            this.pendingPeerLists.set(requestId, { resolve, timeout });
            this.respond({ type: 'p2pPeerList', requestId });
        });
    }
    resolveFetch(requestId, data, error) {
        const pending = this.pendingFetches.get(requestId);
        if (!pending)
            return;
        this.pendingFetches.delete(requestId);
        clearTimeout(pending.timeout);
        pending.resolve(error ? null : data ?? null);
    }
    resolvePeerList(requestId, peerIds, error) {
        const pending = this.pendingPeerLists.get(requestId);
        if (!pending)
            return;
        this.pendingPeerLists.delete(requestId);
        clearTimeout(pending.timeout);
        pending.resolve(error ? [] : [...new Set(peerIds ?? [])]);
    }
    clear() {
        for (const pending of this.pendingFetches.values()) {
            clearTimeout(pending.timeout);
            pending.resolve(null);
        }
        this.pendingFetches.clear();
        for (const pending of this.pendingPeerLists.values()) {
            clearTimeout(pending.timeout);
            pending.resolve([]);
        }
        this.pendingPeerLists.clear();
    }
    nextRequestId(prefix) {
        this.requestCounter += 1;
        return `${prefix}_${Date.now()}_${this.requestCounter}`;
    }
}
//# sourceMappingURL=externalP2P.js.map