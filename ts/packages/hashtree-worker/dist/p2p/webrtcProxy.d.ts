/**
 * WebRTC Proxy
 *
 * Thin transport layer that manages RTCPeerConnection in main thread.
 * Worker controls all logic - this just executes commands and reports events.
 *
 * Main thread owns RTCPeerConnection because it's not available in workers.
 * See: https://github.com/w3c/webrtc-extensions/issues/77
 */
import type { WebRTCCommand, WebRTCEvent } from './protocol.js';
type EventCallback = (event: WebRTCEvent) => void;
type WebRTCProxyConfig = {
    maxUploadBytesPerSecond?: number | null;
};
export declare class WebRTCProxy {
    private peers;
    private onEvent;
    private readonly uploadRateLimiter;
    private draining;
    private drainTimeoutId;
    private nextQueueSequence;
    private static readonly MAX_QUEUE_BYTES;
    private static readonly MAX_QUEUE_ITEMS;
    constructor(onEvent: EventCallback, config?: WebRTCProxyConfig);
    private createSendQueue;
    /**
     * Handle command from worker
     */
    handleCommand(cmd: WebRTCCommand): void;
    private createPeer;
    private setupDataChannel;
    private createOffer;
    private createAnswer;
    private setLocalDescription;
    private setRemoteDescription;
    private addIceCandidate;
    private static readonly BUFFER_THRESHOLD;
    private static readonly QUEUE_HIGH_THRESHOLD;
    private static readonly QUEUE_LOW_THRESHOLD;
    private getQueueSize;
    private isPriorityDataMessage;
    private sendData;
    private drainQueuedPeers;
    private selectNextQueuedPeer;
    private reciprocityWeight;
    private normalizeBandwidthDebt;
    private resetBandwidthDebt;
    private hasQueuedTraffic;
    private maybeSignalBufferLow;
    private refreshBufferedAmountWatchers;
    private closePeer;
    private cleanupPeer;
    /**
     * Close all connections
     */
    close(): void;
    /**
     * Get connected peer count
     */
    getConnectedCount(): number;
    /**
     * Get all peer IDs
     */
    getPeerIds(): string[];
    setUploadLimitBytesPerSecond(maxUploadBytesPerSecond?: number | null): void;
    private scheduleRateLimitedDrain;
}
export declare function initWebRTCProxy(onEvent: EventCallback): WebRTCProxy;
export declare function getWebRTCProxy(): WebRTCProxy | null;
export declare function closeWebRTCProxy(): void;
export {};
//# sourceMappingURL=webrtcProxy.d.ts.map