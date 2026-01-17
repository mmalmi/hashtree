/**
 * Tests for visibility encryption utilities
 */
import { describe, it, expect } from 'vitest';
import {
  generateLinkKey,
  computeKeyId,
  encryptKeyForLink,
  decryptKeyFromLink,
  visibilityHex,
} from '../src/index.js';

describe('visibility', () => {
  describe('generateLinkKey', () => {
    it('should generate 32-byte key', () => {
      const key = generateLinkKey();
      expect(key).toBeInstanceOf(Uint8Array);
      expect(key.length).toBe(32);
    });

    it('should generate unique keys', () => {
      const key1 = generateLinkKey();
      const key2 = generateLinkKey();
      expect(key1).not.toEqual(key2);
    });
  });

  describe('computeKeyId', () => {
    it('should return 8-byte hash of key', async () => {
      const key = generateLinkKey();
      const keyId = await computeKeyId(key);
      expect(keyId).toBeInstanceOf(Uint8Array);
      expect(keyId.length).toBe(8);
    });

    it('should be deterministic', async () => {
      const key = generateLinkKey();
      const keyId1 = await computeKeyId(key);
      const keyId2 = await computeKeyId(key);
      expect(keyId1).toEqual(keyId2);
    });
  });

  describe('encryptKeyForLink / decryptKeyFromLink', () => {
    it('should encrypt and decrypt CHK key (XOR format)', async () => {
      const chkKey = crypto.getRandomValues(new Uint8Array(32));
      const linkKey = generateLinkKey();

      const encrypted = encryptKeyForLink(chkKey, linkKey);
      expect(encrypted.length).toBe(32); // XOR preserves size

      const decrypted = await decryptKeyFromLink(encrypted, linkKey);
      expect(decrypted).not.toBeNull();
      expect(decrypted).toEqual(chkKey);
    });

    it('should produce wrong result with wrong link key', async () => {
      const chkKey = crypto.getRandomValues(new Uint8Array(32));
      const linkKey = generateLinkKey();
      const wrongKey = generateLinkKey();

      const encrypted = encryptKeyForLink(chkKey, linkKey);
      const decrypted = await decryptKeyFromLink(encrypted, wrongKey);
      // XOR with wrong key gives a result, just not the right one
      expect(decrypted).not.toBeNull();
      expect(decrypted).not.toEqual(chkKey);
    });

    it('should be deterministic (same inputs = same output)', async () => {
      const chkKey = crypto.getRandomValues(new Uint8Array(32));
      const linkKey = generateLinkKey();

      const encrypted1 = encryptKeyForLink(chkKey, linkKey);
      const encrypted2 = encryptKeyForLink(chkKey, linkKey);

      // XOR is deterministic
      expect(encrypted1).toEqual(encrypted2);

      // Both decrypt to same key
      const decrypted1 = await decryptKeyFromLink(encrypted1, linkKey);
      const decrypted2 = await decryptKeyFromLink(encrypted2, linkKey);
      expect(decrypted1).toEqual(chkKey);
      expect(decrypted2).toEqual(chkKey);
    });

    it('should return null for invalid length', async () => {
      const shortKey = new Uint8Array(16);
      const linkKey = generateLinkKey();
      expect(await decryptKeyFromLink(shortKey, linkKey)).toBeNull();
    });

    it('should decrypt legacy AES-GCM format (60 bytes)', async () => {
      const chkKey = crypto.getRandomValues(new Uint8Array(32));
      const linkKey = generateLinkKey();

      // Manually create AES-GCM encrypted data (legacy format)
      const keyBuffer = new ArrayBuffer(linkKey.length);
      new Uint8Array(keyBuffer).set(linkKey);
      const dataBuffer = new ArrayBuffer(chkKey.length);
      new Uint8Array(dataBuffer).set(chkKey);

      const cryptoKey = await crypto.subtle.importKey(
        'raw', keyBuffer, { name: 'AES-GCM' }, false, ['encrypt']
      );
      const nonce = crypto.getRandomValues(new Uint8Array(12));
      const ciphertext = await crypto.subtle.encrypt(
        { name: 'AES-GCM', iv: nonce }, cryptoKey, dataBuffer
      );

      // Legacy format: nonce + ciphertext (includes tag)
      const legacyEncrypted = new Uint8Array(12 + ciphertext.byteLength);
      legacyEncrypted.set(nonce);
      legacyEncrypted.set(new Uint8Array(ciphertext), 12);
      expect(legacyEncrypted.length).toBe(60); // 12 + 32 + 16

      // Should decrypt legacy format
      const decrypted = await decryptKeyFromLink(legacyEncrypted, linkKey);
      expect(decrypted).not.toBeNull();
      expect(decrypted).toEqual(chkKey);
    });
  });

  describe('link-visible owner recovery flow', () => {
    it('owner can derive contentKey from linkKey + encryptedKey', async () => {
      // This tests the flow for link-visible content:
      // 1. contentKey is the actual encryption key for content
      // 2. encryptedKey = XOR(contentKey, linkKey) - stored in event
      // 3. Owner stores selfEncryptedLinkKey (linkKey encrypted to self via NIP-44)
      // 4. Owner decrypts selfEncryptedLinkKey -> gets linkKey
      // 5. Owner XORs encryptedKey with linkKey -> gets contentKey

      const contentKey = crypto.getRandomValues(new Uint8Array(32));
      const linkKey = generateLinkKey();

      // Step 1: Create encryptedKey (stored in Nostr event)
      const encryptedKey = encryptKeyForLink(contentKey, linkKey);

      // Step 2: Simulate owner recovering linkKey (from selfEncryptedLinkKey decrypt)
      const recoveredLinkKey = linkKey; // In reality this comes from NIP-44 decrypt

      // Step 3: Owner derives contentKey
      const derivedContentKey = await decryptKeyFromLink(encryptedKey, recoveredLinkKey);

      expect(derivedContentKey).toEqual(contentKey);
    });

    it('hex version of owner recovery flow', async () => {
      const contentKeyHex = visibilityHex.generateLinkKey();
      const linkKeyHex = visibilityHex.generateLinkKey();

      // Create encryptedKey
      const encryptedKeyHex = visibilityHex.encryptKeyForLink(contentKeyHex, linkKeyHex);

      // Owner recovers contentKey
      const derivedContentKeyHex = await visibilityHex.decryptKeyFromLink(encryptedKeyHex, linkKeyHex);

      expect(derivedContentKeyHex).toBe(contentKeyHex);
    });

    it('XOR is symmetric - can decrypt with same operation', () => {
      const a = crypto.getRandomValues(new Uint8Array(32));
      const b = crypto.getRandomValues(new Uint8Array(32));

      // XOR(a, b) = c
      const c = encryptKeyForLink(a, b);

      // XOR(c, b) = a (decryption)
      const recovered = encryptKeyForLink(c, b);

      expect(recovered).toEqual(a);
    });
  });

  describe('hex helpers', () => {
    it('should generate hex link key', () => {
      const key = visibilityHex.generateLinkKey();
      expect(typeof key).toBe('string');
      expect(key.length).toBe(64); // 32 bytes = 64 hex chars
    });

    it('should compute hex keyId', async () => {
      const key = visibilityHex.generateLinkKey();
      const keyId = await visibilityHex.computeKeyId(key);
      expect(typeof keyId).toBe('string');
      expect(keyId.length).toBe(16); // 8 bytes = 16 hex chars
    });

    it('should encrypt and decrypt with hex', async () => {
      const chkKey = visibilityHex.generateLinkKey(); // Use as CHK key
      const linkKey = visibilityHex.generateLinkKey();

      const encrypted = visibilityHex.encryptKeyForLink(chkKey, linkKey);
      expect(typeof encrypted).toBe('string');
      expect(encrypted.length).toBe(64); // 32 bytes = 64 hex chars

      const decrypted = await visibilityHex.decryptKeyFromLink(encrypted, linkKey);
      expect(decrypted).toBe(chkKey);
    });

    it('should return null for invalid length hex', async () => {
      const shortEncrypted = 'abcd'; // Too short
      const linkKey = visibilityHex.generateLinkKey();

      const decrypted = await visibilityHex.decryptKeyFromLink(shortEncrypted, linkKey);
      expect(decrypted).toBeNull();
    });
  });
});
