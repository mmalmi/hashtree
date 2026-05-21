type UploadRateLimiterConfig = {
    bytesPerSecond?: number | null;
    now?: () => number;
};
type UploadReservation = {
    allowed: boolean;
    delayMs: number;
};
export declare class UploadRateLimiter {
    private bytesPerSecond;
    private availableBytes;
    private lastRefillMs;
    private readonly now;
    constructor(config?: UploadRateLimiterConfig);
    setBytesPerSecond(bytesPerSecond?: number | null): void;
    getBytesPerSecond(): number | null;
    reserve(byteLength: number): UploadReservation;
    private refill;
}
export {};
//# sourceMappingURL=uploadRateLimiter.d.ts.map