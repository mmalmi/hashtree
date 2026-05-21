class SlidingWindowRateLimiter {
    maxEvents;
    windowMs;
    now;
    eventsByPeer = new Map();
    constructor(maxEvents, windowMs, now) {
        this.maxEvents = maxEvents;
        this.windowMs = windowMs;
        this.now = now;
    }
    allow(peerId) {
        const now = this.now();
        const events = this.eventsByPeer.get(peerId) ?? [];
        let firstActiveIndex = 0;
        while (firstActiveIndex < events.length && now - events[firstActiveIndex] >= this.windowMs) {
            firstActiveIndex++;
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
export class QueryForwardingMachine {
    requestTimeoutMs;
    scheduleTimeout;
    clearScheduledTimeout;
    onForwardTimeout;
    hashesByRequester = new Map();
    inFlightByHash = new Map();
    rateLimiter;
    constructor(config) {
        this.requestTimeoutMs = config.requestTimeoutMs;
        this.scheduleTimeout = config.scheduleTimeout ?? ((callback, delayMs) => setTimeout(callback, delayMs));
        this.clearScheduledTimeout = config.clearScheduledTimeout ?? ((timeoutId) => clearTimeout(timeoutId));
        this.onForwardTimeout = config.onForwardTimeout;
        const now = config.now ?? (() => Date.now());
        const maxForwardsPerPeerWindow = config.maxForwardsPerPeerWindow ?? 64;
        const forwardRateLimitWindowMs = config.forwardRateLimitWindowMs ?? 1000;
        this.rateLimiter = new SlidingWindowRateLimiter(maxForwardsPerPeerWindow, forwardRateLimitWindowMs, now);
    }
    beginForward(hashKey, requesterId, candidateTargets) {
        const targets = candidateTargets.filter(target => target !== requesterId);
        if (targets.length === 0) {
            return { kind: 'no_targets' };
        }
        const existing = this.inFlightByHash.get(hashKey);
        if (existing) {
            this.trackRequester(hashKey, existing.requesters, requesterId);
            return { kind: 'suppressed' };
        }
        if (!this.rateLimiter.allow(requesterId)) {
            return { kind: 'rate_limited' };
        }
        const requesters = new Set();
        this.trackRequester(hashKey, requesters, requesterId);
        const timeoutId = this.scheduleTimeout(() => {
            this.handleForwardTimeout(hashKey);
        }, this.requestTimeoutMs);
        this.inFlightByHash.set(hashKey, { requesters, timeoutId });
        return { kind: 'forward', targets };
    }
    resolveForward(hashKey) {
        return this.clearForward(hashKey, false);
    }
    cancelForward(hashKey) {
        return this.clearForward(hashKey, false);
    }
    removePeer(peerId) {
        const hashes = this.hashesByRequester.get(peerId);
        if (hashes) {
            for (const hashKey of Array.from(hashes)) {
                const inFlight = this.inFlightByHash.get(hashKey);
                if (!inFlight)
                    continue;
                inFlight.requesters.delete(peerId);
                if (inFlight.requesters.size === 0) {
                    this.clearForward(hashKey, false);
                }
            }
        }
        this.hashesByRequester.delete(peerId);
        this.rateLimiter.resetPeer(peerId);
    }
    stop() {
        for (const hashKey of Array.from(this.inFlightByHash.keys())) {
            this.clearForward(hashKey, false);
        }
        this.hashesByRequester.clear();
        this.rateLimiter.clear();
    }
    isInFlight(hashKey) {
        return this.inFlightByHash.has(hashKey);
    }
    getInFlightCount() {
        return this.inFlightByHash.size;
    }
    handleForwardTimeout(hashKey) {
        this.clearForward(hashKey, true);
    }
    clearForward(hashKey, notifyTimeout) {
        const inFlight = this.inFlightByHash.get(hashKey);
        if (!inFlight)
            return [];
        this.clearScheduledTimeout(inFlight.timeoutId);
        this.inFlightByHash.delete(hashKey);
        const requesterIds = Array.from(inFlight.requesters);
        for (const requesterId of requesterIds) {
            const hashes = this.hashesByRequester.get(requesterId);
            if (!hashes)
                continue;
            hashes.delete(hashKey);
            if (hashes.size === 0) {
                this.hashesByRequester.delete(requesterId);
            }
        }
        if (notifyTimeout && this.onForwardTimeout) {
            this.onForwardTimeout({ hashKey, requesterIds });
        }
        return requesterIds;
    }
    trackRequester(hashKey, requesters, requesterId) {
        requesters.add(requesterId);
        let hashes = this.hashesByRequester.get(requesterId);
        if (!hashes) {
            hashes = new Set();
            this.hashesByRequester.set(requesterId, hashes);
        }
        hashes.add(hashKey);
    }
}
//# sourceMappingURL=queryForwardingMachine.js.map