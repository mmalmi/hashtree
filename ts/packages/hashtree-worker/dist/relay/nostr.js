// @ts-nocheck
/**
 * Nostr Relay Manager for Worker
 *
 * Manages WebSocket connections to Nostr relays using nostr-tools.
 * Provides subscribe/publish functionality for the worker.
 *
 * Used for:
 * - Tree root resolution (kind 30064, with legacy 30078 support)
 */
import { SimplePool } from 'nostr-tools';
export class NostrManager {
    pool;
    relays = [];
    subscriptions = new Map();
    relayStats = new Map();
    onEvent = null;
    onEose = null;
    constructor() {
        this.pool = new SimplePool();
    }
    /**
     * Initialize with relay URLs
     */
    init(relays) {
        this.relays = relays;
        // Initialize stats for each relay
        for (const url of relays) {
            this.relayStats.set(url, {
                url,
                connected: false,
                eventsReceived: 0,
                eventsSent: 0,
            });
        }
        console.log('[NostrManager] Initialized with relays:', relays);
    }
    /**
     * Set event callback
     */
    setOnEvent(callback) {
        this.onEvent = callback;
    }
    /**
     * Set EOSE callback
     */
    setOnEose(callback) {
        this.onEose = callback;
    }
    /**
     * Subscribe to events matching filters
     */
    subscribe(subId, filters) {
        // Close existing subscription with same ID if any
        this.unsubscribe(subId);
        console.log('[NostrManager] Creating subscription:', subId, 'to relays:', this.relays, 'filters:', filters);
        // Convert our NostrFilter to nostr-tools Filter
        // Subscribe to each filter separately and track them
        const subs = [];
        for (const f of filters) {
            // Build filter with any tag filters (e.g., #e, #p, #d, #l)
            const poolFilter = {
                ids: f.ids,
                authors: f.authors,
                kinds: f.kinds,
                since: f.since,
                until: f.until,
                limit: f.limit,
            };
            // Copy any tag filters (keys starting with #)
            for (const key of Object.keys(f)) {
                if (key.startsWith('#') && f[key]) {
                    poolFilter[key] = f[key];
                }
            }
            try {
                console.log('[NostrManager] pool.subscribe called with filter:', poolFilter);
                const sub = this.pool.subscribe(this.relays, poolFilter, {
                    onevent: (event) => {
                        console.log('[NostrManager] Received event:', event.kind, 'from:', event.pubkey?.slice(0, 8), 'id:', event.id?.slice(0, 8));
                        // Convert to SignedEvent
                        const signedEvent = {
                            id: event.id,
                            pubkey: event.pubkey,
                            kind: event.kind,
                            content: event.content,
                            tags: event.tags,
                            created_at: event.created_at,
                            sig: event.sig,
                        };
                        this.onEvent?.(subId, signedEvent);
                    },
                    oneose: () => {
                        console.log('[NostrManager] EOSE for sub:', subId);
                        this.onEose?.(subId);
                    },
                    onerror: (err) => {
                        console.error('[NostrManager] Subscription error:', subId, err);
                    },
                });
                console.log('[NostrManager] Subscription object:', typeof sub, sub);
                subs.push(sub);
            }
            catch (err) {
                console.error('[NostrManager] Error creating subscription:', subId, err);
            }
        }
        // Store all subs for this subscription ID
        this.subscriptions.set(subId, { id: subId, filters, subs });
        console.log('[NostrManager] Subscribed:', subId, filters);
    }
    /**
     * Unsubscribe from a subscription
     */
    unsubscribe(subId) {
        const sub = this.subscriptions.get(subId);
        if (sub) {
            for (const s of sub.subs) {
                s.close();
            }
            this.subscriptions.delete(subId);
            console.log('[NostrManager] Unsubscribed:', subId);
        }
    }
    /**
     * Publish an event to all relays
     */
    async publish(event) {
        // Convert to nostr-tools Event
        const poolEvent = {
            id: event.id,
            pubkey: event.pubkey,
            kind: event.kind,
            content: event.content,
            tags: event.tags,
            created_at: event.created_at,
            sig: event.sig,
        };
        try {
            await Promise.any(this.pool.publish(this.relays, poolEvent));
            // Update stats for successful publish
            for (const [, stats] of this.relayStats) {
                stats.eventsSent++;
            }
            console.log('[NostrManager] Published event:', event.id);
        }
        catch (err) {
            console.error('[NostrManager] Failed to publish:', err);
            throw err;
        }
    }
    /**
     * Get relay connection stats
     */
    getRelayStats() {
        const result = [];
        for (const [url, stats] of this.relayStats) {
            result.push({
                url,
                connected: stats.connected,
                eventsReceived: stats.eventsReceived,
                eventsSent: stats.eventsSent,
            });
        }
        return result;
    }
    /**
     * Update relay connection status
     * Called when connection state changes
     */
    setRelayConnected(url, connected) {
        const stats = this.relayStats.get(url);
        if (stats) {
            stats.connected = connected;
        }
    }
    /**
     * Close all subscriptions and connections
     */
    close() {
        for (const [subId, sub] of this.subscriptions) {
            for (const s of sub.subs) {
                s.close();
            }
            console.log('[NostrManager] Closed subscription:', subId);
        }
        this.subscriptions.clear();
        this.pool.close(this.relays);
        console.log('[NostrManager] Closed');
    }
}
// Singleton instance for the worker
let instance = null;
export function getNostrManager() {
    if (!instance) {
        instance = new NostrManager();
    }
    return instance;
}
export function closeNostrManager() {
    if (instance) {
        instance.close();
        instance = null;
    }
}
//# sourceMappingURL=nostr.js.map