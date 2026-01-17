/**
 * WebRTC Signaling Handler for Hashtree Worker
 *
 * Handles WebRTC signaling via Nostr (kind 25050).
 * - Hello messages: broadcast with #l tag for peer discovery
 * - Directed messages (offer/answer/candidates): gift-wrapped for privacy
 */

import type { SignedEvent } from './protocol';
import type { SignalingMessage } from '../../../../ts/packages/hashtree/src/webrtc/types';
import type { WebRTCController } from './webrtc';
import { subscribe as ndkSubscribe, publish as ndkPublish } from './ndk';
import { signEvent, giftWrap, giftUnwrap } from './signing';

// Kind for WebRTC signaling (ephemeral, gift-wrapped for directed messages)
const SIGNALING_KIND = 25050;
const HELLO_TAG = 'hello';
const MAX_EVENT_AGE_SEC = 30; // Ignore hellos older than this (3 hello intervals)

let webrtc: WebRTCController | null = null;

function normalizeSignalingMessage(raw: unknown, senderPubkey: string): SignalingMessage | null {
  if (!raw || typeof raw !== 'object' || !('type' in raw)) return null;
  const msg = raw as Record<string, unknown>;
  if (typeof msg.type !== 'string') return null;

  if ('targetPeerId' in msg) {
    return msg as SignalingMessage;
  }

  if (!('recipient' in msg) || typeof msg.recipient !== 'string' || typeof msg.peerId !== 'string') {
    return null;
  }

  const senderPeerId = msg.peerId.includes(':') ? msg.peerId : `${senderPubkey}:${msg.peerId}`;
  const targetPeerId = msg.recipient;

  switch (msg.type) {
    case 'offer': {
      const offer = msg.offer as { sdp?: string } | string | undefined;
      const sdp = typeof offer === 'string' ? offer : offer?.sdp;
      return sdp ? { type: 'offer', peerId: senderPeerId, targetPeerId, sdp } : null;
    }
    case 'answer': {
      const answer = msg.answer as { sdp?: string } | string | undefined;
      const sdp = typeof answer === 'string' ? answer : answer?.sdp;
      return sdp ? { type: 'answer', peerId: senderPeerId, targetPeerId, sdp } : null;
    }
    case 'candidate': {
      const candidateObj = msg.candidate as { candidate?: string; sdpMLineIndex?: number; sdpMid?: string } | string | undefined;
      const candidate = typeof candidateObj === 'string' ? candidateObj : candidateObj?.candidate;
      return candidate
        ? {
            type: 'candidate',
            peerId: senderPeerId,
            targetPeerId,
            candidate,
            sdpMLineIndex: typeof candidateObj === 'object' ? candidateObj?.sdpMLineIndex : undefined,
            sdpMid: typeof candidateObj === 'object' ? candidateObj?.sdpMid : undefined,
          }
        : null;
    }
    case 'candidates': {
      const candidates = Array.isArray(msg.candidates)
        ? msg.candidates
            .map((entry) => {
              if (typeof entry === 'string') {
                return { candidate: entry };
              }
              if (entry && typeof entry === 'object') {
                const candidateEntry = entry as { candidate?: string; sdpMLineIndex?: number; sdpMid?: string };
                if (typeof candidateEntry.candidate === 'string') {
                  return {
                    candidate: candidateEntry.candidate,
                    sdpMLineIndex: candidateEntry.sdpMLineIndex,
                    sdpMid: candidateEntry.sdpMid,
                  };
                }
              }
              return null;
            })
            .filter((entry): entry is { candidate: string; sdpMLineIndex?: number; sdpMid?: string } => !!entry)
        : [];

      return { type: 'candidates', peerId: senderPeerId, targetPeerId, candidates };
    }
    default:
      return null;
  }
}

/**
 * Initialize the WebRTC signaling handler
 */
export function initWebRTCSignaling(controller: WebRTCController): void {
  webrtc = controller;
}

/**
 * Send WebRTC signaling message via Nostr (kind 25050)
 * - Hello messages: broadcast with #l tag
 * - Directed messages (offer/answer/candidates): gift-wrapped
 */
export async function sendWebRTCSignaling(
  msg: SignalingMessage,
  recipientPubkey?: string
): Promise<void> {
  try {
    if (recipientPubkey) {
      // Directed message - gift wrap for privacy
      const innerEvent = {
        kind: SIGNALING_KIND,
        content: JSON.stringify(msg),
        tags: [] as string[][],
      };
      const wrappedEvent = await giftWrap(innerEvent, recipientPubkey);
      await ndkPublish(wrappedEvent);
    } else {
      // Hello message - broadcast with #l tag
      const expiration = Math.floor((Date.now() + 5 * 60 * 1000) / 1000); // 5 minutes
      const event = await signEvent({
        kind: SIGNALING_KIND,
        created_at: Math.floor(Date.now() / 1000),
        tags: [
          ['l', HELLO_TAG],
          ['peerId', msg.peerId],
          ['expiration', expiration.toString()],
        ],
        content: '',
      });
      await ndkPublish(event);
    }
  } catch (err) {
    console.error('[Worker] Failed to send WebRTC signaling:', err);
  }
}

/**
 * Subscribe to WebRTC signaling events.
 * NOTE: The caller must set up the event handler via setOnEvent
 * and route webrtc-* subscriptions to handleWebRTCSignalingEvent.
 */
export function setupWebRTCSignalingSubscription(myPubkey: string): void {
  const since = Math.floor((Date.now() - MAX_EVENT_AGE_SEC * 1000) / 1000);

  // Subscribe to hello messages (broadcast discovery)
  ndkSubscribe('webrtc-hello', [
    {
      kinds: [SIGNALING_KIND],
      '#l': [HELLO_TAG],
      since,
    },
  ]);

  // Subscribe to directed signaling (offers/answers to us)
  ndkSubscribe('webrtc-directed', [
    {
      kinds: [SIGNALING_KIND],
      '#p': [myPubkey],
      since,
    },
  ]);
}

/**
 * Handle incoming WebRTC signaling event.
 * Call this from the unified NostrManager event handler for webrtc-* subscriptions.
 */
export async function handleWebRTCSignalingEvent(event: SignedEvent): Promise<void> {
  // Filter out old events
  const eventAge = Date.now() / 1000 - (event.created_at ?? 0);
  if (eventAge > MAX_EVENT_AGE_SEC) {
    return;
  }

  // Check expiration
  const expirationTag = event.tags.find((t) => t[0] === 'expiration');
  if (expirationTag) {
    const expiration = parseInt(expirationTag[1], 10);
    if (expiration < Date.now() / 1000) return;
  }

  // Check if it's a hello message (has #l tag)
  const isHello = event.tags.some((t) => t[0] === 'l' && t[1] === HELLO_TAG);

  if (isHello) {
    // Hello message - extract peerId from tag
    const peerIdTag = event.tags.find((t) => t[0] === 'peerId');
    if (peerIdTag) {
      const msg: SignalingMessage = {
        type: 'hello',
        peerId: peerIdTag[1],
      };
      webrtc?.handleSignalingMessage(msg, event.pubkey);
    }
  } else {
    // Directed message - try to unwrap
    console.log('[WebRTC] Received directed event from', event.pubkey.slice(0, 8), 'with #p tag');
    const seal = await giftUnwrap(event);
    if (seal && seal.content) {
      try {
        const raw = JSON.parse(seal.content);
        const msg = normalizeSignalingMessage(raw, seal.pubkey);
        if (!msg) {
          console.log('[WebRTC] Unwrapped message had unsupported format');
          return;
        }
        console.log('[WebRTC] Unwrapped message:', msg.type, 'from', seal.pubkey.slice(0, 8));
        webrtc?.handleSignalingMessage(msg, seal.pubkey);
      } catch {
        console.log('[WebRTC] Failed to parse seal content');
      }
    } else {
      console.log('[WebRTC] Failed to unwrap event - decryption failed');
    }
  }
}
