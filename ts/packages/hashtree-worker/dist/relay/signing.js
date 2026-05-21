// @ts-nocheck
/**
 * Worker Signing & Encryption
 *
 * Provides signing, encryption, and gift wrap functions.
 * Uses nsec directly when available, delegates to main thread otherwise.
 */
import { finalizeEvent, nip44 } from 'nostr-tools';
import { createAuthenticatedNip44GiftWrap, createDecryptingGiftUnwrapper, } from '../p2p/signaling.js';
import { getSecretKey, getPubkey, getEphemeralSecretKey } from './identity';
// Pending NIP-07 requests (waiting for main thread)
const pendingSignRequests = new Map();
const pendingEncryptRequests = new Map();
const pendingDecryptRequests = new Map();
// Response sender (set by worker.ts)
let postResponse = null;
export function setResponseSender(fn) {
    postResponse = fn;
}
function bytesToHex(bytes) {
    let hex = '';
    for (const byte of bytes) {
        hex += byte.toString(16).padStart(2, '0');
    }
    return hex;
}
function normalizeTagValue(value) {
    if (typeof value === 'string')
        return value;
    if (typeof value === 'number' || typeof value === 'boolean' || typeof value === 'bigint') {
        return String(value);
    }
    if (value == null)
        return '';
    if (value instanceof Uint8Array)
        return bytesToHex(value);
    if (ArrayBuffer.isView(value)) {
        return bytesToHex(new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
    }
    if (value instanceof ArrayBuffer)
        return bytesToHex(new Uint8Array(value));
    try {
        return JSON.stringify(value);
    }
    catch {
        return String(value);
    }
}
function sanitizeTags(tags) {
    if (!Array.isArray(tags))
        return [];
    let needsSanitize = false;
    for (const tag of tags) {
        if (!Array.isArray(tag)) {
            needsSanitize = true;
            break;
        }
        for (const value of tag) {
            if (typeof value !== 'string') {
                needsSanitize = true;
                break;
            }
        }
        if (needsSanitize)
            break;
    }
    if (!needsSanitize)
        return tags;
    const sanitized = [];
    for (const tag of tags) {
        if (!Array.isArray(tag))
            continue;
        const normalized = [];
        for (const value of tag) {
            normalized.push(normalizeTagValue(value));
        }
        sanitized.push(normalized);
    }
    return sanitized;
}
// ============================================================================
// Signing
// ============================================================================
/**
 * Sign an event with user's real identity.
 * - For nsec login: signs directly with secret key
 * - For extension login: delegates to main thread via NIP-07
 */
export async function signEvent(template) {
    template.tags = sanitizeTags(template.tags ?? []);
    const secretKey = getSecretKey();
    if (secretKey) {
        const event = finalizeEvent(template, secretKey);
        return {
            id: event.id,
            pubkey: event.pubkey,
            kind: event.kind,
            content: event.content,
            tags: event.tags,
            created_at: event.created_at,
            sig: event.sig,
        };
    }
    else {
        return requestSign({
            kind: template.kind,
            created_at: template.created_at,
            content: template.content,
            tags: template.tags,
        });
    }
}
/**
 * Synchronous sign (only works with nsec, falls back to ephemeral)
 */
export function signEventSync(template) {
    template.tags = sanitizeTags(template.tags ?? []);
    const secretKey = getSecretKey() || getEphemeralSecretKey();
    if (!secretKey) {
        throw new Error('No signing key available');
    }
    const event = finalizeEvent(template, secretKey);
    return {
        id: event.id,
        pubkey: event.pubkey,
        kind: event.kind,
        content: event.content,
        tags: event.tags,
        created_at: event.created_at,
        sig: event.sig,
    };
}
// ============================================================================
// Encryption
// ============================================================================
/**
 * Encrypt plaintext for a recipient using NIP-44
 */
export async function encrypt(recipientPubkey, plaintext) {
    const secretKey = getSecretKey();
    if (secretKey) {
        const conversationKey = nip44.v2.utils.getConversationKey(secretKey, recipientPubkey);
        return nip44.v2.encrypt(plaintext, conversationKey);
    }
    else {
        return requestEncrypt(recipientPubkey, plaintext);
    }
}
/**
 * Decrypt ciphertext from a sender using NIP-44
 */
export async function decrypt(senderPubkey, ciphertext) {
    const secretKey = getSecretKey();
    if (secretKey) {
        const conversationKey = nip44.v2.utils.getConversationKey(secretKey, senderPubkey);
        return nip44.v2.decrypt(ciphertext, conversationKey);
    }
    else {
        return requestDecrypt(senderPubkey, ciphertext);
    }
}
// ============================================================================
// Gift Wrap (authenticated NIP-59 seal inside hashtree signaling envelope)
// ============================================================================
/**
 * Gift wrap an event for private delivery.
 */
export async function giftWrap(innerEvent, recipientPubkey) {
    const myPubkey = getPubkey();
    if (!myPubkey)
        throw new Error('No pubkey available');
    const wrap = createAuthenticatedNip44GiftWrap({
        senderPubkey: myPubkey,
        signEvent,
        encrypt,
    });
    return wrap(innerEvent, recipientPubkey);
}
/**
 * Unwrap a gift wrapped event.
 */
export async function giftUnwrap(event) {
    return createDecryptingGiftUnwrapper(decrypt)(event);
}
// ============================================================================
// NIP-07 Delegation (for extension login)
// ============================================================================
async function requestSign(event) {
    const id = `sign_${Date.now()}_${Math.random().toString(36).slice(2)}`;
    return new Promise((resolve, reject) => {
        pendingSignRequests.set(id, (signed, error) => {
            if (error)
                reject(new Error(error));
            else if (signed)
                resolve(signed);
            else
                reject(new Error('Signing failed'));
        });
        postResponse?.({ type: 'signEvent', id, event });
        setTimeout(() => {
            if (pendingSignRequests.has(id)) {
                pendingSignRequests.delete(id);
                reject(new Error('Signing timeout'));
            }
        }, 60000);
    });
}
async function requestEncrypt(pubkey, plaintext) {
    const id = `enc_${Date.now()}_${Math.random().toString(36).slice(2)}`;
    return new Promise((resolve, reject) => {
        pendingEncryptRequests.set(id, (ciphertext, error) => {
            if (error)
                reject(new Error(error));
            else if (ciphertext)
                resolve(ciphertext);
            else
                reject(new Error('Encryption failed'));
        });
        postResponse?.({ type: 'nip44Encrypt', id, pubkey, plaintext });
        setTimeout(() => {
            if (pendingEncryptRequests.has(id)) {
                pendingEncryptRequests.delete(id);
                reject(new Error('Encryption timeout'));
            }
        }, 30000);
    });
}
async function requestDecrypt(pubkey, ciphertext) {
    const id = `dec_${Date.now()}_${Math.random().toString(36).slice(2)}`;
    return new Promise((resolve, reject) => {
        pendingDecryptRequests.set(id, (plaintext, error) => {
            if (error)
                reject(new Error(error));
            else if (plaintext)
                resolve(plaintext);
            else
                reject(new Error('Decryption failed'));
        });
        postResponse?.({ type: 'nip44Decrypt', id, pubkey, ciphertext });
        setTimeout(() => {
            if (pendingDecryptRequests.has(id)) {
                pendingDecryptRequests.delete(id);
                reject(new Error('Decryption timeout'));
            }
        }, 30000);
    });
}
// ============================================================================
// Response Handlers (called by worker.ts when main thread responds)
// ============================================================================
export function handleSignedResponse(id, event, error) {
    const resolver = pendingSignRequests.get(id);
    if (resolver) {
        pendingSignRequests.delete(id);
        resolver(event || null, error);
    }
}
export function handleEncryptedResponse(id, ciphertext, error) {
    const resolver = pendingEncryptRequests.get(id);
    if (resolver) {
        pendingEncryptRequests.delete(id);
        resolver(ciphertext || null, error);
    }
}
export function handleDecryptedResponse(id, plaintext, error) {
    const resolver = pendingDecryptRequests.get(id);
    if (resolver) {
        pendingDecryptRequests.delete(id);
        resolver(plaintext || null, error);
    }
}
//# sourceMappingURL=signing.js.map