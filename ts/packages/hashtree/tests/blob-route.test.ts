import { describe, expect, it } from 'vitest';
import {
  BLOB_DEFAULT_HTL,
  BLOB_MAX_BYTES,
  BLOB_MAX_HTL,
  BLOB_NO_RESULT,
  MemoryStore,
  StoreBlobRoute,
  blobData,
  blobReplyFromNullable,
  createBlobRequest,
  sha256,
  type BlobRouteContext,
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

  it('adapts one store into a terminal route without interpreting HTL', async () => {
    const data = new Uint8Array([1, 2, 3]);
    const hash = await sha256(data) as Hash;
    const store = new MemoryStore();
    await store.put(hash, data);
    const route = new StoreBlobRoute('local', store);
    const context: BlobRouteContext = {
      signal: new AbortController().signal,
      deadlineMs: Date.now() + 1_000,
      attemptBudget: 1,
    };

    await expect(route.read(createBlobRequest(hash, 0), context)).resolves.toEqual(blobData(data));
    await expect(route.read(createBlobRequest(HASH, BLOB_MAX_HTL))).resolves.toBe(BLOB_NO_RESULT);
  });

  it('rejects corrupt and oversized store results', async () => {
    const data = new Uint8Array([4, 5, 6]);
    const hash = await sha256(data) as Hash;
    const corrupt = new StoreBlobRoute('corrupt', {
      put: async () => false,
      get: async () => new Uint8Array([9]),
      has: async () => true,
      delete: async () => false,
    });
    const oversized = new StoreBlobRoute('oversized', {
      put: async () => false,
      get: async () => new Uint8Array(BLOB_MAX_BYTES + 1),
      has: async () => true,
      delete: async () => false,
    });

    await expect(corrupt.read(createBlobRequest(hash))).rejects.toThrow(/wrong hash|mismatched/i);
    await expect(oversized.read(createBlobRequest(hash))).rejects.toThrow(/exceed/i);
  });
});
