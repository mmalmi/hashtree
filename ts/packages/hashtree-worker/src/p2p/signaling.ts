import type { SignalingMessage } from '@hashtree/nostr';

type DirectedSignalingMessage = Exclude<SignalingMessage, { type: 'hello' }>;
type HelloSignalingMessage = Extract<SignalingMessage, { type: 'hello' }> & {
  hashGet?: boolean;
};

export const SIGNALING_KIND = 25050;
export const HELLO_TAG = 'hello';
export const MAX_EVENT_AGE_SEC = 30;
const HELLO_EXPIRATION_SEC = 5 * 60;

export interface SignalingEventLike {
  pubkey: string;
  created_at?: number;
  tags: string[][];
  content: string;
}

export interface GiftSeal {
  pubkey: string;
  kind: number;
  content: string;
  tags: string[][];
}

export interface SignalingTemplate {
  kind: number;
  created_at: number;
  tags: string[][];
  content: string;
}

export interface SignalingInnerEvent {
  kind: number;
  content: string;
  tags: string[][];
}

export interface SignalingFilters {
  since: number;
  helloFilter: {
    kinds: number[];
    '#l': string[];
    since: number;
  };
  directedFilter: {
    kinds: number[];
    '#p': string[];
    since: number;
  };
}

interface SendSignalingMessageOptions<TEvent extends SignalingEventLike> {
  msg: SignalingMessage;
  recipientPubkey?: string;
  signEvent: (template: SignalingTemplate) => Promise<TEvent>;
  giftWrap: (innerEvent: SignalingInnerEvent, recipientPubkey: string) => Promise<TEvent>;
  publish: (event: TEvent) => Promise<void>;
  nowMs?: () => number;
}

interface DecodeSignalingEventOptions<TEvent extends SignalingEventLike> {
  event: TEvent;
  giftUnwrap: (event: TEvent) => Promise<GiftSeal | null>;
  nowMs?: () => number;
  maxEventAgeSec?: number;
}

export interface DecodedSignalingEvent {
  senderPubkey: string;
  message: SignalingMessage;
}

function getSince(nowMs: number, maxEventAgeSec: number): number {
  return Math.floor((nowMs - maxEventAgeSec * 1000) / 1000);
}

function isExpired(event: SignalingEventLike, nowSec: number, maxEventAgeSec: number): boolean {
  const createdAt = event.created_at ?? 0;
  if (nowSec - createdAt > maxEventAgeSec) {
    return true;
  }

  const expirationTag = event.tags.find((tag) => tag[0] === 'expiration');
  if (expirationTag?.[1]) {
    const expiration = Number.parseInt(expirationTag[1], 10);
    if (Number.isFinite(expiration) && expiration < nowSec) {
      return true;
    }
  }

  return false;
}

function normalizePeerEndpoint(value: string, senderPubkey: string): string {
  const trimmed = value.trim();
  if (!trimmed) return senderPubkey;
  return trimmed.includes(':') ? senderPubkey : trimmed;
}

function decodeHashGetTag(value: string | undefined): boolean {
  if (value === undefined) return true;
  return !['0', 'false', 'FALSE', 'no', 'NO'].includes(value);
}

function normalizeSignalingMessage(raw: unknown, senderPubkey: string): SignalingMessage | null {
  if (!raw || typeof raw !== 'object' || !('type' in raw)) return null;
  const msg = raw as Record<string, unknown>;
  if (typeof msg.type !== 'string') return null;

  if (typeof msg.peerId === 'string' && msg.type === 'hello') {
    return {
      ...(msg as unknown as HelloSignalingMessage),
      peerId: senderPubkey,
      hashGet: msg.hashGet !== false,
    } as HelloSignalingMessage;
  }

  if (
    'targetPeerId' in msg &&
    typeof msg.targetPeerId === 'string' &&
    typeof msg.peerId === 'string'
  ) {
    return {
      ...(msg as unknown as DirectedSignalingMessage),
      peerId: senderPubkey,
      targetPeerId: normalizePeerEndpoint(msg.targetPeerId, senderPubkey),
    };
  }

  if (!('recipient' in msg) || typeof msg.recipient !== 'string' || typeof msg.peerId !== 'string') {
    return null;
  }

  const senderPeerId = senderPubkey;
  const targetPeerId = normalizePeerEndpoint(msg.recipient, senderPubkey);

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

export function createSignalingFilters(
  myPubkey: string,
  nowMs = Date.now(),
  maxEventAgeSec = MAX_EVENT_AGE_SEC
): SignalingFilters {
  const since = getSince(nowMs, maxEventAgeSec);
  return {
    since,
    helloFilter: {
      kinds: [SIGNALING_KIND],
      '#l': [HELLO_TAG],
      since,
    },
    directedFilter: {
      kinds: [SIGNALING_KIND],
      '#p': [myPubkey],
      since,
    },
  };
}

export async function sendSignalingMessage<TEvent extends SignalingEventLike>({
  msg,
  recipientPubkey,
  signEvent,
  giftWrap,
  publish,
  nowMs = () => Date.now(),
}: SendSignalingMessageOptions<TEvent>): Promise<void> {
  if (recipientPubkey) {
    const wrappedEvent = await giftWrap(
      {
        kind: SIGNALING_KIND,
        content: JSON.stringify(msg),
        tags: [],
      },
      recipientPubkey
    );
    await publish(wrappedEvent);
    return;
  }

  const createdAt = Math.floor(nowMs() / 1000);
  const event = await signEvent({
    kind: SIGNALING_KIND,
    created_at: createdAt,
    tags: [
      ['l', HELLO_TAG],
      ['peerId', msg.peerId],
      ['hashGet', msg.type === 'hello' && (msg as HelloSignalingMessage).hashGet === false ? '0' : '1'],
      ['expiration', String(createdAt + HELLO_EXPIRATION_SEC)],
    ],
    content: '',
  });
  await publish(event);
}

export async function decodeSignalingEvent<TEvent extends SignalingEventLike>({
  event,
  giftUnwrap,
  nowMs = () => Date.now(),
  maxEventAgeSec = MAX_EVENT_AGE_SEC,
}: DecodeSignalingEventOptions<TEvent>): Promise<DecodedSignalingEvent | null> {
  const nowSec = nowMs() / 1000;
  if (isExpired(event, nowSec, maxEventAgeSec)) {
    return null;
  }

  const isHello = event.tags.some((tag) => tag[0] === 'l' && tag[1] === HELLO_TAG);
  if (isHello) {
    const peerIdTag = event.tags.find((tag) => tag[0] === 'peerId');
    if (!peerIdTag?.[1]) return null;
    const senderPeerId = normalizePeerEndpoint(event.pubkey, event.pubkey);
    if (normalizePeerEndpoint(peerIdTag[1], event.pubkey) !== senderPeerId) {
      return null;
    }
    return {
      senderPubkey: event.pubkey,
      message: {
        type: 'hello',
        peerId: senderPeerId,
        hashGet: decodeHashGetTag(event.tags.find((tag) => tag[0] === 'hashGet')?.[1]),
      } as HelloSignalingMessage,
    };
  }

  const seal = await giftUnwrap(event);
  if (!seal?.content) {
    return null;
  }

  try {
    const raw = JSON.parse(seal.content);
    const message = normalizeSignalingMessage(raw, seal.pubkey);
    if (!message) return null;
    return {
      senderPubkey: seal.pubkey,
      message,
    };
  } catch {
    return null;
  }
}
