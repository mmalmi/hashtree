import { describe, it, expect } from 'vitest';
import { sha256 } from '../src/hash.js';
import { encryptChk, decryptChk } from '../src/crypto.js';
import { cryptoInput } from '../src/cryptoInput.js';

const bytes = (length: number) => Uint8Array.from({ length }, (_, i) => (i * 31) % 251);

describe('cryptoInput', () => {
  it('passes ordinary views through without copying', () => {
    const data = bytes(64);
    expect(cryptoInput(data)).toBe(data);
  });

  it('hashes and encrypts only the bytes of an offset view', async () => {
    const whole = bytes(96);
    const view = whole.subarray(16, 80);
    const copy = whole.slice(16, 80);

    expect(await sha256(view)).toEqual(await sha256(copy));
    const fromView = await encryptChk(view);
    const fromCopy = await encryptChk(copy);
    expect(fromView.ciphertext).toEqual(fromCopy.ciphertext);
    expect(await decryptChk(fromView.ciphertext, fromView.key)).toEqual(copy);
  });

  it('copies views over a SharedArrayBuffer, which WebCrypto rejects', async () => {
    const plain = bytes(64);
    const shared = new Uint8Array(new SharedArrayBuffer(64));
    shared.set(plain);

    const input = cryptoInput(shared) as Uint8Array;
    expect(input).not.toBe(shared);
    expect(input.buffer).toBeInstanceOf(ArrayBuffer);
    expect(await sha256(shared)).toEqual(await sha256(plain));

    const { ciphertext, key } = await encryptChk(shared);
    expect(await decryptChk(ciphertext, key)).toEqual(plain);
  });
});
