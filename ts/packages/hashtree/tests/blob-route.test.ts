import { describe, expect, it } from 'vitest';
import {
  BLOB_DEFAULT_HTL,
  BLOB_MAX_HTL,
  BLOB_NO_RESULT,
  blobReplyFromNullable,
  createBlobRequest,
  type Hash,
} from '../src/index.js';

const HASH = new Uint8Array(32) as Hash;

describe('blob route contract', () => {
  it('defaults remote requests to the native maximum HTL', () => {
    expect(BLOB_DEFAULT_HTL).toBe(10);
    expect(createBlobRequest(HASH)).toEqual({ hash: HASH, htl: BLOB_DEFAULT_HTL });
  });

  it('rejects HTL values outside the native range', () => {
    expect(() => createBlobRequest(HASH, -1)).toThrow(RangeError);
    expect(() => createBlobRequest(HASH, BLOB_MAX_HTL + 1)).toThrow(RangeError);
  });

  it('keeps an explicit route-local miss distinct from empty data', () => {
    expect(blobReplyFromNullable(null)).toBe(BLOB_NO_RESULT);
    expect(blobReplyFromNullable(new Uint8Array(0))).toEqual({
      type: 'data',
      data: new Uint8Array(0),
    });
  });
});
