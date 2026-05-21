function normalizeBytesPerSecond(value) {
    if (!Number.isFinite(value) || !value || value <= 0) {
        return null;
    }
    return Math.floor(value);
}
export class UploadRateLimiter {
    bytesPerSecond;
    availableBytes;
    lastRefillMs;
    now;
    constructor(config = {}) {
        this.now = config.now ?? (() => performance.now());
        this.bytesPerSecond = normalizeBytesPerSecond(config.bytesPerSecond);
        this.availableBytes = this.bytesPerSecond ?? Number.POSITIVE_INFINITY;
        this.lastRefillMs = this.now();
    }
    setBytesPerSecond(bytesPerSecond) {
        const nowMs = this.now();
        this.refill(nowMs);
        this.bytesPerSecond = normalizeBytesPerSecond(bytesPerSecond);
        this.availableBytes = this.bytesPerSecond
            ? Math.min(this.availableBytes, this.bytesPerSecond)
            : Number.POSITIVE_INFINITY;
        this.lastRefillMs = nowMs;
    }
    getBytesPerSecond() {
        return this.bytesPerSecond;
    }
    reserve(byteLength) {
        if (byteLength <= 0) {
            return { allowed: true, delayMs: 0 };
        }
        const limit = this.bytesPerSecond;
        if (!limit) {
            return { allowed: true, delayMs: 0 };
        }
        const nowMs = this.now();
        this.refill(nowMs);
        if (this.availableBytes >= byteLength) {
            this.availableBytes = Math.max(0, this.availableBytes - byteLength);
            return { allowed: true, delayMs: 0 };
        }
        const missingBytes = byteLength - this.availableBytes;
        return {
            allowed: false,
            delayMs: Math.max(4, Math.ceil((missingBytes / limit) * 1000)),
        };
    }
    refill(nowMs) {
        const limit = this.bytesPerSecond;
        if (!limit) {
            this.availableBytes = Number.POSITIVE_INFINITY;
            this.lastRefillMs = nowMs;
            return;
        }
        const elapsedMs = Math.max(0, nowMs - this.lastRefillMs);
        this.lastRefillMs = nowMs;
        this.availableBytes = Math.min(limit, this.availableBytes + (elapsedMs * limit) / 1000);
    }
}
//# sourceMappingURL=uploadRateLimiter.js.map