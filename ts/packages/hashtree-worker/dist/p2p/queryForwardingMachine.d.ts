type TimeoutHandle = ReturnType<typeof setTimeout>;
export interface ForwardTimeoutEvent {
    hashKey: string;
    requesterIds: string[];
}
export type ForwardDecision = {
    kind: 'forward';
    targets: string[];
} | {
    kind: 'suppressed';
} | {
    kind: 'rate_limited';
} | {
    kind: 'no_targets';
};
export interface QueryForwardingMachineConfig {
    requestTimeoutMs: number;
    maxForwardsPerPeerWindow?: number;
    forwardRateLimitWindowMs?: number;
    now?: () => number;
    scheduleTimeout?: (callback: () => void, delayMs: number) => TimeoutHandle;
    clearScheduledTimeout?: (timeoutId: TimeoutHandle) => void;
    onForwardTimeout?: (event: ForwardTimeoutEvent) => void;
}
export declare class QueryForwardingMachine {
    private readonly requestTimeoutMs;
    private readonly scheduleTimeout;
    private readonly clearScheduledTimeout;
    private readonly onForwardTimeout?;
    private readonly hashesByRequester;
    private readonly inFlightByHash;
    private readonly rateLimiter;
    constructor(config: QueryForwardingMachineConfig);
    beginForward(hashKey: string, requesterId: string, candidateTargets: string[]): ForwardDecision;
    resolveForward(hashKey: string): string[];
    cancelForward(hashKey: string): string[];
    removePeer(peerId: string): void;
    stop(): void;
    isInFlight(hashKey: string): boolean;
    getInFlightCount(): number;
    private handleForwardTimeout;
    private clearForward;
    private trackRequester;
}
export {};
//# sourceMappingURL=queryForwardingMachine.d.ts.map