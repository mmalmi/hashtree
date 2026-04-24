import type { SignalingMessage } from '@hashtree/mesh';
import {
  finalizeEvent,
  generateSecretKey,
  getEventHash,
  getPublicKey,
  nip59,
  nip44,
  verifyEvent,
  type Event as NostrEvent,
  type VerifiedEvent,
} from 'nostr-tools';
import type { SimplePool } from 'nostr-tools/pool';

type DirectedSignalingMessage = Exclude<SignalingMessage, { type: 'hello' }>;
type HelloSignalingMessage = Extract<SignalingMessage, { type: 'hello' }> & {
  hashGet?: boolean;
};

export const SIGNALING_KIND = 25050;
export const HELLO_TAG = 'hello';
export const MAX_EVENT_AGE_SEC = 30;
const HELLO_EXPIRATION_SEC = 5 * 60;
const NIP59_SEAL_KIND = 13;

export interface SignalingEventLike {
  pubkey: string;
  kind?: number;
  created_at?: number;
  tags: string[][];
  content: string;
  id?: string;
  sig?: string;
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

export type SimplePoolPublishMode = 'require-one' | 'best-effort';

export interface CreateSimplePoolSignalingSenderOptions<TEvent extends NostrEvent> {
  signEvent: (template: SignalingTemplate) => Promise<TEvent>;
  giftWrap: (innerEvent: SignalingInnerEvent, recipientPubkey: string) => Promise<TEvent>;
  publishMode?: SimplePoolPublishMode;
  publishMaxWaitMs?: number;
  nowMs?: () => number;
}

export interface CreateNip44GiftWrapOptions {
  expirationSec?: number;
  nowMs?: () => number;
}

export interface CreateAuthenticatedNip44GiftWrapOptions<TEvent extends SignalingEventLike = SignalingEventLike>
  extends CreateNip44GiftWrapOptions {
  senderPubkey: string;
  signEvent: (template: SignalingTemplate) => Promise<TEvent>;
  encrypt: (recipientPubkey: string, plaintext: string) => Promise<string> | string;
}

export type GiftCiphertextDecryptor = (
  senderPubkey: string,
  ciphertext: string,
) => string | Promise<string>;

type NostrEventLike = SignalingEventLike & {
  kind: number;
  created_at: number;
  id: string;
  sig: string;
};

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

function isSignalingKind(kind: number | undefined): boolean {
  return kind === undefined || kind === SIGNALING_KIND;
}

function hasCompleteEventFields(value: unknown): value is NostrEventLike {
  if (!value || typeof value !== 'object') return false;
  const event = value as Record<string, unknown>;
  return (
    typeof event.id === 'string' &&
    typeof event.pubkey === 'string' &&
    typeof event.kind === 'number' &&
    typeof event.created_at === 'number' &&
    Array.isArray(event.tags) &&
    typeof event.content === 'string' &&
    typeof event.sig === 'string'
  );
}

function verifyNostrEvent(value: unknown): NostrEventLike | null {
  if (!hasCompleteEventFields(value)) return null;
  const eventForVerification: NostrEvent = {
    id: value.id,
    pubkey: value.pubkey,
    kind: value.kind,
    created_at: value.created_at,
    tags: value.tags,
    content: value.content,
    sig: value.sig,
  };
  if (!verifyEvent(eventForVerification)) return null;
  return value;
}

function signedEventToSeal(value: unknown): GiftSeal | null {
  const event = verifyNostrEvent(value);
  if (!event || !isSignalingKind(event.kind)) return null;
  return {
    pubkey: event.pubkey,
    kind: event.kind,
    content: event.content,
    tags: event.tags,
  };
}

function parseJson(value: string): unknown {
  return JSON.parse(value) as unknown;
}

function createRumor(innerEvent: SignalingInnerEvent, senderPubkey: string, createdAt: number) {
  const rumor = {
    kind: innerEvent.kind,
    content: innerEvent.content,
    tags: innerEvent.tags,
    created_at: createdAt,
    pubkey: senderPubkey,
  };
  return {
    ...rumor,
    id: getEventHash(rumor),
  };
}

function createOuterGiftWrap<TEvent extends NostrEvent = VerifiedEvent>(
  encryptedContent: string,
  recipientPubkey: string,
  createdAt: number,
  expirationSec: number,
  ephemeralSecretKey: Uint8Array,
): TEvent {
  return finalizeEvent({
    kind: SIGNALING_KIND,
    created_at: createdAt,
    tags: [
      ['p', recipientPubkey],
      ['expiration', String(createdAt + expirationSec)],
    ],
    content: encryptedContent,
  }, ephemeralSecretKey) as TEvent;
}

function encryptOuterGiftWrapPayload(payload: unknown, recipientPubkey: string, ephemeralSecretKey: Uint8Array): string {
  const conversationKey = nip44.v2.utils.getConversationKey(ephemeralSecretKey, recipientPubkey);
  return nip44.v2.encrypt(JSON.stringify(payload), conversationKey);
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

  return null;
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

export function createSimplePoolSignalingSender<TEvent extends NostrEvent>({
  signEvent,
  giftWrap,
  publishMode = 'require-one',
  publishMaxWaitMs,
  nowMs,
}: CreateSimplePoolSignalingSenderOptions<TEvent>) {
  return ({ signalPool, relayUrls }: { signalPool: SimplePool; relayUrls: string[] }) => {
    return async (msg: SignalingMessage, recipientPubkey?: string): Promise<void> => {
      await sendSignalingMessage({
        msg,
        recipientPubkey,
        signEvent,
        giftWrap,
        publish: async (event) => {
          const publishPromises = signalPool.publish(relayUrls, event);

          if (publishMode === 'best-effort') {
            await Promise.allSettled(publishPromises);
            return;
          }

          const publishResult = Promise.any(publishPromises);
          if (publishMaxWaitMs === undefined) {
            await publishResult;
            return;
          }

          await Promise.race([
            publishResult,
            new Promise<never>((_, reject) => {
              setTimeout(() => reject(new Error('Timed out publishing signaling event')), publishMaxWaitMs);
            }),
          ]);
        },
        nowMs,
      });
    };
  };
}

export function createSecretKeyEventSigner(secretKey: Uint8Array) {
  return async (template: SignalingTemplate): Promise<VerifiedEvent> => finalizeEvent(template, secretKey);
}

export function createSecretKeyNip44GiftWrap<TEvent extends NostrEvent = VerifiedEvent>(
  senderSecretKey: Uint8Array,
  options: CreateNip44GiftWrapOptions = {},
) {
  const nowMs = options.nowMs ?? (() => Date.now());
  const expirationSec = options.expirationSec ?? HELLO_EXPIRATION_SEC;

  return async (innerEvent: SignalingInnerEvent, recipientPubkey: string): Promise<TEvent> => {
    const senderPubkey = getPublicKey(senderSecretKey);
    const rumor = nip59.createRumor({
      kind: innerEvent.kind,
      content: innerEvent.content,
      tags: innerEvent.tags,
    }, senderSecretKey);
    const seal = nip59.createSeal(rumor, senderSecretKey, recipientPubkey);
    const legacyReadableSeal = {
      pubkey: senderPubkey,
      kind: innerEvent.kind,
      content: innerEvent.content,
      tags: innerEvent.tags,
      seal,
    };

    const ephemeralSecretKey = generateSecretKey();
    const createdAt = Math.floor(nowMs() / 1000);
    const encryptedContent = encryptOuterGiftWrapPayload(
      legacyReadableSeal,
      recipientPubkey,
      ephemeralSecretKey,
    );

    return createOuterGiftWrap<TEvent>(
      encryptedContent,
      recipientPubkey,
      createdAt,
      expirationSec,
      ephemeralSecretKey,
    );
  };
}

export function createAuthenticatedNip44GiftWrap<TEvent extends SignalingEventLike = SignalingEventLike>({
  senderPubkey,
  signEvent,
  encrypt,
  nowMs = () => Date.now(),
  expirationSec = HELLO_EXPIRATION_SEC,
}: CreateAuthenticatedNip44GiftWrapOptions<TEvent>) {
  return async (innerEvent: SignalingInnerEvent, recipientPubkey: string): Promise<TEvent> => {
    const createdAt = Math.floor(nowMs() / 1000);
    const rumor = createRumor(innerEvent, senderPubkey, createdAt);
    const seal = await signEvent({
      kind: NIP59_SEAL_KIND,
      created_at: createdAt,
      tags: [],
      content: await encrypt(recipientPubkey, JSON.stringify(rumor)),
    });
    const legacyReadableSeal = {
      pubkey: senderPubkey,
      kind: innerEvent.kind,
      content: innerEvent.content,
      tags: innerEvent.tags,
      seal,
    };

    const ephemeralSecretKey = generateSecretKey();
    const encryptedContent = encryptOuterGiftWrapPayload(
      legacyReadableSeal,
      recipientPubkey,
      ephemeralSecretKey,
    );

    return createOuterGiftWrap<NostrEvent>(
      encryptedContent,
      recipientPubkey,
      createdAt,
      expirationSec,
      ephemeralSecretKey,
    ) as TEvent;
  };
}

export function createNip44GiftWrap<TEvent extends NostrEvent = VerifiedEvent>(
  senderPubkey: string,
  options: CreateNip44GiftWrapOptions = {},
) {
  const nowMs = options.nowMs ?? (() => Date.now());
  const expirationSec = options.expirationSec ?? HELLO_EXPIRATION_SEC;

  return async (innerEvent: SignalingInnerEvent, recipientPubkey: string): Promise<TEvent> => {
    const seal: GiftSeal = {
      pubkey: senderPubkey,
      kind: innerEvent.kind,
      content: innerEvent.content,
      tags: innerEvent.tags,
    };

    const ephemeralSecretKey = generateSecretKey();
    const createdAt = Math.floor(nowMs() / 1000);
    const encryptedContent = encryptOuterGiftWrapPayload(seal, recipientPubkey, ephemeralSecretKey);

    return createOuterGiftWrap<TEvent>(
      encryptedContent,
      recipientPubkey,
      createdAt,
      expirationSec,
      ephemeralSecretKey,
    );
  };
}

export function createDecryptingGiftUnwrapper(
  decrypt: GiftCiphertextDecryptor,
) {
  return async (event: SignalingEventLike): Promise<GiftSeal | null> => {
    try {
      const content = await decrypt(event.pubkey, event.content);
      const unwrapped = parseJson(content);
      const directSignedEvent = signedEventToSeal(unwrapped);
      if (directSignedEvent) return directSignedEvent;
      if (!unwrapped || typeof unwrapped !== 'object') return null;

      const seal = unwrapped as Record<string, unknown>;
      if ('event' in seal) {
        return signedEventToSeal(seal.event);
      }

      if ('seal' in seal) {
        const sealEvent = verifyNostrEvent(seal.seal);
        if (!sealEvent || sealEvent.kind !== NIP59_SEAL_KIND) return null;

        const rumorPlaintext = await decrypt(sealEvent.pubkey, sealEvent.content);
        const rumor = parseJson(rumorPlaintext);
        if (!rumor || typeof rumor !== 'object') return null;
        const rumorEvent = rumor as Record<string, unknown>;
        if (
          rumorEvent.pubkey !== sealEvent.pubkey ||
          typeof rumorEvent.kind !== 'number' ||
          typeof rumorEvent.content !== 'string' ||
          !Array.isArray(rumorEvent.tags)
        ) {
          return null;
        }

        return {
          pubkey: sealEvent.pubkey,
          kind: rumorEvent.kind,
          content: rumorEvent.content,
          tags: rumorEvent.tags as string[][],
        };
      }

      if (
        typeof seal.pubkey === 'string' &&
        typeof seal.kind === 'number' &&
        typeof seal.content === 'string' &&
        Array.isArray(seal.tags)
      ) {
        return {
          pubkey: seal.pubkey,
          kind: seal.kind,
          content: seal.content,
          tags: seal.tags as string[][],
        };
      }

      return null;
    } catch {
      return null;
    }
  };
}

export function createSecretKeyGiftUnwrapper(secretKey: Uint8Array) {
  return createDecryptingGiftUnwrapper((senderPubkey, ciphertext) => {
    const conversationKey = nip44.v2.utils.getConversationKey(secretKey, senderPubkey);
    return nip44.v2.decrypt(ciphertext, conversationKey);
  });
}

export async function decodeSignalingEvent<TEvent extends SignalingEventLike>({
  event,
  giftUnwrap,
  nowMs = () => Date.now(),
  maxEventAgeSec = MAX_EVENT_AGE_SEC,
}: DecodeSignalingEventOptions<TEvent>): Promise<DecodedSignalingEvent | null> {
  if (hasCompleteEventFields(event)) {
    if (!verifyNostrEvent(event) || event.kind !== SIGNALING_KIND) {
      return null;
    }
  } else if (!isSignalingKind(event.kind)) {
    return null;
  }

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
