import { describe, it, expect } from 'vitest';
import { toHex, fromHex } from '../src/types.js';

describe('toHex', () => {
  it('should convert empty array', () => {
    expect(toHex(new Uint8Array(0))).toBe('');
  });

  it('should convert bytes to hex', () => {
    expect(toHex(new Uint8Array([0, 255, 16]))).toBe('00ff10');
  });

  it('should pad single digits', () => {
    expect(toHex(new Uint8Array([1, 2, 3]))).toBe('010203');
  });
});

describe('fromHex', () => {
  it('should convert empty string', () => {
    expect(fromHex('')).toEqual(new Uint8Array(0));
  });

  it('should convert hex to bytes', () => {
    expect(fromHex('00ff10')).toEqual(new Uint8Array([0, 255, 16]));
  });

  it('should handle uppercase', () => {
    expect(fromHex('ABCD')).toEqual(new Uint8Array([171, 205]));
  });
});

describe('roundtrip', () => {
  it('should roundtrip correctly', () => {
    const original = new Uint8Array([0, 1, 127, 128, 255]);
    const hex = toHex(original);
    const result = fromHex(hex);
    expect(result).toEqual(original);
  });
});
