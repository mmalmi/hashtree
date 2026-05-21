import type { BlossomLogEntry } from '@hashtree/core';
export interface BlossomBandwidthServerStats {
    url: string;
    bytesSent: number;
    bytesReceived: number;
}
export interface BlossomBandwidthStats {
    totalBytesSent: number;
    totalBytesReceived: number;
    updatedAt: number;
    servers: BlossomBandwidthServerStats[];
}
export type BlossomBandwidthUpdateHandler = (stats: BlossomBandwidthStats) => void;
export declare class BlossomBandwidthTracker {
    private totalBytesSent;
    private totalBytesReceived;
    private readonly serverBandwidth;
    private readonly onUpdate?;
    private readonly now;
    constructor(onUpdate?: BlossomBandwidthUpdateHandler, now?: () => number);
    apply(entry: BlossomLogEntry): void;
    getStats(): BlossomBandwidthStats;
    reset(): void;
    private getOrderedServerBandwidth;
}
//# sourceMappingURL=blossomBandwidthTracker.d.ts.map