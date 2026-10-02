import { BLOB_NO_RESULT, blobData, toHex, } from '@hashtree/core';
export class P2PBridge {
    respond;
    fetchTimeoutMs;
    peerListTimeoutMs;
    fetches = new Map();
    peerLists = new Map();
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
            this.clear('P2P provider is not configured');
    }
    isEnabled() {
        return this.enabled;
    }
    fetch(request, peerId, signal) {
        if (!this.enabled)
            return Promise.reject(new Error('P2P provider is not configured'));
        const requestId = this.nextRequestId('p2p');
        const message = {
            type: 'p2pFetch',
            requestId,
            hashHex: toHex(request.hash),
            htl: request.htl,
        };
        if (peerId)
            message.peerId = peerId;
        return new Promise((resolve, reject) => {
            const pending = { resolve, reject, signal };
            if (signal) {
                pending.abort = () => this.rejectFetch(requestId, new Error('P2P blob request was cancelled'));
                signal.addEventListener('abort', pending.abort, { once: true });
            }
            if (this.fetchTimeoutMs && this.fetchTimeoutMs > 0) {
                pending.timeout = setTimeout(() => {
                    this.rejectFetch(requestId, new Error(`P2P blob request timed out after ${this.fetchTimeoutMs}ms`));
                }, this.fetchTimeoutMs);
            }
            this.fetches.set(requestId, pending);
            if (signal?.aborted) {
                pending.abort?.();
                return;
            }
            try {
                this.respond(message);
            }
            catch (error) {
                this.rejectFetch(requestId, error instanceof Error ? error : new Error(String(error)));
            }
        });
    }
    listPeers() {
        if (!this.enabled)
            return Promise.resolve([]);
        const requestId = this.nextRequestId('p2p_peers');
        return new Promise((resolve, reject) => {
            const pending = { resolve, reject };
            pending.timeout = setTimeout(() => {
                this.rejectPeerList(requestId, new Error('P2P peer list request timed out'));
            }, this.peerListTimeoutMs);
            this.peerLists.set(requestId, pending);
            try {
                this.respond({ type: 'p2pPeerList', requestId });
            }
            catch (error) {
                this.rejectPeerList(requestId, error instanceof Error ? error : new Error(String(error)));
            }
        });
    }
    resolveFetch(requestId, data, error) {
        const pending = this.take(this.fetches, requestId);
        if (!pending)
            return;
        if (error) {
            pending.reject(new Error(error));
            return;
        }
        pending.resolve(data === undefined ? BLOB_NO_RESULT : blobData(data));
    }
    resolvePeerList(requestId, peerIds, error) {
        const pending = this.take(this.peerLists, requestId);
        if (!pending)
            return;
        if (error) {
            pending.reject(new Error(error));
            return;
        }
        pending.resolve([...new Set(peerIds ?? [])]);
    }
    clear(message = 'P2P bridge was cleared') {
        for (const requestId of this.fetches.keys())
            this.rejectFetch(requestId, new Error(message));
        for (const requestId of this.peerLists.keys())
            this.take(this.peerLists, requestId)?.resolve([]);
    }
    rejectFetch(requestId, error) {
        this.take(this.fetches, requestId)?.reject(error);
    }
    rejectPeerList(requestId, error) {
        this.take(this.peerLists, requestId)?.reject(error);
    }
    take(pendingById, requestId) {
        const pending = pendingById.get(requestId);
        if (!pending)
            return undefined;
        pendingById.delete(requestId);
        if (pending.timeout)
            clearTimeout(pending.timeout);
        if (pending.signal && pending.abort)
            pending.signal.removeEventListener('abort', pending.abort);
        return pending;
    }
    nextRequestId(prefix) {
        this.requestCounter += 1;
        return `${prefix}_${Date.now()}_${this.requestCounter}`;
    }
}
//# sourceMappingURL=p2pBridge.js.map