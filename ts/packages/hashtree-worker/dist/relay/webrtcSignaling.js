// @ts-nocheck
/**
 * WebRTC Signaling Handler for Hashtree Worker
 *
 * Handles WebRTC signaling via Nostr (kind 25050).
 * - Hello messages: broadcast with #l tag for peer discovery
 * - Directed messages (offer/answer/candidates): gift-wrapped for privacy
 */
import { createSignalingFilters, decodeSignalingEvent, sendSignalingMessage, } from '../p2p/signaling.js';
import { subscribe as ndkSubscribe, publish as ndkPublish } from './ndk';
import { signEvent, giftWrap, giftUnwrap } from './signing';
let webrtc = null;
let signalingPubkey = null;
/**
 * Initialize the WebRTC signaling handler
 */
export function initWebRTCSignaling(controller) {
    webrtc = controller;
}
/**
 * Send WebRTC signaling message via Nostr (kind 25050)
 * - Hello messages: broadcast with #l tag
 * - Directed messages (offer/answer/candidates): gift-wrapped
 */
export async function sendWebRTCSignaling(msg, recipientPubkey) {
    try {
        await sendSignalingMessage({
            msg,
            recipientPubkey,
            signEvent,
            giftWrap,
            publish: ndkPublish,
        });
    }
    catch (err) {
        console.error('[Worker] Failed to send WebRTC signaling:', err);
    }
}
/**
 * Subscribe to WebRTC signaling events.
 * NOTE: The caller must set up the event handler via setOnEvent
 * and route webrtc-* subscriptions to handleWebRTCSignalingEvent.
 */
export function setupWebRTCSignalingSubscription(myPubkey) {
    signalingPubkey = myPubkey;
    const { since, helloFilter, directedFilter } = createSignalingFilters(myPubkey);
    console.log('[WebRTC Signaling] Setting up subscriptions for', myPubkey.slice(0, 16), 'since', since);
    // Subscribe to hello messages (broadcast discovery)
    console.log('[WebRTC Signaling] Creating webrtc-hello subscription');
    ndkSubscribe('webrtc-hello', [helloFilter]);
    // Subscribe to directed signaling (offers/answers to us)
    console.log('[WebRTC Signaling] Creating webrtc-directed subscription');
    ndkSubscribe('webrtc-directed', [directedFilter]);
    console.log('[WebRTC Signaling] Subscriptions setup complete');
}
/**
 * Re-subscribe to WebRTC signaling after relay change.
 * Call this after setRelays to ensure subscriptions work on new relays.
 */
export function resubscribeWebRTCSignaling() {
    if (!signalingPubkey) {
        console.warn('[WebRTC Signaling] Cannot resubscribe - no pubkey set');
        return;
    }
    console.log('[WebRTC Signaling] Resubscribing with pubkey:', signalingPubkey.slice(0, 16));
    setupWebRTCSignalingSubscription(signalingPubkey);
    console.log('[WebRTC Signaling] Resubscription complete');
}
/**
 * Handle incoming WebRTC signaling event.
 * Call this from the unified NostrManager event handler for webrtc-* subscriptions.
 */
export async function handleWebRTCSignalingEvent(event) {
    const decoded = await decodeSignalingEvent({
        event,
        giftUnwrap,
    });
    if (!decoded) {
        return;
    }
    if (decoded.message.type !== 'hello') {
        console.log('[WebRTC] Unwrapped message:', decoded.message.type, 'from', decoded.senderPubkey.slice(0, 8));
    }
    webrtc?.handleSignalingMessage(decoded.message, decoded.senderPubkey);
}
//# sourceMappingURL=webrtcSignaling.js.map