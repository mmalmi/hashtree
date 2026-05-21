export class BlossomBandwidthTracker {
    totalBytesSent = 0;
    totalBytesReceived = 0;
    serverBandwidth = new Map();
    onUpdate;
    now;
    constructor(onUpdate, now = () => Date.now()) {
        this.onUpdate = onUpdate;
        this.now = now;
    }
    apply(entry) {
        const bytes = entry.bytes ?? 0;
        if (!entry.success || bytes <= 0)
            return;
        const serverStats = this.serverBandwidth.get(entry.server) ?? { bytesSent: 0, bytesReceived: 0 };
        if (entry.operation === 'put') {
            this.totalBytesSent += bytes;
            serverStats.bytesSent += bytes;
        }
        else if (entry.operation === 'get') {
            this.totalBytesReceived += bytes;
            serverStats.bytesReceived += bytes;
        }
        else {
            return;
        }
        this.serverBandwidth.set(entry.server, serverStats);
        this.onUpdate?.(this.getStats());
    }
    getStats() {
        return {
            totalBytesSent: this.totalBytesSent,
            totalBytesReceived: this.totalBytesReceived,
            updatedAt: this.now(),
            servers: this.getOrderedServerBandwidth(),
        };
    }
    reset() {
        this.totalBytesSent = 0;
        this.totalBytesReceived = 0;
        this.serverBandwidth.clear();
    }
    getOrderedServerBandwidth() {
        return Array.from(this.serverBandwidth.entries())
            .map(([url, stats]) => ({
            url,
            bytesSent: stats.bytesSent,
            bytesReceived: stats.bytesReceived,
        }))
            .sort((a, b) => a.url.localeCompare(b.url));
    }
}
//# sourceMappingURL=blossomBandwidthTracker.js.map