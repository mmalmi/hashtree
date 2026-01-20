/**
 * Worker Signing & Encryption
 *
 * Provides signing, encryption, and gift wrap functions.
 * Uses nsec directly when available, delegates to main thread otherwise.
 */

import { generateSecretKey, finalizeEvent, nip44 } from 'nostr-tools';
import type { EventTemplate } from 'nostr-tools';
import { getSecretKey, getPubkey, getEphemeralSecretKey } from './identity';
import type { SignedEvent, UnsignedEvent } from './protocol';

// Pending NIP-07 requests (waiting for main thread)
const pendingSignRequests = new Map<string, (event: SignedEvent | null, error?: string) => void>();
const pendingEncryptRequests = new Map<string, (ciphertext: string | null, error?: string) => void>();
const pendingDecryptRequests = new Map<string, (plaintext: string | null, error?: string) => void>();

// Response sender (set by worker.ts)
let postResponse: ((msg: unknown) => void) | null = null;

export function setResponseSender(fn: (msg: unknown) => void) {
  postResponse = fn;
}

function bytesToHex(bytes: Uint8Array): string {
  let hex = '';
  for (const byte of bytes) {
    hex += byte.toString(16).padStart(2, '0');
  }
  return hex;
}

function normalizeTagValue(value: unknown): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean' || typeof value === 'bigint') {
    return String(value);
  }
  if (value == null) return '';
  if (value instanceof Uint8Array) return bytesToHex(value);
  if (ArrayBuffer.isView(value)) {
    return bytesToHex(new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
  }
  if (value instanceof ArrayBuffer) return bytesToHex(new Uint8Array(value));
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function sanitizeTags(tags: unknown): string[][] {
  if (!Array.isArray(tags)) return [];

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
    if (needsSanitize) break;
  }

  if (!needsSanitize) return tags as string[][];

  const sanitized: string[][] = [];
  for (const tag of tags) {
    if (!Array.isArray(tag)) continue;
    const normalized: string[] = [];
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
export async function signEvent(template: EventTemplate): Promise<SignedEvent> {
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
  } else {
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
export function signEventSync(template: EventTemplate): SignedEvent {
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
export async function encrypt(recipientPubkey: string, plaintext: string): Promise<string> {
  const secretKey = getSecretKey();
  if (secretKey) {
    const conversationKey = nip44.v2.utils.getConversationKey(secretKey, recipientPubkey);
    return nip44.v2.encrypt(plaintext, conversationKey);
  } else {
    return requestEncrypt(recipientPubkey, plaintext);
  }
}

/**
 * Decrypt ciphertext from a sender using NIP-44
 */
export async function decrypt(senderPubkey: string, ciphertext: string): Promise<string> {
  const secretKey = getSecretKey();
  if (secretKey) {
    const conversationKey = nip44.v2.utils.getConversationKey(secretKey, senderPubkey);
    return nip44.v2.decrypt(ciphertext, conversationKey);
  } else {
    return requestDecrypt(senderPubkey, ciphertext);
  }
}

// ============================================================================
// Gift Wrap (NIP-17 style private messaging)
// ============================================================================

interface Seal {
  pubkey: string;
  kind: number;
  content: string;
  tags: string[][];
}

/**
 * Gift wrap an event for private delivery.
 */
export async function giftWrap(
  innerEvent: { kind: number; content: string; tags: string[][] },
  recipientPubkey: string
): Promise<SignedEvent> {
  const myPubkey = getPubkey();
  if (!myPubkey) throw new Error('No pubkey available');

  const seal: Seal = {
    pubkey: myPubkey,
    kind: innerEvent.kind,
    content: innerEvent.content,
    tags: innerEvent.tags,
  };

  // Generate ephemeral keypair for the wrapper
  const ephemeralSk = generateSecretKey();

  // Encrypt the seal for the recipient
  const conversationKey = nip44.v2.utils.getConversationKey(ephemeralSk, recipientPubkey);
  const encryptedContent = nip44.v2.encrypt(JSON.stringify(seal), conversationKey);

  const createdAt = Math.floor(Date.now() / 1000);
  const expiration = createdAt + 5 * 60;

  const event = finalizeEvent({
    kind: 25050,
    created_at: createdAt,
    tags: [
      ['p', recipientPubkey],
      ['expiration', expiration.toString()],
    ],
    content: encryptedContent,
  }, ephemeralSk);

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

/**
 * Unwrap a gift wrapped event.
 */
export async function giftUnwrap(event: SignedEvent): Promise<Seal | null> {
  try {
    const decrypted = await decrypt(event.pubkey, event.content);
    return JSON.parse(decrypted) as Seal;
  } catch {
    return null;
  }
}

// ============================================================================
// NIP-07 Delegation (for extension login)
// ============================================================================

async function requestSign(event: UnsignedEvent): Promise<SignedEvent> {
  const id = `sign_${Date.now()}_${Math.random().toString(36).slice(2)}`;

  return new Promise((resolve, reject) => {
    pendingSignRequests.set(id, (signed, error) => {
      if (error) reject(new Error(error));
      else if (signed) resolve(signed);
      else reject(new Error('Signing failed'));
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

async function requestEncrypt(pubkey: string, plaintext: string): Promise<string> {
  const id = `enc_${Date.now()}_${Math.random().toString(36).slice(2)}`;

  return new Promise((resolve, reject) => {
    pendingEncryptRequests.set(id, (ciphertext, error) => {
      if (error) reject(new Error(error));
      else if (ciphertext) resolve(ciphertext);
      else reject(new Error('Encryption failed'));
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

async function requestDecrypt(pubkey: string, ciphertext: string): Promise<string> {
  const id = `dec_${Date.now()}_${Math.random().toString(36).slice(2)}`;

  return new Promise((resolve, reject) => {
    pendingDecryptRequests.set(id, (plaintext, error) => {
      if (error) reject(new Error(error));
      else if (plaintext) resolve(plaintext);
      else reject(new Error('Decryption failed'));
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

export function handleSignedResponse(id: string, event?: SignedEvent, error?: string) {
  const resolver = pendingSignRequests.get(id);
  if (resolver) {
    pendingSignRequests.delete(id);
    resolver(event || null, error);
  }
}

export function handleEncryptedResponse(id: string, ciphertext?: string, error?: string) {
  const resolver = pendingEncryptRequests.get(id);
  if (resolver) {
    pendingEncryptRequests.delete(id);
    resolver(ciphertext || null, error);
  }
}

export function handleDecryptedResponse(id: string, plaintext?: string, error?: string) {
  const resolver = pendingDecryptRequests.get(id);
  if (resolver) {
    pendingDecryptRequests.delete(id);
    resolver(plaintext || null, error);
  }
}
