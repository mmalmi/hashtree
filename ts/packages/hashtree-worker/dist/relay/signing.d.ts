/**
 * Worker Signing & Encryption
 *
 * Provides signing, encryption, and gift wrap functions.
 * Uses nsec directly when available, delegates to main thread otherwise.
 */
import type { EventTemplate } from 'nostr-tools';
import { type GiftSeal } from '../p2p/signaling.js';
import type { SignedEvent } from './protocol';
export declare function setResponseSender(fn: (msg: unknown) => void): void;
/**
 * Sign an event with user's real identity.
 * - For nsec login: signs directly with secret key
 * - For extension login: delegates to main thread via NIP-07
 */
export declare function signEvent(template: EventTemplate): Promise<SignedEvent>;
/**
 * Synchronous sign (only works with nsec, falls back to ephemeral)
 */
export declare function signEventSync(template: EventTemplate): SignedEvent;
/**
 * Encrypt plaintext for a recipient using NIP-44
 */
export declare function encrypt(recipientPubkey: string, plaintext: string): Promise<string>;
/**
 * Decrypt ciphertext from a sender using NIP-44
 */
export declare function decrypt(senderPubkey: string, ciphertext: string): Promise<string>;
/**
 * Gift wrap an event for private delivery.
 */
export declare function giftWrap(innerEvent: {
    kind: number;
    content: string;
    tags: string[][];
}, recipientPubkey: string): Promise<SignedEvent>;
/**
 * Unwrap a gift wrapped event.
 */
export declare function giftUnwrap(event: SignedEvent): Promise<GiftSeal | null>;
export declare function handleSignedResponse(id: string, event?: SignedEvent, error?: string): void;
export declare function handleEncryptedResponse(id: string, ciphertext?: string, error?: string): void;
export declare function handleDecryptedResponse(id: string, plaintext?: string, error?: string): void;
//# sourceMappingURL=signing.d.ts.map