import { describe, it, expect } from 'vitest';
import { sha256 } from '../src/hash.js';
import { toHex } from '../src/types.js';

describe('sha256', () => {
  it('should hash empty data', async () => {
    const hash = await sha256(new Uint8Array(0));
    expect(toHex(hash)).toBe('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
  });

  it('should hash hello world', async () => {
    const data = new TextEncoder().encode('hello world');
    const hash = await sha256(data);
    expect(toHex(hash)).toBe('b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9');
  });

  it('should produce consistent hashes', async () => {
    const data = new Uint8Array([1, 2, 3, 4, 5]);
    const hash1 = await sha256(data);
    const hash2 = await sha256(data);
    expect(toHex(hash1)).toBe(toHex(hash2));
  });

  it('should produce 32-byte hashes', async () => {
    const data = new Uint8Array([1, 2, 3]);
    const hash = await sha256(data);
    expect(hash.length).toBe(32);
  });
});
