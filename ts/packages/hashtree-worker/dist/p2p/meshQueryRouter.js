import { MAX_HTL, createRequest, decrementHTL, encodeRequest, hashToKey, shouldForward, verifyHash, } from '@hashtree/mesh';
class SlidingWindowRateLimiter {
    maxEvents;
    windowMs;
    eventsByPeer = new Map();
    constructor(maxEvents, windowMs) {
        this.maxEvents = maxEvents;
        this.windowMs = windowMs;
    }
    allow(peerId) {
        const now = Date.now();
        const events = this.eventsByPeer.get(peerId) ?? [];
        let firstActiveIndex = 0;
        while (firstActiveIndex < events.length && now - events[firstActiveIndex] >= this.windowMs) {
            firstActiveIndex += 1;
        }
        if (firstActiveIndex > 0) {
            events.splice(0, firstActiveIndex);
        }
        if (events.length >= this.maxEvents) {
            this.eventsByPeer.set(peerId, events);
            return false;
        }
        events.push(now);
        this.eventsByPeer.set(peerId, events);
        return true;
    }
    resetPeer(peerId) {
        this.eventsByPeer.delete(peerId);
    }
    clear() {
        this.eventsByPeer.clear();
    }
}
export class MeshQueryRouter {
    localStore;
    requestTimeoutMs;
    rateLimiter;
    peers = new Map();
    hashesByRequester = new Map();
    inFlightByHash = new Map();
    pendingUpstreamFetches = new Map();
    upstreamFetch;
    queryPeers;
    constructor(config) {
        this.localStore = config.localStore;
        this.requestTimeoutMs = config.requestTimeoutMs;
        this.upstreamFetch = config.upstreamFetch;
        this.queryPeers = config.queryPeers;
        this.rateLimiter = this.createRateLimiter({
            maxForwardsPerPeerWindow: config.maxForwardsPerPeerWindow,
            windowMs: config.forwardRateLimitWindowMs,
        });
    }
    registerPeer(peer) {
        this.peers.set(peer.peerId, peer);
    }
    removePeer(peerId) {
        const hashes = this.hashesByRequester.get(peerId);
        if (hashes) {
            for (const hashKey of Array.from(hashes)) {
                const inFlight = this.inFlightByHash.get(hashKey);
                if (!inFlight)
                    continue;
                inFlight.requesterIds.delete(peerId);
                if (inFlight.requesterIds.size === 0) {
                    this.clearQuery(hashKey);
                }
            }
        }
        this.hashesByRequester.delete(peerId);
        this.peers.delete(peerId);
        this.rateLimiter.resetPeer(peerId);
    }
    setUpstreamFetch(upstreamFetch) {
        this.upstreamFetch = upstreamFetch;
    }
    setForwardRateLimit(config) {
        this.rateLimiter = this.createRateLimiter(config);
    }
    hasInFlight(hashKey) {
        return this.inFlightByHash.has(hashKey);
    }
    stop() {
        for (const hashKey of Array.from(this.inFlightByHash.keys())) {
            this.clearQuery(hashKey);
        }
        this.hashesByRequester.clear();
        this.pendingUpstreamFetches.clear();
        this.rateLimiter.clear();
    }
    createRateLimiter(config) {
        return new SlidingWindowRateLimiter(config?.maxForwardsPerPeerWindow ?? 64, config?.windowMs ?? 1000);
    }
    async handleRequest(requesterId, req) {
        const hashKey = hashToKey(req.h);
        const requester = this.peers.get(requesterId);
        if (!requester) {
            return;
        }
        const local = await this.localStore.get(req.h);
        if (local) {
            await requester.sendResponse(req.h, local);
            return;
        }
        const begin = this.beginQuery(hashKey, requesterId);
        if (begin === 'suppressed') {
            requester.onForwardedSuppressed?.();
            return;
        }
        const shouldAttemptPeerQuery = this.shouldAttemptPeerQuery(requesterId, req.htl ?? MAX_HTL);
        const peerQueryAllowed = !shouldAttemptPeerQuery || this.rateLimiter.allow(requesterId);
        const peerQueryActive = peerQueryAllowed
            ? this.startPeerQuery(hashKey, req.h, requesterId, req.htl ?? MAX_HTL)
            : false;
        const upstreamActive = this.startUpstreamFetch(hashKey, req.h);
        const forwarded = peerQueryAllowed && !peerQueryActive
            ? this.forwardRequest(requesterId, req.h, req.htl ?? MAX_HTL)
            : 0;
        if (peerQueryActive || forwarded > 0 || upstreamActive) {
            requester.onForwardedRequest?.();
            return;
        }
        this.clearQuery(hashKey);
    }
    async resolve(hash, data) {
        const hashKey = hashToKey(hash);
        const requesterIds = this.clearQuery(hashKey);
        if (requesterIds.length === 0) {
            return;
        }
        await this.localStore.put(hash, data).catch(() => false);
        for (const requesterId of requesterIds) {
            const requester = this.peers.get(requesterId);
            if (!requester) {
                continue;
            }
            requester.onForwardedResolved?.();
            await requester.sendResponse(hash, data);
        }
    }
    beginQuery(hashKey, requesterId) {
        const existing = this.inFlightByHash.get(hashKey);
        if (existing) {
            this.trackRequester(hashKey, existing.requesterIds, requesterId);
            return 'suppressed';
        }
        const requesterIds = new Set();
        this.trackRequester(hashKey, requesterIds, requesterId);
        const timeoutId = setTimeout(() => {
            this.clearQuery(hashKey);
        }, this.requestTimeoutMs);
        this.inFlightByHash.set(hashKey, { requesterIds, timeoutId });
        return 'new';
    }
    shouldAttemptPeerQuery(requesterId, htl) {
        if (!shouldForward(htl)) {
            return false;
        }
        for (const peer of this.peers.values()) {
            if (peer.peerId !== requesterId && peer.canSend()) {
                return true;
            }
        }
        return false;
    }
    clearQuery(hashKey) {
        const inFlight = this.inFlightByHash.get(hashKey);
        if (!inFlight) {
            return [];
        }
        clearTimeout(inFlight.timeoutId);
        this.inFlightByHash.delete(hashKey);
        const requesterIds = Array.from(inFlight.requesterIds);
        for (const requesterId of requesterIds) {
            const hashes = this.hashesByRequester.get(requesterId);
            if (!hashes)
                continue;
            hashes.delete(hashKey);
            if (hashes.size === 0) {
                this.hashesByRequester.delete(requesterId);
            }
        }
        return requesterIds;
    }
    trackRequester(hashKey, requesterIds, requesterId) {
        requesterIds.add(requesterId);
        let hashes = this.hashesByRequester.get(requesterId);
        if (!hashes) {
            hashes = new Set();
            this.hashesByRequester.set(requesterId, hashes);
        }
        hashes.add(hashKey);
    }
    forwardRequest(requesterId, hash, htl) {
        if (!shouldForward(htl)) {
            return 0;
        }
        const requester = this.peers.get(requesterId);
        if (!requester) {
            return 0;
        }
        const nextHtl = decrementHTL(htl, requester.getHtlConfig());
        let forwarded = 0;
        for (const peer of this.peers.values()) {
            if (peer.peerId === requesterId || !peer.canSend()) {
                continue;
            }
            if (peer.sendRequest(hash, nextHtl)) {
                forwarded += 1;
            }
        }
        return forwarded;
    }
    startPeerQuery(hashKey, hash, requesterId, htl) {
        if (!this.queryPeers || !shouldForward(htl)) {
            return false;
        }
        const requester = this.peers.get(requesterId);
        if (!requester) {
            return false;
        }
        const nextHtl = decrementHTL(htl, requester.getHtlConfig());
        void this.queryPeers(hash, {
            excludePeerId: requesterId,
            htl: nextHtl,
        }).then(async (data) => {
            if (!data || !this.inFlightByHash.has(hashKey)) {
                return;
            }
            const valid = await verifyHash(data, hash);
            if (!valid) {
                return;
            }
            await this.resolve(hash, data);
        }).catch(() => undefined);
        return true;
    }
    startUpstreamFetch(hashKey, hash) {
        if (!this.upstreamFetch) {
            return false;
        }
        const existing = this.pendingUpstreamFetches.get(hashKey);
        if (existing) {
            return true;
        }
        let pending;
        pending = this.upstreamFetch(hash)
            .then(async (data) => {
            if (!data) {
                return null;
            }
            const valid = await verifyHash(data, hash);
            if (!valid) {
                return null;
            }
            await this.resolve(hash, data);
            return data;
        })
            .catch(() => null)
            .finally(() => {
            if (this.pendingUpstreamFetches.get(hashKey) === pending) {
                this.pendingUpstreamFetches.delete(hashKey);
            }
        });
        this.pendingUpstreamFetches.set(hashKey, pending);
        return true;
    }
}
export function encodeForwardRequest(hash, htl) {
    return new Uint8Array(encodeRequest(createRequest(hash, htl)));
}
//# sourceMappingURL=meshQueryRouter.js.map