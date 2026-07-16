import type { Hash } from './types.js';

/** Native-compatible upper bound for a Hashtree blob route request. */
export const BLOB_MAX_HTL = 10;

/** Native-compatible default for an explicitly configured remote route. */
export const BLOB_DEFAULT_HTL = 10;

export interface BlobRequest {
  hash: Hash;
  htl: number;
}

export type BlobReply =
  | { type: 'data'; data: Uint8Array }
  | { type: 'no-result' };

export interface BlobRoute {
  id: string;
  groupId?: string;
  read(request: BlobRequest, signal?: AbortSignal): Promise<BlobReply>;
}

export const BLOB_NO_RESULT: BlobReply = Object.freeze({ type: 'no-result' });

export function createBlobRequest(hash: Hash, htl = BLOB_DEFAULT_HTL): BlobRequest {
  if (!Number.isInteger(htl) || htl < 0 || htl > BLOB_MAX_HTL) {
    throw new RangeError(`Blob request HTL must be an integer from 0 to ${BLOB_MAX_HTL}`);
  }
  return { hash, htl };
}

export function blobData(data: Uint8Array): BlobReply {
  return { type: 'data', data };
}

export function blobReplyFromNullable(data: Uint8Array | null | undefined): BlobReply {
  return data === null || data === undefined ? BLOB_NO_RESULT : blobData(data);
}
