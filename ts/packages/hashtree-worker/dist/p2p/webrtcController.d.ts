/**
 * Worker WebRTC Controller
 *
 * Controls WebRTC connections from the worker thread.
 * Main thread proxy executes RTCPeerConnection operations.
 *
 * Worker owns:
 * - Peer state tracking
 * - Connection lifecycle decisions
 * - Data protocol (request/response)
 * - Signaling message handling
 *
 * Main thread proxy owns:
 * - RTCPeerConnection instances (not available in workers)
 * - Data channel I/O
 */
import type { Store } from '@hashtree/core';
import { type SignalingMessage, type PeerPool, type SelectionStrategy, type RequestDispatchConfig } from '@hashtree/mesh';
import type { WebRTCCommand, WebRTCEvent } from './protocol.js';
export interface WebRTCControllerConfig {
    pubkey: string;
    localStore: Store;
    sendCommand: (cmd: WebRTCCommand) => void;
    sendSignaling: (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
    upstreamFetch?: (hash: Uint8Array) => Promise<Uint8Array | null>;
    getFollows?: () => Set<string>;
    requestTimeout?: number;
    forwardRateLimit?: {
        maxForwardsPerPeerWindow?: number;
        windowMs?: number;
    };
    requestSelectionStrategy?: SelectionStrategy;
    requestFairnessEnabled?: boolean;
    requestDispatch?: RequestDispatchConfig;
    debug?: boolean;
}
type PublicPoolConfig = {
    follows: {
        max: number;
        satisfied: number;
    };
    other: {
        max: number;
        satisfied: number;
    };
};
export declare class WebRTCController {
    private myPeerId;
    private peers;
    private pendingRemoteCandidates;
    private localStore;
    private sendCommand;
    private sendSignaling;
    private classifyPeer;
    private requestTimeout;
    private debug;
    private recentRequests;
    private readonly activePeerRequests;
    private readonly meshRouter;
    private readonly peerSelector;
    private routing;
    private poolConfig;
    private helloInterval?;
    private readonly HELLO_INTERVAL;
    constructor(config: WebRTCControllerConfig);
    start(): void;
    stop(): void;
    private sendHello;
    /**
     * Public method to trigger a hello broadcast.
     * Used for testing to force peer discovery after follows are set up.
     */
    broadcastHello(): void;
    /**
     * Handle incoming signaling message (from Nostr kind 25050)
     *
     * `peerId` is the remote endpoint identity.
     */
    handleSignalingMessage(msg: SignalingMessage, senderPubkey: string): Promise<void>;
    private isMessageForUs;
    private handleHello;
    private handleOffer;
    private handleAnswer;
    private handleIceCandidate;
    private shouldConnect;
    private getPoolCount;
    /**
     * Check if we already have a connection from this pubkey in the 'other' pool.
     * In the 'other' pool, we only allow 1 connection per pubkey to prevent spam.
     */
    private hasOtherPoolPubkey;
    private clearPeerRecoveryTimer;
    private schedulePeerRecovery;
    private shouldReplacePeer;
    private createPeer;
    private createOutboundPeer;
    private closePeer;
    /**
     * Handle event from main thread proxy
     */
    handleProxyEvent(event: WebRTCEvent): void;
    private onPeerCreated;
    private onPeerStateChange;
    private onPeerClosed;
    private onOfferCreated;
    private onAnswerCreated;
    private onDescriptionSet;
    private onIceCandidate;
    private onDataChannelOpen;
    private onDataChannelClose;
    private onDataChannelError;
    private onBufferHigh;
    private onBufferLow;
    private processDeferredRequests;
    private orderedConnectedPeers;
    private peerMetadataPointerHash;
    private createInFlightRequest;
    private resetPendingRequestTimeout;
    private waitForInFlightResult;
    private clearPendingHashFromPeers;
    private reservePeerRequest;
    private releasePeerRequest;
    /**
     * Persist selector metadata snapshot to local store.
     * Returns the snapshot hash.
     */
    persistPeerMetadata(): Promise<Uint8Array | null>;
    /**
     * Load selector metadata snapshot from local store.
     */
    loadPeerMetadata(): Promise<boolean>;
    private sendDataToPeer;
    private onDataChannelMessage;
    private handleRequest;
    private processRequest;
    private isFragmentedResponse;
    private handleFragmentResponse;
    private handleResponse;
    private sendResponse;
    private fragmentStallTimeoutMs;
    private sendRequestToPeer;
    private queryPeersWithDispatch;
    /**
     * Request data from peers
     */
    get(hash: Uint8Array): Promise<Uint8Array | null>;
    getConnectedPeerIds(excludePeerId?: string): string[];
    getFromPeer(peerId: string, hash: Uint8Array, htl?: number): Promise<Uint8Array | null>;
    /**
     * Get peer stats for UI
     */
    getPeerStats(): Array<{
        peerId: string;
        pubkey: string;
        connected: boolean;
        pool: PeerPool;
        requestsSent: number;
        requestsReceived: number;
        responsesSent: number;
        responsesReceived: number;
        bytesSent: number;
        bytesReceived: number;
        forwardedRequests: number;
        forwardedResolved: number;
        forwardedSuppressed: number;
    }>;
    /**
     * Get connected peer count
     */
    getConnectedCount(): number;
    getConnectedHashGetPeerIds(): string[];
    /**
     * Set pool configuration
     */
    setPoolConfig(config: PublicPoolConfig | null): void;
    setForwardRateLimit(config?: {
        maxForwardsPerPeerWindow?: number;
        windowMs?: number;
    }): void;
    /**
     * Update identity (pubkey) and restart signaling if already running.
     * This keeps peerId consistent with the current account.
     */
    setIdentity(pubkey: string): void;
    private log;
}
export {};
//# sourceMappingURL=webrtcController.d.ts.map