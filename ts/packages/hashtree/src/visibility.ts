/**
 * Tree visibility and encryption utilities
 *
 * ## Encryption Modes
 *
 * - **Unencrypted**: No CHK, just hash - anyone with hash can read
 * - **Public**: CHK encrypted, ["key", "<hex>"] in event - anyone can decrypt
 * - **Link-visible**: CHK + XOR mask, ["encryptedKey", XOR(key,secret)] - need #k=<secret> URL
 * - **Private**: CHK + NIP-44 to self, ["selfEncryptedKey", "..."] - author only
 *
 * Default is Public (CHK encrypted, key in nostr event).
 */

import { sha256 } from './hash.js';
import { toHex, fromHex } from './types.js';
import { encryptionMode } from './encryption.js';

/**
 * Tree visibility levels
 */
export type TreeVisibility = 'public' | 'link-visible' | 'private';

/**
 * Generate a random 32-byte link key for link-visible trees
 */
export function generateLinkKey(length: 32 | 33 = 33): Uint8Array {
  return crypto.getRandomValues(new Uint8Array(length));
}

/**
 * Compute keyId from link key (first 8 bytes of SHA-256 hash)
 * Used to identify which link key was used without revealing the key
 */
export async function computeKeyId(linkKey: Uint8Array): Promise<Uint8Array> {
  const hash = await sha256(linkKey);
  return hash.slice(0, 8);
}

/** Mask or recover the encoded object key using its link secret. */
function xorKey(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (a.length !== b.length || (a.length !== 32 && a.length !== 33)) {
    throw new Error('Key and link secret must have the same length (32 or 33 bytes)');
  }
  const result = new Uint8Array(a.length);
  for (let i = 0; i < a.length; i++) {
    result[i] = a[i] ^ b[i];
  }
  return result;
}

/**
 * Legacy AES-GCM decryption for backward compatibility
 * Old format: 12-byte nonce + ciphertext + 16-byte tag = 60 bytes total
 */
async function decryptAesGcm(encryptedKey: Uint8Array, linkKey: Uint8Array): Promise<Uint8Array | null> {
  try {
    const keyBuffer = new ArrayBuffer(linkKey.length);
    new Uint8Array(keyBuffer).set(linkKey);

    const nonce = encryptedKey.slice(0, 12);
    const ciphertext = encryptedKey.slice(12);
    const ciphertextBuffer = new ArrayBuffer(ciphertext.length);
    new Uint8Array(ciphertextBuffer).set(ciphertext);

    const cryptoKey = await crypto.subtle.importKey(
      'raw', keyBuffer, { name: 'AES-GCM' }, false, ['decrypt']
    );

    const plaintext = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: nonce }, cryptoKey, ciphertextBuffer
    );

    return new Uint8Array(plaintext);
  } catch {
    return null;
  }
}

/** Mask a root key, preserving existing 32-byte sharing links for CHK roots. */
export function encryptKeyForLink(chkKey: Uint8Array, linkKey: Uint8Array): Uint8Array {
  const suite = encryptionMode(chkKey);
  if (suite === 'chk-v1' && linkKey.length === 32) return xorKey(chkKey.subarray(1), linkKey);
  if (suite === 'legacy-chk' && linkKey.length === 33) return xorKey(Uint8Array.of(1, ...chkKey), linkKey);
  return xorKey(chkKey, linkKey);
}

/** Recover the object key from a masked key or a legacy 60-byte AES-GCM envelope. */
export async function decryptKeyFromLink(encryptedKey: Uint8Array, linkKey: Uint8Array): Promise<Uint8Array | null> {
  if (linkKey.length !== 32 && linkKey.length !== 33) {
    return null;
  }

  // The opaque link secret and masked key have matching lengths.
  if (encryptedKey.length === linkKey.length) {
    const key = xorKey(encryptedKey, linkKey);
    try { encryptionMode(key); } catch { return null; }
    return key;
  }

  // Legacy AES-GCM format: 60 bytes (12 nonce + 32 ciphertext + 16 tag)
  if (encryptedKey.length === 60 && linkKey.length === 32) {
    return decryptAesGcm(encryptedKey, linkKey);
  }

  return null;
}

/**
 * Hex string versions of the encryption functions for convenience
 */
export const hex = {
  generateLinkKey(length: 32 | 33 = 33): string {
    return toHex(generateLinkKey(length));
  },

  async computeKeyId(linkKeyHex: string): Promise<string> {
    const keyId = await computeKeyId(fromHex(linkKeyHex));
    return toHex(keyId);
  },

  encryptKeyForLink(chkKeyHex: string, linkKeyHex: string): string {
    const encrypted = encryptKeyForLink(fromHex(chkKeyHex), fromHex(linkKeyHex));
    return toHex(encrypted);
  },

  async decryptKeyFromLink(encryptedKeyHex: string, linkKeyHex: string): Promise<string | null> {
    const decrypted = await decryptKeyFromLink(fromHex(encryptedKeyHex), fromHex(linkKeyHex));
    return decrypted ? toHex(decrypted) : null;
  },
};
