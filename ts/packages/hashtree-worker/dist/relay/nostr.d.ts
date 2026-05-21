/**
 * Nostr Relay Manager for Worker
 *
 * Manages WebSocket connections to Nostr relays using nostr-tools.
 * Provides subscribe/publish functionality for the worker.
 *
 * Used for:
 * - WebRTC signaling (kind 25050 ephemeral)
 * - Tree root resolution (kind 30078)
 */
import type { NostrFilter, SignedEvent, RelayStats } from './protocol';
export declare class NostrManager {
    private pool;
    private relays;
    private subscriptions;
    private relayStats;
    private onEvent;
    private onEose;
    constructor();
    /**
     * Initialize with relay URLs
     */
    init(relays: string[]): void;
    /**
     * Set event callback
     */
    setOnEvent(callback: (subId: string, event: SignedEvent) => void): void;
    /**
     * Set EOSE callback
     */
    setOnEose(callback: (subId: string) => void): void;
    /**
     * Subscribe to events matching filters
     */
    subscribe(subId: string, filters: NostrFilter[]): void;
    /**
     * Unsubscribe from a subscription
     */
    unsubscribe(subId: string): void;
    /**
     * Publish an event to all relays
     */
    publish(event: SignedEvent): Promise<void>;
    /**
     * Get relay connection stats
     */
    getRelayStats(): RelayStats[];
    /**
     * Update relay connection status
     * Called when connection state changes
     */
    setRelayConnected(url: string, connected: boolean): void;
    /**
     * Close all subscriptions and connections
     */
    close(): void;
}
export declare function getNostrManager(): NostrManager;
export declare function closeNostrManager(): void;
//# sourceMappingURL=nostr.d.ts.map