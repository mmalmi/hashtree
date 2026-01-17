/**
 * Encryption utilities for HashTree
 *
 * CHK (Content Hash Key) encryption for deterministic encryption:
 * - key = SHA256(plaintext) - content hash becomes decryption key
 * - encryption_key = HKDF(key, salt="hashtree-chk", info="encryption-key")
 * - ciphertext = AES-256-GCM(encryption_key, zero_nonce, plaintext)
 *
 * Zero nonce is safe because CHK guarantees: same key = same content.
 * This enables deduplication: same content → same ciphertext → same hash.
 *
 * Format: [ciphertext][16-byte auth tag] (no IV needed with CHK)
 *
 * Also includes legacy functions with random IV for backward compatibility:
 * Format: [12-byte IV][ciphertext][16-byte auth tag]
 */

import { sha256 } from './hash.js';

/** 32-byte encryption key (256 bits) */
export type EncryptionKey = Uint8Array;

/** IV/Nonce size for AES-GCM */
const IV_SIZE = 12;

/** Auth tag size for AES-GCM */
const TAG_SIZE = 16;

/** Minimum encrypted data size: IV + tag (for legacy format) */
const MIN_ENCRYPTED_SIZE = IV_SIZE + TAG_SIZE;

/** Minimum CHK encrypted size: just tag (no IV) */
const MIN_CHK_ENCRYPTED_SIZE = TAG_SIZE;

/** HKDF salt for CHK key derivation */
const CHK_SALT = new TextEncoder().encode('hashtree-chk');

/** HKDF info for CHK key derivation */
const CHK_INFO = new TextEncoder().encode('encryption-key');

/** Generate a random 32-byte encryption key */
export function generateKey(): EncryptionKey {
  const key = new Uint8Array(32);
  crypto.getRandomValues(key);
  return key;
}

/**
 * Import raw key bytes as CryptoKey for AES-GCM
 */
async function importKey(key: EncryptionKey): Promise<CryptoKey> {
  // Copy to ensure we have a clean ArrayBuffer (not SharedArrayBuffer)
  const keyBuffer = new ArrayBuffer(key.length);
  new Uint8Array(keyBuffer).set(key);
  return crypto.subtle.importKey(
    'raw',
    keyBuffer,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt']
  );
}

/**
 * Derive AES key from content hash using HKDF
 * @param contentHash - 32-byte content hash (SHA256 of plaintext)
 * @returns 32-byte derived encryption key
 */
async function deriveKey(contentHash: EncryptionKey): Promise<CryptoKey> {
  // Import content hash as HKDF key material
  const keyBuffer = new ArrayBuffer(contentHash.length);
  new Uint8Array(keyBuffer).set(contentHash);

  const hkdfKey = await crypto.subtle.importKey(
    'raw',
    keyBuffer,
    { name: 'HKDF' },
    false,
    ['deriveKey']
  );

  // Derive AES-GCM key using HKDF
  return crypto.subtle.deriveKey(
    {
      name: 'HKDF',
      salt: CHK_SALT,
      info: CHK_INFO,
      hash: 'SHA-256',
    },
    hkdfKey,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt']
  );
}

/**
 * Compute content hash (SHA256) - this becomes the decryption key for CHK
 */
export async function contentHash(data: Uint8Array): Promise<EncryptionKey> {
  return sha256(data);
}

/**
 * CHK encrypt: derive key from content, encrypt with zero nonce
 *
 * Returns: (ciphertext with auth tag, content_hash as decryption key)
 *
 * Zero nonce is safe because CHK guarantees: same key = same content.
 * We never encrypt different content with the same key.
 *
 * The content_hash is both:
 * - The decryption key (store securely, share with authorized users)
 * - Enables dedup: same content → same ciphertext
 *
 * @param plaintext - Data to encrypt
 * @returns Object with encrypted data and content hash (decryption key)
 */
export async function encryptChk(plaintext: Uint8Array): Promise<{ ciphertext: Uint8Array; key: EncryptionKey }> {
  // 1. Compute content hash - this is the "key" we return
  const chash = await contentHash(plaintext);

  // 2. Derive actual encryption key from content hash via HKDF
  const cryptoKey = await deriveKey(chash);

  // 3. Zero nonce - safe because same key = same content with CHK
  const zeroNonce = new Uint8Array(IV_SIZE);

  // 4. Copy plaintext to clean ArrayBuffer for WebCrypto
  const plaintextBuffer = new ArrayBuffer(plaintext.length);
  new Uint8Array(plaintextBuffer).set(plaintext);

  // 5. Encrypt
  const ciphertext = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv: zeroNonce },
    cryptoKey,
    plaintextBuffer
  );

  return {
    ciphertext: new Uint8Array(ciphertext),
    key: chash
  };
}

/**
 * CHK decrypt: derive key from content_hash, decrypt with zero nonce
 *
 * @param ciphertext - Encrypted data (includes auth tag)
 * @param key - Content hash returned from encryptChk
 * @returns Decrypted plaintext
 * @throws Error if decryption fails (wrong key or tampered data)
 */
export async function decryptChk(ciphertext: Uint8Array, key: EncryptionKey): Promise<Uint8Array> {
  if (key.length !== 32) {
    throw new Error('CHK key must be 32 bytes');
  }
  if (ciphertext.length < MIN_CHK_ENCRYPTED_SIZE) {
    throw new Error('CHK encrypted data too short');
  }

  // Derive encryption key from content hash
  const cryptoKey = await deriveKey(key);

  // Zero nonce
  const zeroNonce = new Uint8Array(IV_SIZE);

  // Copy ciphertext to clean ArrayBuffer for WebCrypto
  const ciphertextBuffer = new ArrayBuffer(ciphertext.length);
  new Uint8Array(ciphertextBuffer).set(ciphertext);

  const plaintext = await crypto.subtle.decrypt(
    { name: 'AES-GCM', iv: zeroNonce },
    cryptoKey,
    ciphertextBuffer
  );

  return new Uint8Array(plaintext);
}

/**
 * Calculate encrypted size for CHK (no nonce prefix, just ciphertext + auth tag)
 */
export function encryptedSizeChk(plaintextSize: number): number {
  return plaintextSize + TAG_SIZE;
}

/**
 * Encrypt data using AES-256-GCM with random IV
 * @deprecated Use encryptChk for deterministic CHK encryption
 *
 * @param plaintext - Data to encrypt
 * @param key - 32-byte encryption key
 * @returns Encrypted data: [12-byte IV][ciphertext + 16-byte auth tag]
 */
export async function encrypt(plaintext: Uint8Array, key: EncryptionKey): Promise<Uint8Array> {
  if (key.length !== 32) {
    throw new Error('Encryption key must be 32 bytes');
  }

  const iv = new Uint8Array(IV_SIZE);
  crypto.getRandomValues(iv);

  const cryptoKey = await importKey(key);

  // Copy plaintext to clean ArrayBuffer for WebCrypto
  const plaintextBuffer = new ArrayBuffer(plaintext.length);
  new Uint8Array(plaintextBuffer).set(plaintext);

  const ciphertext = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv },
    cryptoKey,
    plaintextBuffer
  );

  // Prepend IV to ciphertext
  const result = new Uint8Array(IV_SIZE + ciphertext.byteLength);
  result.set(iv, 0);
  result.set(new Uint8Array(ciphertext), IV_SIZE);

  return result;
}

/**
 * Decrypt data using AES-256-GCM
 *
 * @param encrypted - Encrypted data: [12-byte IV][ciphertext + auth tag]
 * @param key - 32-byte encryption key
 * @returns Decrypted plaintext
 * @throws Error if decryption fails (wrong key or tampered data)
 */
export async function decrypt(encrypted: Uint8Array, key: EncryptionKey): Promise<Uint8Array> {
  if (key.length !== 32) {
    throw new Error('Encryption key must be 32 bytes');
  }
  if (encrypted.length < MIN_ENCRYPTED_SIZE) {
    throw new Error('Encrypted data too short');
  }

  const iv = encrypted.slice(0, IV_SIZE);
  const ciphertext = encrypted.slice(IV_SIZE);

  const cryptoKey = await importKey(key);

  const plaintext = await crypto.subtle.decrypt(
    { name: 'AES-GCM', iv },
    cryptoKey,
    ciphertext
  );

  return new Uint8Array(plaintext);
}

/**
 * Check if data could be encrypted (based on minimum size).
 * Note: This is a heuristic - actual encrypted data might be larger.
 */
export function couldBeEncrypted(data: Uint8Array): boolean {
  return data.length >= MIN_ENCRYPTED_SIZE;
}

/**
 * Calculate encrypted size for given plaintext size
 */
export function encryptedSize(plaintextSize: number): number {
  return IV_SIZE + plaintextSize + TAG_SIZE;
}

/**
 * Calculate plaintext size from encrypted size
 */
export function plaintextSize(encryptedSize: number): number {
  return Math.max(0, encryptedSize - IV_SIZE - TAG_SIZE);
}

/** Convert key to hex string */
export function keyToHex(key: EncryptionKey): string {
  return Array.from(key).map(b => b.toString(16).padStart(2, '0')).join('');
}

/** Convert hex string to key */
export function keyFromHex(hex: string): EncryptionKey {
  if (hex.length !== 64) {
    throw new Error('Key hex must be 64 characters (32 bytes)');
  }
  const key = new Uint8Array(32);
  for (let i = 0; i < 32; i++) {
    key[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return key;
}
