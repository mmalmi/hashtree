import { sha256 } from './hash.js';
import { toHex, type Hash, type Store } from './types.js';

/** Native-compatible upper bound for routed blob data. */
export const BLOB_MAX_BYTES = 16 * 1024 * 1024;

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

/** Process-local limits supplied by an outer router to one opaque route. */
export interface BlobRouteContext {
  signal: AbortSignal;
  deadlineMs: number;
  attemptBudget: number;
}

export interface BlobRoute {
  id: string;
  groupId?: string;
  isAvailable?: () => boolean;
  read(request: BlobRequest, context?: BlobRouteContext): Promise<BlobReply>;
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

/** Copy and verify data before it crosses a blob-routing trust boundary. */
export async function verifyBlobData(
  expectedHash: Hash,
  data: Uint8Array,
  routeId: string,
): Promise<Uint8Array> {
  if (data.byteLength > BLOB_MAX_BYTES) {
    throw new Error(
      `Blob route ${routeId} returned ${data.byteLength} bytes, exceeding the ${BLOB_MAX_BYTES}-byte limit`,
    );
  }
  const stableData = data.slice();
  if (toHex(await sha256(stableData)) !== toHex(expectedHash)) {
    throw new Error(`Blob route ${routeId} returned content with the wrong hash`);
  }
  return stableData;
}

/** A terminal route that performs one lookup and does not interpret HTL. */
export class StoreBlobRoute implements BlobRoute {
  constructor(
    readonly id: string,
    private readonly store: Store,
    readonly groupId?: string,
  ) {
    if (!id) throw new Error('Blob route identity must not be empty');
  }

  async read(request: BlobRequest): Promise<BlobReply> {
    const data = await this.store.get(request.hash);
    return data === null
      ? BLOB_NO_RESULT
      : blobData(await verifyBlobData(request.hash, data, this.id));
  }
}
