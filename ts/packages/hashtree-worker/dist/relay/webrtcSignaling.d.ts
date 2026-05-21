/**
 * WebRTC Signaling Handler for Hashtree Worker
 *
 * Handles WebRTC signaling via Nostr (kind 25050).
 * - Hello messages: broadcast with #l tag for peer discovery
 * - Directed messages (offer/answer/candidates): gift-wrapped for privacy
 */
import type { SignedEvent } from './protocol';
import type { SignalingMessage } from '@hashtree/nostr';
import type { WebRTCController } from './webrtc';
/**
 * Initialize the WebRTC signaling handler
 */
export declare function initWebRTCSignaling(controller: WebRTCController): void;
/**
 * Send WebRTC signaling message via Nostr (kind 25050)
 * - Hello messages: broadcast with #l tag
 * - Directed messages (offer/answer/candidates): gift-wrapped
 */
export declare function sendWebRTCSignaling(msg: SignalingMessage, recipientPubkey?: string): Promise<void>;
/**
 * Subscribe to WebRTC signaling events.
 * NOTE: The caller must set up the event handler via setOnEvent
 * and route webrtc-* subscriptions to handleWebRTCSignalingEvent.
 */
export declare function setupWebRTCSignalingSubscription(myPubkey: string): void;
/**
 * Re-subscribe to WebRTC signaling after relay change.
 * Call this after setRelays to ensure subscriptions work on new relays.
 */
export declare function resubscribeWebRTCSignaling(): void;
/**
 * Handle incoming WebRTC signaling event.
 * Call this from the unified NostrManager event handler for webrtc-* subscriptions.
 */
export declare function handleWebRTCSignalingEvent(event: SignedEvent): Promise<void>;
//# sourceMappingURL=webrtcSignaling.d.ts.map