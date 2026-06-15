import { describe, expect, it } from 'vitest';
import { compareNames } from '../src/compare.js';

describe('compareNames', () => {
  it('orders by UTF-8 bytes instead of UTF-16 code units', () => {
    const bmpPrivateUse = '\uE000';
    const supplementary = '\u{10000}';

    expect(compareNames(bmpPrivateUse, supplementary)).toBeLessThan(0);
    expect(bmpPrivateUse < supplementary).toBe(false);
  });
});
