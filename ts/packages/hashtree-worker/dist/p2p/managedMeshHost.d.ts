import type { Store } from '@hashtree/core';
import type { SignalingMessage } from '@hashtree/mesh';
import type { HashtreeWorkerClient } from '../client.js';
import type { WebRTCEvent } from './protocol.js';
import { type GiftSeal, type SignalingEventLike } from './signaling.js';
import { SimplePool } from 'nostr-tools/pool';
import { WebRTCController, type WebRTCControllerConfig } from './webrtcController.js';
import { WebRTCProxy } from './webrtcProxy.js';
export type WebRTCMeshPoolConfig = {
    follows: {
        max: number;
        satisfied: number;
    };
    other: {
        max: number;
        satisfied: number;
    };
};
export interface ManagedWebRTCMeshSessionConfig {
    signature: string;
    pubkey: string;
    relayUrls: string[];
    localStore: Store;
    closeLocalStore?: () => void | Promise<void>;
    sendSignaling?: (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
    createSendSignaling?: (context: {
        signalPool: SimplePool;
        relayUrls: string[];
    }) => (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
    unwrapGift: (event: SignalingEventLike) => Promise<GiftSeal | null>;
    getFollows?: () => Set<string>;
    requestTimeoutMs?: number;
    forwardRateLimit?: WebRTCControllerConfig['forwardRateLimit'];
    upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>;
    debug?: boolean;
}
export interface ManagedWebRTCMeshHostOptions {
    healthCheckIntervalMs?: number;
    reannounceIntervalMs?: number;
    restartIntervalMs?: number;
    createSignalPool?: () => SimplePool;
    createController?: (config: WebRTCControllerConfig) => WebRTCController;
    createProxy?: (onEvent: (event: WebRTCEvent) => void, maxUploadBytesPerSecond: number | null) => WebRTCProxy;
}
export declare class ManagedWebRTCMeshHost {
    private readonly healthCheckIntervalMs;
    private readonly reannounceIntervalMs;
    private readonly restartIntervalMs;
    private readonly createSignalPool;
    private readonly createController;
    private readonly createProxy;
    private workerClient;
    private desiredSession;
    private active;
    private syncVersion;
    private currentSync;
    private uploadLimitBytesPerSecond;
    private poolConfig;
    private healthCheckTimer;
    private lastHealthyAt;
    private lastHelloAt;
    private lastRestartAt;
    constructor(options?: ManagedWebRTCMeshHostOptions);
    attachWorkerClient(client: HashtreeWorkerClient, options?: {
        canFetch?: () => boolean | Promise<boolean>;
    }): void;
    setSession(session: ManagedWebRTCMeshSessionConfig | null, force?: boolean): Promise<void>;
    setUploadLimitBytesPerSecond(maxUploadBytesPerSecond?: number | null): void;
    setPoolConfig(config: WebRTCMeshPoolConfig | null): void;
    broadcastHello(): void;
    getController(): WebRTCController | null;
    getPubkey(): string | null;
    getActiveSignature(): string | null;
    getRelayUrls(): string[];
    getRelayConnectionStatus(): ReadonlyMap<string, boolean>;
    getConnectedPeerCount(): number;
    getPeerStats(): ReturnType<WebRTCController['getPeerStats']>;
    isActive(): boolean;
    close(): Promise<void>;
    private ensureStarted;
    private startHealthCheck;
    private stopHealthCheck;
    private checkHealth;
    private runSync;
    private routeSignalEvent;
    private disposeActive;
}
//# sourceMappingURL=managedMeshHost.d.ts.map