import { describe, it, expect } from 'vitest';
import { fromHex, nhashDecode, toHex } from '@hashtree/core';
import { profileSearchIndexNhash } from '../scripts/profileSearchIndex';

describe('profileSearchIndexNhash', () => {
  it('returns null when no root is provided', () => {
    expect(profileSearchIndexNhash(null)).toBeNull();
  });

  it('encodes the root as a valid nhash', () => {
    const hash = fromHex('11'.repeat(32));
    const nhash = profileSearchIndexNhash({ hash });

    expect(nhash).toMatch(/^nhash1[a-z0-9]+$/);
    const decoded = nhashDecode(nhash!);
    expect(toHex(decoded.hash)).toBe(toHex(hash));
  });
});
