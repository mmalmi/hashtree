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

/**
 * Tree visibility levels
 */
export type TreeVisibility = 'public' | 'link-visible' | 'private';

/**
 * Generate a random 32-byte link key for link-visible trees
 */
export function generateLinkKey(): Uint8Array {
  return crypto.getRandomValues(new Uint8Array(32));
}

/**
 * Compute keyId from link key (first 8 bytes of SHA-256 hash)
 * Used to identify which link key was used without revealing the key
 */
export async function computeKeyId(linkKey: Uint8Array): Promise<Uint8Array> {
  const hash = await sha256(linkKey);
  return hash.slice(0, 8);
}

/**
 * XOR two 32-byte arrays
 * Used for encrypting/decrypting CHK keys with link keys.
 * XOR with random key is one-time pad (information-theoretically secure).
 */
function xor32(a: Uint8Array, b: Uint8Array): Uint8Array {
  const result = new Uint8Array(32);
  for (let i = 0; i < 32; i++) {
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

/**
 * Encrypt a CHK key for link-visible visibility using XOR (one-time pad)
 * @param chkKey - The CHK key to encrypt (32 bytes)
 * @param linkKey - The link decryption key (32 bytes)
 * @returns Encrypted key (32 bytes) - XOR of chkKey and linkKey
 */
export function encryptKeyForLink(chkKey: Uint8Array, linkKey: Uint8Array): Uint8Array {
  return xor32(chkKey, linkKey);
}

/**
 * Decrypt a CHK key for link-visible visibility
 * Supports both new XOR format (32 bytes) and legacy AES-GCM format (60 bytes)
 * @param encryptedKey - Encrypted key (32 bytes for XOR, 60 bytes for AES-GCM)
 * @param linkKey - The link decryption key (32 bytes)
 * @returns Decrypted CHK key (32 bytes), or null if decryption fails
 */
export async function decryptKeyFromLink(encryptedKey: Uint8Array, linkKey: Uint8Array): Promise<Uint8Array | null> {
  if (linkKey.length !== 32) {
    return null;
  }

  // New XOR format: 32 bytes
  if (encryptedKey.length === 32) {
    return xor32(encryptedKey, linkKey);
  }

  // Legacy AES-GCM format: 60 bytes (12 nonce + 32 ciphertext + 16 tag)
  if (encryptedKey.length === 60) {
    return decryptAesGcm(encryptedKey, linkKey);
  }

  return null;
}

/**
 * Hex string versions of the encryption functions for convenience
 */
export const hex = {
  generateLinkKey(): string {
    return toHex(generateLinkKey());
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
