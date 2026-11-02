/** Object encryption suites. Legacy keys remain readable; new keys carry a suite byte. */
import { decrypt, decryptChk, encrypt, encryptChk, generateKey } from './crypto.js';

export type EncryptionMode = 'legacy-chk' | 'chk-v1' | 'rnd-v1';

export function encryptionMode(key: Uint8Array): EncryptionMode {
  if (!(key instanceof Uint8Array)) throw new Error('Encryption key must be bytes');
  if (key.length === 32) return 'legacy-chk';
  if (key.length !== 33) throw new Error('Encryption key must be 32 or 33 bytes');
  if (key[0] === 1) return 'chk-v1';
  if (key[0] === 2) return 'rnd-v1';
  throw new Error(`Unsupported encryption suite: ${key[0]}`);
}

export function objectKeyFromHex(hex: string): Uint8Array {
  if (!/^(?:[0-9a-f]{2}){32,33}$/i.test(hex)) throw new Error('Invalid encryption key hex');
  const key = Uint8Array.from(hex.match(/../g)!, pair => Number.parseInt(pair, 16));
  encryptionMode(key);
  return key;
}

export async function encryptObject(
  plaintext: Uint8Array,
  mode: EncryptionMode = 'chk-v1'
): Promise<{ ciphertext: Uint8Array; key: Uint8Array }> {
  if (mode === 'legacy-chk') return encryptChk(plaintext);
  if (mode === 'chk-v1') {
    const result = await encryptChk(plaintext);
    return { ciphertext: result.ciphertext, key: Uint8Array.of(1, ...result.key) };
  }
  if (mode !== 'rnd-v1') throw new Error(`Unsupported encryption mode: ${mode}`);
  const rawKey = generateKey();
  return { ciphertext: await encrypt(plaintext, rawKey), key: Uint8Array.of(2, ...rawKey) };
}

export async function decryptObject(ciphertext: Uint8Array, key: Uint8Array): Promise<Uint8Array> {
  const mode = encryptionMode(key);
  if (mode === 'legacy-chk') return decryptChk(ciphertext, key);
  const rawKey = key.subarray(1);
  return mode === 'chk-v1' ? decryptChk(ciphertext, rawKey) : decrypt(ciphertext, rawKey);
}
