import type { SignalingMessage } from '@hashtree/mesh';
import { type Event as NostrEvent, type VerifiedEvent } from 'nostr-tools';
import type { SimplePool } from 'nostr-tools/pool';
export declare const SIGNALING_KIND = 25050;
export declare const HELLO_TAG = "hello";
export declare const MAX_EVENT_AGE_SEC = 30;
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
export interface CreateAuthenticatedNip44GiftWrapOptions<TEvent extends SignalingEventLike = SignalingEventLike> extends CreateNip44GiftWrapOptions {
    senderPubkey: string;
    signEvent: (template: SignalingTemplate) => Promise<TEvent>;
    encrypt: (recipientPubkey: string, plaintext: string) => Promise<string> | string;
}
export type GiftCiphertextDecryptor = (senderPubkey: string, ciphertext: string) => string | Promise<string>;
export declare function createSignalingFilters(myPubkey: string, nowMs?: number, maxEventAgeSec?: number): SignalingFilters;
export declare function sendSignalingMessage<TEvent extends SignalingEventLike>({ msg, recipientPubkey, signEvent, giftWrap, publish, nowMs, }: SendSignalingMessageOptions<TEvent>): Promise<void>;
export declare function createSimplePoolSignalingSender<TEvent extends NostrEvent>({ signEvent, giftWrap, publishMode, publishMaxWaitMs, nowMs, }: CreateSimplePoolSignalingSenderOptions<TEvent>): ({ signalPool, relayUrls }: {
    signalPool: SimplePool;
    relayUrls: string[];
}) => (msg: SignalingMessage, recipientPubkey?: string) => Promise<void>;
export declare function createSecretKeyEventSigner(secretKey: Uint8Array): (template: SignalingTemplate) => Promise<VerifiedEvent>;
export declare function createSecretKeyNip44GiftWrap<TEvent extends NostrEvent = VerifiedEvent>(senderSecretKey: Uint8Array, options?: CreateNip44GiftWrapOptions): (innerEvent: SignalingInnerEvent, recipientPubkey: string) => Promise<TEvent>;
export declare function createAuthenticatedNip44GiftWrap<TEvent extends SignalingEventLike = SignalingEventLike>({ senderPubkey, signEvent, encrypt, nowMs, expirationSec, }: CreateAuthenticatedNip44GiftWrapOptions<TEvent>): (innerEvent: SignalingInnerEvent, recipientPubkey: string) => Promise<TEvent>;
export declare function createNip44GiftWrap<TEvent extends NostrEvent = VerifiedEvent>(senderPubkey: string, options?: CreateNip44GiftWrapOptions): (innerEvent: SignalingInnerEvent, recipientPubkey: string) => Promise<TEvent>;
export declare function createDecryptingGiftUnwrapper(decrypt: GiftCiphertextDecryptor): (event: SignalingEventLike) => Promise<GiftSeal | null>;
export declare function createSecretKeyGiftUnwrapper(secretKey: Uint8Array): (event: SignalingEventLike) => Promise<GiftSeal | null>;
export declare function decodeSignalingEvent<TEvent extends SignalingEventLike>({ event, giftUnwrap, nowMs, maxEventAgeSec, }: DecodeSignalingEventOptions<TEvent>): Promise<DecodedSignalingEvent | null>;
export {};
//# sourceMappingURL=signaling.d.ts.map